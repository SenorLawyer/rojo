//! Backups of Studio instances kept outside the place, for
//! `rojo studio backup`.
//!
//! Agents used to copy a map or model into ServerStorage before rebuilding it,
//! and those copies stayed in the place, got published and had to be cleaned
//! up by hand. A backup here is a native `.rbxm` export saved in the
//! repository's shared state directory with a small JSON record next to it,
//! so it survives switching worktrees and never ends up in the place.

use std::{
    collections::HashSet,
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
    /// Taken by Rojo itself before it overwrote or removed instances, such as
    /// before `verify --fix`. Only these are pruned; a backup someone made on
    /// purpose stays until they remove it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub automatic: bool,
}

/// How many automatic backups are kept. Each is at most 5 MiB, and every
/// reconnect that changes Studio takes one, so without a bound they would
/// fill the disk.
pub const KEEP_AUTOMATIC: usize = 20;

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

    /// A store in `dir`, for tests that must not touch the per-user folder.
    pub(crate) fn in_dir(dir: &Path) -> Store {
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

    /// Saves `model` as an automatic backup. It does not prune: an operation
    /// that saves several prunes once afterwards with [`Self::prune_automatic`],
    /// so it cannot remove its own earlier backups.
    pub fn save_automatic(&self, mut record: Record, model: &[u8]) -> anyhow::Result<Record> {
        record.automatic = true;
        self.save(record, model)
    }

    /// Removes the oldest automatic backups, keeping the `keep` newest, and
    /// returns their IDs. Backups in `protected`, those the current operation
    /// just saved, are never removed and count towards `keep`, so the
    /// operation's own backups all stay even when there are more of them.
    /// Backups made on purpose are never removed.
    pub fn prune_automatic(
        &self,
        keep: usize,
        protected: &HashSet<String>,
    ) -> anyhow::Result<Vec<String>> {
        let _lock = self.lock()?;
        let automatic: Vec<Record> = self
            .list()?
            .into_iter()
            .filter(|record| record.automatic)
            .collect();
        let kept_protected = automatic
            .iter()
            .filter(|record| protected.contains(&record.id))
            .count();
        let removed: Vec<String> = automatic
            .into_iter()
            .filter(|record| !protected.contains(&record.id))
            .skip(keep.saturating_sub(kept_protected))
            .map(|record| record.id)
            .collect();
        for id in &removed {
            self.remove_locked(id)?;
        }
        Ok(removed)
    }

    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        self.remove_locked(id)
    }

    fn remove_locked(&self, id: &str) -> anyhow::Result<()> {
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
const MAX_NOTE_BYTES: usize = 4096;

/// `text` cut to at most `max` bytes on a character boundary.
pub(crate) fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// The record of an automatic backup holding `contents`, the path and class
/// of each instance in its model. The note starts with `reason` and lists as
/// many of the paths as fit, so `backup list` says what is in it.
pub(crate) fn automatic_record(
    name: &str,
    reason: &str,
    contents: &[(String, String)],
    place_id: Option<u64>,
    place_name: Option<&str>,
) -> Record {
    let instance = match contents {
        [] => String::new(),
        [(path, _)] => path.clone(),
        [(path, _), rest @ ..] => format!("{path} and {} more", rest.len()),
    };
    let class_name = match contents.split_first() {
        Some(((_, first), rest)) if rest.iter().all(|(_, class)| class == first) => first.clone(),
        _ => "Instances".to_owned(),
    };
    let mut note = format!("{reason}; {} instances:", contents.len());
    for (index, (path, _)) in contents.iter().enumerate() {
        let more = format!(" (and {} more)", contents.len() - index);
        if note.len() + 1 + path.len() + more.len() > MAX_NOTE_BYTES {
            note.push_str(&more);
            break;
        }
        note.push(' ');
        note.push_str(path);
    }
    Record {
        id: String::new(),
        name: clip(name, 64).to_owned(),
        instance,
        class_name,
        place_id,
        place_name: place_name.map(|name| clip(name, 256).to_owned()),
        git_head: None,
        git_branch: None,
        created_at: now_millis(),
        bytes: 0,
        note: Some(clip(&note, MAX_NOTE_BYTES).to_owned()),
        automatic: true,
    }
}

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
        let automatic = if record.automatic { " [automatic]" } else { "" };
        println!(
            "{}  {} ({}, {size}) from {place}{commit}{automatic}",
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
            automatic: false,
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
    fn pruning_keeps_the_newest_automatic_backups_and_every_manual_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let manual_old = store.save(record("Map", 500), b"manual").unwrap();
        let mut automatic = Vec::new();
        for index in 0..5 {
            let saved = store
                .save_automatic(record("auto", 1_000 + index * 1_000), b"auto")
                .unwrap();
            assert!(saved.automatic);
            automatic.push(saved.id);
        }
        let manual_new = store.save(record("Lobby", 100_000), b"manual").unwrap();
        let removed = store.prune_automatic(2, &HashSet::new()).unwrap();
        assert_eq!(
            removed,
            vec![
                automatic[2].clone(),
                automatic[1].clone(),
                automatic[0].clone()
            ]
        );
        let left: Vec<String> = store.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(
            left,
            vec![
                manual_new.id,
                automatic[4].clone(),
                automatic[3].clone(),
                manual_old.id
            ],
            "manual backups stay however old they are"
        );
        for id in &removed {
            assert!(!store.model_path(id).exists() && !store.record_path(id).exists());
        }
    }

    #[test]
    fn pruning_beyond_the_limit_keeps_manual_backups() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let manual = store.save(record("Map", 1), b"manual").unwrap();
        for index in 0..KEEP_AUTOMATIC as u64 + 3 {
            store
                .save_automatic(record("auto", 1_000 + index * 1_000), b"auto")
                .unwrap();
        }
        // Saving alone never prunes.
        assert_eq!(store.list().unwrap().len(), KEEP_AUTOMATIC + 4);
        assert_eq!(
            store
                .prune_automatic(KEEP_AUTOMATIC, &HashSet::new())
                .unwrap()
                .len(),
            3
        );
        let listed = store.list().unwrap();
        assert_eq!(
            listed.iter().filter(|r| r.automatic).count(),
            KEEP_AUTOMATIC
        );
        assert!(listed.iter().any(|r| r.id == manual.id));
        assert_eq!(listed.len(), KEEP_AUTOMATIC + 1);
    }

    #[test]
    fn an_operation_with_more_backups_than_the_limit_keeps_all_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let older: Vec<String> = (0..5)
            .map(|index| {
                store
                    .save_automatic(record("older", 1_000 + index * 1_000), b"old")
                    .unwrap()
                    .id
            })
            .collect();
        let own: HashSet<String> = (0..KEEP_AUTOMATIC as u64 + 5)
            .map(|index| {
                store
                    .save_automatic(record("own", 100_000 + index * 1_000), b"own")
                    .unwrap()
                    .id
            })
            .collect();
        let removed = store.prune_automatic(KEEP_AUTOMATIC, &own).unwrap();
        assert_eq!(removed.len(), older.len(), "every older one goes first");
        let left: HashSet<String> = store.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(left, own);

        // With room left, the newest unprotected ones stay beside the
        // protected ones.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let old: Vec<String> = (0..4)
            .map(|index| {
                store
                    .save_automatic(record("old", 1_000 + index * 1_000), b"old")
                    .unwrap()
                    .id
            })
            .collect();
        // Protected, though older than the rest, as a slow operation's
        // first backup would be.
        let protected: HashSet<String> = [old[0].clone()].into();
        let removed = store.prune_automatic(2, &protected).unwrap();
        assert_eq!(removed, vec![old[2].clone(), old[1].clone()]);
    }

    #[test]
    fn records_from_before_automatic_backups_read_as_manual() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let saved = store.save(record("Map", 1_000), b"model").unwrap();
        let text = fs::read_to_string(store.record_path(&saved.id)).unwrap();
        assert!(
            !text.contains("automatic"),
            "manual records keep the old format"
        );
        assert!(!store.get(&saved.id).unwrap().0.automatic);
        assert_eq!(
            store.prune_automatic(0, &HashSet::new()).unwrap(),
            Vec::<String>::new()
        );
        let auto = store
            .save_automatic(record("auto", 2_000), b"model")
            .unwrap();
        let text = fs::read_to_string(store.record_path(&auto.id)).unwrap();
        assert!(text.contains("\"automatic\": true"));
    }

    #[test]
    fn automatic_records_describe_their_contents_within_the_limits() {
        let one = automatic_record(
            "auto-verify-fix",
            "auto: before verify --fix",
            &[(
                "game/ReplicatedStorage/A".to_owned(),
                "ModuleScript".to_owned(),
            )],
            Some(7),
            Some("Place"),
        );
        assert!(one.automatic);
        assert_eq!(one.instance, "game/ReplicatedStorage/A");
        assert_eq!(one.class_name, "ModuleScript");
        assert_eq!(
            one.note.as_deref(),
            Some("auto: before verify --fix; 1 instances: game/ReplicatedStorage/A")
        );
        let many: Vec<(String, String)> = (0..2_000)
            .map(|index| {
                (
                    format!("game/Workspace/Part{index}"),
                    if index == 0 { "Script" } else { "Part" }.to_owned(),
                )
            })
            .collect();
        let record = automatic_record("auto-sync", "auto: before sync", &many, None, None);
        assert_eq!(record.instance, "game/Workspace/Part0 and 1999 more");
        assert_eq!(record.class_name, "Instances");
        let note = record.note.clone().unwrap();
        assert!(note.len() <= 4096 && note.ends_with("more)"), "{note}");
        // A record this large still saves, so `list` shows it.
        let dir = tempfile::tempdir().unwrap();
        assert!(Store::in_dir(dir.path()).save(record, b"model").is_ok());
        assert_eq!(clip("aé", 2), "a");
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
