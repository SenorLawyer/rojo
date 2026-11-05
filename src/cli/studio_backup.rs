//! Backups of Studio instances kept outside the place, for
//! `rojo studio backup`.
//!
//! Agents used to copy a map or model into ServerStorage before rebuilding it,
//! and those copies stayed in the place, got published and had to be cleaned
//! up by hand. A backup here is a native `.rbxm` export saved in the
//! repository's shared state directory with a small JSON record next to it,
//! so it survives switching worktrees and never ends up in the place.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

/// What `backup list` shows about a backup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub id: String,
    pub name: String,
    /// The exported instance's path, such as `game/Workspace/Map`.
    pub instance: String,
    pub class_name: String,
    pub place_id: Option<u64>,
    pub place_name: Option<String>,
    pub git_head: Option<String>,
    pub git_branch: Option<String>,
    /// Unix milliseconds.
    pub created_at: u64,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub struct Store {
    dir: PathBuf,
    /// The directory Git commands run in, for the commit a backup records.
    workdir: PathBuf,
}

/// Git's answer in `dir`, through the shared helper, which ignores Git
/// variables such as `GIT_DIR` inherited from a hook or another tool.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let value = crate::cli::lifecycle::git(dir, args).ok()?;
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Keeps the name usable in a file name on every platform.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "backup".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// UTC `YYYYMMDD-HHMMSS` for the given Unix milliseconds, so backup IDs sort
/// by time.
fn timestamp(millis: u64) -> String {
    let secs = millis / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm), to
    // avoid a date dependency for one timestamp.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

impl Store {
    /// The store for the repository the current directory is in, shared by
    /// all of its worktrees, or a per-user directory outside a repository.
    pub fn open() -> anyhow::Result<Store> {
        let cwd = std::env::current_dir().context("Could not read the current directory")?;
        Self::at_workdir(&cwd)
    }

    pub fn at_workdir(workdir: &Path) -> anyhow::Result<Store> {
        // Only "not a repository" (or no Git) falls back, like the locks: a
        // repository whose common directory cannot be read is an error.
        let in_repository = git(workdir, &["rev-parse", "--git-dir"]).is_some();
        // As for the locks: Git failing inside a repository must not send the
        // backups somewhere the other worktrees and processes do not look.
        anyhow::ensure!(
            in_repository || !crate::cli::has_git_marker(workdir),
            "{} is in a Git repository, but Git could not read it. Fix Git and try again.",
            workdir.display()
        );
        let dir = if in_repository {
            let common = crate::cli::lifecycle::git_common_dir(workdir)?;
            // Git prints forward slashes on Windows; rebuilding the path from
            // its components gives one separator style in printed paths.
            let common: PathBuf = common.components().collect();
            crate::cli::repository_state_dir(&common)?.join("backups")
        } else {
            // Never inside the project: a folder that `rojo serve` maps would
            // sync the backups into the place.
            super::super::project_state_dir(workdir)?.join("backups")
        };
        Ok(Store {
            dir,
            workdir: workdir.to_owned(),
        })
    }

    #[cfg(test)]
    fn in_dir(dir: &Path) -> Store {
        Store {
            dir: dir.to_owned(),
            workdir: dir.to_owned(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn model_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.rbxm"))
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// An operating system lock on the store, held while a backup is saved,
    /// orphans are found or one is removed, so cleanup cannot mistake a
    /// backup still being written for one left by an interrupted save. The
    /// system releases it if the process dies.
    fn lock(&self) -> anyhow::Result<fs::File> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("Could not create {}", self.dir.display()))?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(".store.lock"))
            .context("Could not open the backup store lock")?;
        file.lock().context("Could not lock the backup store")?;
        Ok(file)
    }

    /// Saves `model` and its record. Never overwrites an existing backup.
    pub fn save(&self, mut record: Record, model: &[u8]) -> anyhow::Result<Record> {
        let _lock = self.lock()?;
        // `list` skips records over this size, so a larger one would vanish.
        anyhow::ensure!(
            record.name.len() <= 64 && record.note.as_ref().is_none_or(|note| note.len() <= 4096),
            "A backup name is at most 64 bytes and a note at most 4096"
        );
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("Could not create {}", self.dir.display()))?;
        record.git_head = git(&self.workdir, &["rev-parse", "HEAD"]);
        record.git_branch = git(
            &self.workdir,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        );
        record.bytes = model.len() as u64;
        let base = format!(
            "{}-{}",
            timestamp(record.created_at),
            sanitize(&record.name)
        );
        // Two backups with the same name in the same second get a suffix.
        let mut id = base.clone();
        let mut attempt = 1;
        while self.model_path(&id).exists() || self.record_path(&id).exists() {
            attempt += 1;
            id = format!("{base}-{attempt}");
        }
        record.id = id.clone();
        anyhow::ensure!(
            serde_json::to_vec_pretty(&record)?.len() as u64 <= MAX_RECORD_BYTES,
            "The backup record would be larger than {} KiB",
            MAX_RECORD_BYTES / 1024
        );
        let mut model_file = tempfile::NamedTempFile::new_in(&self.dir)?;
        std::io::Write::write_all(&mut model_file, model)?;
        model_file
            .persist_noclobber(self.model_path(&id))
            .map_err(|error| error.error)
            .context("Could not save the backup model")?;
        // The record is written last, so a backup that `list` shows always
        // has its model.
        let written = (|| -> anyhow::Result<()> {
            let mut record_file = tempfile::NamedTempFile::new_in(&self.dir)?;
            std::io::Write::write_all(
                &mut record_file,
                serde_json::to_string_pretty(&record)?.as_bytes(),
            )?;
            record_file
                .persist_noclobber(self.record_path(&id))
                .map_err(|error| error.error)?;
            Ok(())
        })();
        if let Err(error) = written {
            // A model without its record could not be listed or restored.
            let _ = fs::remove_file(self.model_path(&id));
            return Err(error.context("Could not save the backup record"));
        }
        Ok(record)
    }

    pub fn list(&self) -> anyhow::Result<Vec<Record>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("Could not read the backup folder"),
        };
        let mut records = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Records are a few hundred bytes; a large file is not one.
            let small = entry_len(&path).is_some_and(|len| len <= MAX_RECORD_BYTES);
            let record: Record = match small
                .then(|| fs::read(&path).ok())
                .flatten()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            {
                Some(record) => record,
                None => {
                    log::warn!("Skipping unreadable backup record {}", path.display());
                    continue;
                }
            };
            if self.model_path(&record.id).is_file() {
                records.push(record);
            }
        }
        records.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        Ok(records)
    }

    pub fn get(&self, id: &str) -> anyhow::Result<(Record, PathBuf)> {
        if !valid_id(id) {
            bail!("Unknown backup '{id}'. Run `rojo studio backup list` for backup IDs.");
        }
        let small = entry_len(&self.record_path(id)).is_some_and(|len| len <= MAX_RECORD_BYTES);
        let record: Record = small
            .then(|| fs::read(self.record_path(id)).ok())
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .with_context(|| {
                format!("Unknown backup '{id}'. Run `rojo studio backup list` for backup IDs.")
            })?;
        let model = self.model_path(id);
        anyhow::ensure!(model.is_file(), "Backup '{id}' has no model file");
        Ok((record, model))
    }

    /// Models whose record is missing, left by a process that stopped between
    /// writing the two. `remove` deletes them by ID.
    pub fn orphans(&self) -> anyhow::Result<Vec<String>> {
        if !self.dir.is_dir() {
            return Ok(Vec::new());
        }
        let _lock = self.lock()?;
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("Could not read the backup folder"),
        };
        let mut orphans = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rbxm") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if valid_id(id) && !self.record_path(id).exists() {
                orphans.push(id.to_owned());
            }
        }
        orphans.sort();
        Ok(orphans)
    }

    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        if valid_id(id) && !self.record_path(id).exists() && self.model_path(id).is_file() {
            fs::remove_file(self.model_path(id))?;
            return Ok(());
        }
        anyhow::ensure!(
            valid_id(id) && self.record_path(id).exists() && self.model_path(id).is_file(),
            "Unknown backup '{id}'. Run `rojo studio backup list` for backup IDs."
        );
        let model = self.model_path(id);
        // The record goes first, so a half-removed backup is not listed.
        fs::remove_file(self.record_path(id))?;
        fs::remove_file(model)?;
        Ok(())
    }
}

const MAX_RECORD_BYTES: u64 = 64 * 1024;

fn entry_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn print_list(store: &Store, records: &[Record]) {
    if records.is_empty() {
        println!("No backups in {}", store.dir().display());
        return;
    }
    for record in records {
        let size = if record.bytes >= 1024 * 1024 {
            format!("{:.1} MB", record.bytes as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.1} KB", record.bytes as f64 / 1024.0)
        };
        let place = super::escape_controls(record.place_name.as_deref().unwrap_or("unknown place"));
        let commit = record.git_head.as_deref().map_or_else(String::new, |head| {
            format!(" @ {}", &head[..head.len().min(8)])
        });
        println!(
            "{}  {} ({}, {size}) from {place}{commit}",
            record.id,
            super::escape_controls(&record.instance),
            super::escape_controls(&record.class_name)
        );
        if let Some(note) = &record.note {
            println!("    {}", super::escape_controls(note));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, created_at: u64) -> Record {
        Record {
            id: String::new(),
            name: name.to_owned(),
            instance: format!("game/Workspace/{name}"),
            class_name: "Model".to_owned(),
            place_id: Some(1),
            place_name: Some("Place".to_owned()),
            git_head: None,
            git_branch: None,
            created_at,
            bytes: 0,
            note: None,
        }
    }

    #[test]
    fn timestamps_are_utc_and_sortable() {
        assert_eq!(timestamp(0), "19700101-000000");
        // 2026-11-05T03:04:05Z
        assert_eq!(timestamp(1_793_847_845_000), "20261105-030405");
        assert_eq!(timestamp(951_782_400_000), "20000229-000000");
    }

    #[test]
    fn names_are_made_safe_for_file_names() {
        assert_eq!(sanitize("Map"), "Map");
        assert_eq!(sanitize("../../etc"), "etc");
        assert_eq!(sanitize("Lobby Area/Part 2"), "Lobby-Area-Part-2");
        assert_eq!(sanitize("///"), "backup");
    }

    #[test]
    fn saves_lists_gets_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(&dir.path().join("backups"));
        assert!(store.list().unwrap().is_empty());
        let first = store.save(record("Map", 1_000), b"first").unwrap();
        let second = store.save(record("Map", 1_000), b"second").unwrap();
        assert_ne!(first.id, second.id, "same name and second get distinct IDs");
        let newer = store.save(record("Lobby", 5_000), b"third").unwrap();
        assert_eq!(newer.bytes, 5);
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].id, newer.id, "newest first");
        let (got, model) = store.get(&first.id).unwrap();
        assert_eq!(got, first);
        assert_eq!(fs::read(model).unwrap(), b"first");
        store.remove(&first.id).unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
        assert!(store.get(&first.id).is_err());
    }

    #[test]
    fn ids_cannot_escape_the_backup_folder() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        assert!(store.get("../secret").is_err());
        assert!(store.get("a/b").is_err());
        assert!(store.remove("..").is_err());
    }

    #[test]
    fn records_that_list_would_skip_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let mut big = record("Map", 1_000);
        big.note = Some("x".repeat(5000));
        assert!(store.save(big, b"model").is_err());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn models_without_records_are_reported_and_removable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let saved = store.save(record("Map", 1_000), b"model").unwrap();
        fs::remove_file(store.record_path(&saved.id)).unwrap();
        assert_eq!(store.orphans().unwrap(), vec![saved.id.clone()]);
        store.remove(&saved.id).unwrap();
        assert!(store.orphans().unwrap().is_empty());
    }

    #[test]
    fn records_without_models_are_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let saved = store.save(record("Map", 1_000), b"model").unwrap();
        fs::remove_file(store.model_path(&saved.id)).unwrap();
        assert!(store.list().unwrap().is_empty());
    }
}
