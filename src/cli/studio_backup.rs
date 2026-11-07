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
    /// Taken by Rojo itself before it overwrote or removed instances, such as
    /// before `verify --fix`. Only these are pruned; a backup someone made on
    /// purpose stays until they remove it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub automatic: bool,
    /// Every root of the model, in the order it holds them, with the path
    /// each had in the place. Backups of many instances, such as hundreds of
    /// modules with the same name, need this to tell where each came from.
    /// Records from before it was added have none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<Root>,
}

/// One root of a backup's model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Root {
    pub path: String,
    pub class_name: String,
}

/// How many automatic backups are kept. Each is at most 5 MiB, and every
/// reconnect that changes Studio takes one, so without a bound they would
/// fill the disk.
pub const KEEP_AUTOMATIC: usize = 20;

/// An automatic backup younger than this is never pruned, whoever prunes.
/// The store is shared by every worktree and server of the repository, so
/// another process may prune while an operation that saved several backups,
/// such as a `verify --fix`, still relies on them. This is far longer than
/// any such operation runs (20 seconds for a fix, 30 for a sync's request).
pub const AUTOMATIC_GRACE_MILLIS: u64 = 60 * 60 * 1000;

/// The most automatic backups kept, however young, so a burst of them (many
/// reconnects within the grace period) cannot fill the disk. The oldest go
/// first.
pub const MAX_AUTOMATIC: usize = 100;
/// The most model bytes all automatic backups may use together, however
/// young. The newest [`KEEP_AUTOMATIC`] at 5 MiB each stay well under it.
pub const MAX_AUTOMATIC_BYTES: u64 = 512 * 1024 * 1024;

/// How [`Store::prune_automatic_with`] decides what to remove.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PruneLimits {
    /// Kept whatever their age.
    pub keep: usize,
    /// Kept beyond `keep` while younger than this.
    pub grace_millis: u64,
    /// Never more than this many, nor more than `max_bytes` of models.
    pub max_count: usize,
    pub max_bytes: u64,
}

impl PruneLimits {
    pub(crate) const DEFAULT: PruneLimits = PruneLimits {
        keep: KEEP_AUTOMATIC,
        grace_millis: AUTOMATIC_GRACE_MILLIS,
        max_count: MAX_AUTOMATIC,
        max_bytes: MAX_AUTOMATIC_BYTES,
    };
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

    /// The store `rojo serve` uses for the project at `project`, a project
    /// file or the folder holding it, which is where its automatic backups
    /// go. `backup --project` opens this one.
    pub fn for_project(project: &Path) -> anyhow::Result<Store> {
        let folder = if project.is_file() {
            project
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
        } else {
            project
        };
        anyhow::ensure!(
            folder.is_dir(),
            "{} is not a project file or a folder",
            project.display()
        );
        Self::at_workdir(folder)
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
    pub fn save(&self, record: Record, model: &[u8]) -> anyhow::Result<Record> {
        let _lock = self.lock()?;
        self.save_locked(record, model)
    }

    fn save_locked(&self, mut record: Record, model: &[u8]) -> anyhow::Result<Record> {
        // `list` skips records over this size, so a larger one would vanish.
        anyhow::ensure!(
            record.name.len() <= 64
                && record
                    .note
                    .as_ref()
                    .is_none_or(|note| note.len() <= MAX_NOTE_BYTES),
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
        let mut name = sanitize(&record.name);
        // IDs of automatic backups start `<time>-auto-`, so a backup made on
        // purpose never gets one, whatever it is called.
        if !record.automatic && (name == "auto" || name.starts_with("auto-")) {
            name = format!("manual-{name}");
        }
        let mut base = format!("{}-{name}", timestamp(record.created_at));
        // An operation tells its backups apart by ID, so an automatic one
        // gets a random part: otherwise one saved in the same second as a
        // backup that was just removed would take its ID.
        if record.automatic {
            base.push('-');
            base.push_str(&uuid::Uuid::new_v4().simple().to_string()[..8]);
        }
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
        if record.automatic {
            fs::write(self.claim_path(&id), b"")
                .context("Could not mark the backup as automatic")?;
        }
        let saved_model = (|| -> anyhow::Result<()> {
            let mut model_file = tempfile::NamedTempFile::new_in(&self.dir)?;
            std::io::Write::write_all(&mut model_file, model)?;
            model_file
                .persist_noclobber(self.model_path(&id))
                .map_err(|error| error.error)?;
            Ok(())
        })();
        if let Err(error) = saved_model {
            let _ = fs::remove_file(self.claim_path(&id));
            return Err(error.context("Could not save the backup model"));
        }
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
            if fs::remove_file(self.model_path(&id)).is_ok() {
                let _ = fs::remove_file(self.claim_path(&id));
            }
            return Err(error.context("Could not save the backup record"));
        }
        // The record now says it is automatic.
        let _ = fs::remove_file(self.claim_path(&id));
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

    /// A file written before an automatic backup's model and removed once
    /// its record is written, so a model left without a record can be told
    /// to be automatic, and removed, from more than its name.
    fn claim_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.auto"))
    }

    fn pin_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.pin"))
    }

    /// Whether `id` has a pin that has not expired by `now`.
    fn pinned(&self, id: &str, now: u64) -> bool {
        fs::read_to_string(self.pin_path(id))
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .is_some_and(|until| until > now)
    }

    /// Saves `model` as an automatic backup, then prunes old automatic
    /// backups (see [`Self::prune_automatic`]), so every save keeps the store
    /// bounded, including in operations that fail or are cancelled part way.
    /// A failure to prune is only logged, because the backup is saved.
    pub fn save_automatic(&self, record: Record, model: &[u8]) -> anyhow::Result<Record> {
        self.save_automatic_with(record, model, None, &[], PruneLimits::DEFAULT, now_millis())
    }

    /// [`Self::save_automatic`], and pins the backup until `until` (Unix
    /// milliseconds), so no prune removes it while the operation that saved
    /// it still relies on it. The operation removes the pin with
    /// [`Self::unpin`] when it ends; the expiry covers a crash. `own` are the
    /// backups the same operation pinned before, which do not count against
    /// it when deciding whether pinned backups fill the limits: one operation
    /// may go over them with its own pins, but not because of others'.
    pub fn save_automatic_pinned(
        &self,
        record: Record,
        model: &[u8],
        until: u64,
        own: &[String],
    ) -> anyhow::Result<Record> {
        self.save_automatic_with(
            record,
            model,
            Some(until),
            own,
            PruneLimits::DEFAULT,
            now_millis(),
        )
    }

    /// [`Self::save_automatic`] with other limits, a pin and `now`. Refuses
    /// the backup, saving nothing, when backups pinned by other operations
    /// already fill the limits, since none of them can be removed to make
    /// room.
    pub(crate) fn save_automatic_with(
        &self,
        mut record: Record,
        model: &[u8],
        pin: Option<u64>,
        own: &[String],
        limits: PruneLimits,
        now: u64,
    ) -> anyhow::Result<Record> {
        record.automatic = true;
        let _lock = self.lock()?;
        let (pinned_count, pinned_bytes) = self
            .list()?
            .into_iter()
            .filter(|record| {
                record.automatic && !own.contains(&record.id) && self.pinned(&record.id, now)
            })
            .fold((0usize, 0u64), |(count, bytes), record| {
                (count + 1, bytes.saturating_add(record.bytes))
            });
        anyhow::ensure!(
            pinned_count < limits.max_count
                && pinned_bytes.saturating_add(model.len() as u64) <= limits.max_bytes,
            "The backup folder {} already holds {pinned_count} automatic backups ({} MiB) that running \
             operations still rely on, which fill its limit of {} backups and {} MiB. \
             Wait for them to finish and try again.",
            self.dir.display(),
            pinned_bytes / (1024 * 1024),
            limits.max_count,
            limits.max_bytes / (1024 * 1024)
        );
        // Room is made before anything is written: the prune counts the new
        // backup as kept, and when what it must remove cannot be removed,
        // such as when Windows will not let an old model be deleted, the
        // backup is refused, so the folder never grows past its limits.
        self.prune_locked(limits, now, Some(model.len() as u64))
            .context("The backup was not saved, because old automatic backups could not be removed to make room")?;
        let record = self.save_locked(record, model)?;
        if let Some(until) = pin {
            if let Err(error) = self.write_pin(&record.id, until) {
                // An unpinned backup could be pruned before it is needed.
                return Err(match self.remove_locked(&record.id) {
                    Ok(()) => error.context("Could not pin the backup, so it was not kept"),
                    // Still listed and counted, so the next prune removes it.
                    Err(removal) => error.context(format!(
                        "Could not pin the backup, nor remove it again ({removal:#}); \
                         it stays until a later prune removes it"
                    )),
                });
            }
        }
        Ok(record)
    }

    fn write_pin(&self, id: &str, until: u64) -> anyhow::Result<()> {
        #[cfg(test)]
        if FAILING_PINS.with(std::cell::Cell::get) {
            anyhow::bail!("Simulated failure to write a pin");
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        std::io::Write::write_all(&mut file, until.to_string().as_bytes())?;
        file.persist(self.pin_path(id))
            .map_err(|error| error.error)?;
        Ok(())
    }

    /// Removes the pins of `ids`, then prunes, so an operation that went over
    /// the limits with its own pins does not leave the folder over them.
    /// Other operations' unexpired pins still protect theirs.
    pub fn unpin(&self, ids: &[String]) -> anyhow::Result<()> {
        self.unpin_with(ids, PruneLimits::DEFAULT, now_millis())
    }

    pub(crate) fn unpin_with(
        &self,
        ids: &[String],
        limits: PruneLimits,
        now: u64,
    ) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        for id in ids {
            if valid_id(id) {
                match fs::remove_file(self.pin_path(id)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("Could not remove a backup pin"),
                }
            }
        }
        self.prune_locked(limits, now, None)?;
        Ok(())
    }

    /// Whether the backup `id` is still in the store.
    pub fn exists(&self, id: &str) -> bool {
        valid_id(id) && self.record_path(id).is_file() && self.model_path(id).is_file()
    }

    /// Removes automatic backups and returns their IDs: beyond the
    /// [`KEEP_AUTOMATIC`] newest, those older than [`AUTOMATIC_GRACE_MILLIS`],
    /// and beyond [`MAX_AUTOMATIC`] backups or [`MAX_AUTOMATIC_BYTES`] of
    /// models, the oldest whatever their age. A backup with an unexpired pin
    /// is never removed, even over those limits. Backups made on purpose are
    /// never removed.
    #[cfg(test)]
    pub fn prune_automatic(&self) -> anyhow::Result<Vec<String>> {
        self.prune_automatic_with(PruneLimits::DEFAULT, now_millis())
    }

    /// [`Self::prune_automatic`] with other limits, taking `now` as the
    /// current time in Unix milliseconds.
    #[cfg(test)]
    pub(crate) fn prune_automatic_with(
        &self,
        limits: PruneLimits,
        now: u64,
    ) -> anyhow::Result<Vec<String>> {
        let _lock = self.lock()?;
        self.prune_locked(limits, now, None)
    }

    /// Prunes, never removing a pinned backup, nor taking the room of one
    /// about to be saved. Those are kept first and count towards every limit;
    /// the rest, newest first by `created_at` then ID, fill what is left. So
    /// a backup about to be saved fits even when others are dated in the
    /// future, as after the clock was set back. Leftovers of failed removals
    /// count too until they go, and any removal that fails is an error.
    fn prune_locked(
        &self,
        limits: PruneLimits,
        now: u64,
        incoming: Option<u64>,
    ) -> anyhow::Result<Vec<String>> {
        let mut removed = Vec::new();
        let mut failures = Vec::new();
        // A backup about to be saved, of `incoming` bytes, is counted as kept
        // and newest, so room is made for it.
        let mut count = usize::from(incoming.is_some());
        let mut bytes: u64 = incoming.unwrap_or(0);
        // What removals that failed part way left behind is removed again
        // first. Until a leftover model goes, it still takes up its room.
        for (id, model_bytes) in self.leftovers_locked()? {
            match self.remove_locked(&id) {
                Ok(()) => removed.push(id),
                Err(error) => {
                    failures.push(format!("{id}: {error:#}"));
                    if let Some(len) = model_bytes {
                        count += 1;
                        bytes = bytes.saturating_add(len);
                    }
                }
            }
        }
        let (kept, others): (Vec<Record>, Vec<Record>) = self
            .list()?
            .into_iter()
            .filter(|record| record.automatic)
            .partition(|record| self.pinned(&record.id, now));
        count += kept.len();
        bytes = kept
            .iter()
            .fold(bytes, |total, record| total.saturating_add(record.bytes));
        let doomed: Vec<Record> = others
            .into_iter()
            .filter(|record| {
                let old = record.created_at.saturating_add(limits.grace_millis) <= now;
                let fits = count < limits.max_count
                    && bytes.saturating_add(record.bytes) <= limits.max_bytes
                    && (count < limits.keep || !old);
                if fits {
                    count += 1;
                    bytes = bytes.saturating_add(record.bytes);
                }
                !fits
            })
            .collect();
        for record in doomed {
            match self.remove_locked(&record.id) {
                Ok(()) => removed.push(record.id),
                Err(error) => failures.push(format!("{}: {error:#}", record.id)),
            }
        }
        anyhow::ensure!(
            failures.is_empty(),
            "Could not remove {} old automatic backups ({})",
            failures.len(),
            failures.join("; ")
        );
        Ok(removed)
    }

    /// Parts of automatic backups that a failed removal or an interrupted
    /// save left behind: records whose model is gone, and models whose record
    /// is gone but whose claim file says they are automatic, with the size of
    /// each model. A model without a record or a claim may be a backup made
    /// on purpose, so it is never removed here; `backup list` shows it.
    fn leftovers_locked(&self) -> anyhow::Result<Vec<(String, Option<u64>)>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("Could not read the backup folder"),
        };
        let mut leftovers = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !valid_id(id) {
                continue;
            }
            match path.extension().and_then(|e| e.to_str()) {
                Some("json") if !self.model_path(id).exists() => {
                    let automatic = entry_len(&path).is_some_and(|len| len <= MAX_RECORD_BYTES)
                        && fs::read(&path)
                            .ok()
                            .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
                            .is_some_and(|record| record.automatic);
                    if automatic {
                        leftovers.push((id.to_owned(), None));
                    }
                }
                Some("rbxm") if !self.record_path(id).exists() && self.claim_path(id).exists() => {
                    leftovers.push((id.to_owned(), entry_len(&path)));
                }
                // A claim left by a save stopped before its model was written.
                Some("auto") if !self.model_path(id).exists() && !self.record_path(id).exists() => {
                    let _ = remove_file(&path);
                }
                _ => {}
            }
        }
        leftovers.sort();
        Ok(leftovers)
    }

    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        self.remove_locked(id)
    }

    fn remove_locked(&self, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            valid_id(id) && (self.record_path(id).exists() || self.model_path(id).is_file()),
            "Unknown backup '{id}'. Run `rojo studio backup list` for backup IDs."
        );
        // The model goes first. If it cannot be deleted, the record stays, so
        // the backup is still listed and counted and the next prune tries
        // again. A record left without its model is not listed, and pruning
        // removes it too.
        remove_file(&self.model_path(id)).context("Could not delete the backup model")?;
        remove_file(&self.record_path(id)).context("Could not delete the backup record")?;
        let _ = remove_file(&self.pin_path(id));
        let _ = remove_file(&self.claim_path(id));
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    /// Text whose deletion fails for any path containing it, for tests of
    /// failed removals.
    static FAILING_REMOVALS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Whether writing a pin fails, for tests.
    static FAILING_PINS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Deletes `path`, which may already be gone.
fn remove_file(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    {
        let text = path.to_string_lossy();
        if FAILING_REMOVALS.with(|failing| {
            failing
                .borrow()
                .iter()
                .any(|failing| text.contains(failing.as_str()))
        }) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "The process cannot access the file because it is being used by another process",
            ));
        }
    }
    match fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

const MAX_RECORD_BYTES: u64 = 4 * 1024 * 1024;
const MAX_NOTE_BYTES: usize = 4096;
/// The note of an automatic backup is a short summary; `roots` has them all.
const AUTOMATIC_NOTE_BYTES: usize = 512;

/// Whether `record` is small enough to save, which a record with very many
/// or very long root paths may not be. Root paths are never shortened, since
/// two deep instances could then be recorded with the same path.
pub(crate) fn record_fits(record: &Record) -> bool {
    serde_json::to_vec_pretty(record).is_ok_and(|json| json.len() as u64 <= MAX_RECORD_BYTES)
}

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
/// of each root of its model in order, all of which it keeps in `roots`. The
/// note starts with `reason` and names the first few, as a summary for
/// `backup list`.
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
        if note.len() + 1 + path.len() + more.len() > AUTOMATIC_NOTE_BYTES {
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
        note: Some(clip(&note, AUTOMATIC_NOTE_BYTES).to_owned()),
        automatic: true,
        roots: contents
            .iter()
            .map(|(path, class_name)| Root {
                path: path.clone(),
                class_name: class_name.clone(),
            })
            .collect(),
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
            roots: Vec::new(),
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

    /// Unix milliseconds `hours` hours before now.
    fn hours_ago(hours: u64) -> u64 {
        now_millis() - hours * 60 * 60 * 1000
    }

    /// Saves an automatic record without pruning, as a store filled by
    /// earlier operations would be.
    fn fill(store: &Store, name: &str, created_at: u64) -> String {
        let mut record = record(name, created_at);
        record.automatic = true;
        store.save(record, b"auto").unwrap().id
    }

    fn automatic_ids(store: &Store) -> Vec<String> {
        store
            .list()
            .unwrap()
            .into_iter()
            .filter(|r| r.automatic)
            .map(|r| r.id)
            .collect()
    }

    #[test]
    fn pruning_keeps_the_newest_automatic_backups_and_every_manual_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let manual_old = store.save(record("Map", 500), b"manual").unwrap();
        let automatic: Vec<String> = (0..5)
            .map(|index| fill(&store, "auto", 1_000 + index * 1_000))
            .collect();
        let manual_new = store.save(record("Lobby", 100_000), b"manual").unwrap();
        let removed = store
            .prune_automatic_with(
                PruneLimits {
                    keep: 2,
                    ..PruneLimits::DEFAULT
                },
                now_millis(),
            )
            .unwrap();
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
    fn old_automatic_backups_beyond_the_limit_are_pruned_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let manual = store.save(record("Map", 1), b"manual").unwrap();
        for index in 0..KEEP_AUTOMATIC as u64 + 3 {
            let saved = store
                .save_automatic(record("auto", hours_ago(2) + index * 1_000), b"auto")
                .unwrap();
            assert!(saved.automatic);
        }
        let listed = store.list().unwrap();
        assert_eq!(
            listed.iter().filter(|r| r.automatic).count(),
            KEEP_AUTOMATIC
        );
        assert!(listed.iter().any(|r| r.id == manual.id));
    }

    #[test]
    fn young_automatic_backups_beyond_the_limit_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let now = now_millis();
        for index in 0..KEEP_AUTOMATIC as u64 + 5 {
            store
                .save_automatic(record("auto", now - 60_000 + index * 1_000), b"auto")
                .unwrap();
        }
        assert_eq!(automatic_ids(&store).len(), KEEP_AUTOMATIC + 5);
        // Once they are old, the next prune brings them back to the limit.
        let later = now + AUTOMATIC_GRACE_MILLIS;
        assert_eq!(
            store
                .prune_automatic_with(
                    PruneLimits {
                        keep: KEEP_AUTOMATIC,
                        ..PruneLimits::DEFAULT
                    },
                    later
                )
                .unwrap()
                .len(),
            5
        );
        assert_eq!(automatic_ids(&store).len(), KEEP_AUTOMATIC);
    }

    #[test]
    fn a_burst_of_young_backups_is_capped_by_count_and_by_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let manual = store.save(record("Map", 1), b"manual").unwrap();
        let now = now_millis();
        let ids: Vec<String> = (0..12)
            .map(|index| fill(&store, "young", now - 60_000 + index * 100))
            .collect();
        let limits = PruneLimits {
            keep: 2,
            grace_millis: AUTOMATIC_GRACE_MILLIS,
            max_count: 8,
            max_bytes: u64::MAX,
        };
        // All are young, but only 8 may stay: the 4 oldest go.
        let removed = store.prune_automatic_with(limits, now).unwrap();
        assert_eq!(removed, ids[..4].iter().rev().cloned().collect::<Vec<_>>());
        assert_eq!(automatic_ids(&store).len(), 8);
        // Each model here is 4 bytes, so 20 bytes keep the newest 5.
        let removed = store
            .prune_automatic_with(
                PruneLimits {
                    max_bytes: 20,
                    ..limits
                },
                now,
            )
            .unwrap();
        assert_eq!(removed.len(), 3);
        assert_eq!(
            automatic_ids(&store),
            ids[7..].iter().rev().cloned().collect::<Vec<_>>()
        );
        assert!(
            store.get(&manual.id).is_ok(),
            "manual backups are never capped"
        );
        // The real limits are a hard cap of 100 by default.
        for index in 0..MAX_AUTOMATIC as u64 + 3 {
            fill(&store, "burst", now - 30_000 + index);
        }
        store.prune_automatic().unwrap();
        assert_eq!(automatic_ids(&store).len(), MAX_AUTOMATIC);
    }

    #[test]
    fn pinned_backups_are_never_pruned_until_their_pins_expire_or_go() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let now = now_millis();
        let limits = PruneLimits {
            keep: 1,
            grace_millis: AUTOMATIC_GRACE_MILLIS,
            max_count: 3,
            max_bytes: u64::MAX,
        };
        // Old, so without pins all but the newest would go.
        let pinned: Vec<String> = (0..2)
            .map(|index| {
                store
                    .save_automatic_with(
                        record("pinned", hours_ago(5) + index),
                        b"pin",
                        Some(now + 600_000),
                        &[],
                        limits,
                        now,
                    )
                    .unwrap()
                    .id
            })
            .collect();
        let expired = store
            .save_automatic_with(
                record("expired", hours_ago(4)),
                b"exp",
                Some(now - 1),
                &[],
                limits,
                now,
            )
            .unwrap()
            .id;
        for index in 0..4 {
            fill(&store, "burst", now - 1_000 + index);
        }
        store.prune_automatic_with(limits, now).unwrap();
        let left = automatic_ids(&store);
        for id in &pinned {
            assert!(left.contains(id), "pinned backups stay, even over the cap");
        }
        assert!(!left.contains(&expired), "an expired pin does not protect");
        assert!(!dir.path().join(format!("{expired}.pin")).exists());
        // Once unpinned, they are pruned like any other.
        store.unpin(&pinned).unwrap();
        store.prune_automatic_with(limits, now).unwrap();
        assert!(automatic_ids(&store).iter().all(|id| !pinned.contains(id)));
    }

    #[test]
    fn a_backup_just_saved_survives_others_dated_in_the_future() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let now = now_millis();
        let limits = PruneLimits {
            keep: 2,
            grace_millis: AUTOMATIC_GRACE_MILLIS,
            max_count: 5,
            max_bytes: u64::MAX,
        };
        // Saved before the clock was set back a day.
        let future: Vec<String> = (0..5)
            .map(|index| fill(&store, "future", now + 86_400_000 + index))
            .collect();
        let saved = store
            .save_automatic_with(record("now", now), b"new", None, &[], limits, now)
            .unwrap();
        let left = automatic_ids(&store);
        assert!(
            left.contains(&saved.id),
            "the backup reported as saved is kept"
        );
        assert_eq!(left.len(), 5, "the cap still holds");
        assert!(!left.contains(&future[0]), "the oldest other one made room");
        // The real cap too.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        for index in 0..MAX_AUTOMATIC as u64 {
            fill(&store, "future", now + 86_400_000 + index);
        }
        let saved = store.save_automatic(record("now", now), b"new").unwrap();
        assert!(store.exists(&saved.id));
        assert_eq!(automatic_ids(&store).len(), MAX_AUTOMATIC);
    }

    #[test]
    fn releasing_pins_brings_the_folder_back_within_the_limits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let now = now_millis();
        let limits = PruneLimits {
            keep: 1,
            grace_millis: AUTOMATIC_GRACE_MILLIS,
            max_count: 3,
            max_bytes: u64::MAX,
        };
        // Another operation's pin, which still protects its backup.
        let theirs = store
            .save_automatic_with(
                record("theirs", now - 1_000),
                b"theirs",
                Some(now + 60_000),
                &[],
                limits,
                now,
            )
            .unwrap()
            .id;
        let mut own = Vec::new();
        for index in 0..5 {
            let saved = store
                .save_automatic_with(
                    record("own", now - 100 + index),
                    b"own",
                    Some(now + 60_000),
                    &own,
                    limits,
                    now,
                )
                .unwrap();
            own.push(saved.id);
        }
        assert_eq!(
            automatic_ids(&store).len(),
            6,
            "pins keep all of them for now"
        );
        store.unpin_with(&own, limits, now).unwrap();
        let left = automatic_ids(&store);
        assert_eq!(left.len(), 3);
        assert!(left.contains(&theirs));
        assert!(left.contains(&own[4]) && left.contains(&own[3]));
    }

    fn fail_removing(path: PathBuf) {
        FAILING_REMOVALS.with(|failing| {
            failing
                .borrow_mut()
                .push(path.to_string_lossy().into_owned())
        });
    }

    fn stop_failing() {
        FAILING_REMOVALS.with(|failing| failing.borrow_mut().clear());
        FAILING_PINS.with(|failing| failing.set(false));
    }

    #[test]
    fn a_save_that_cannot_make_room_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let now = now_millis();
        let limits = PruneLimits {
            keep: 1,
            grace_millis: 0,
            max_count: 2,
            max_bytes: u64::MAX,
        };
        let old = fill(&store, "old", hours_ago(3));
        let older = fill(&store, "older", hours_ago(4));
        fail_removing(store.model_path(&older));
        assert!(store
            .save_automatic_with(record("new", now), b"new", None, &[], limits, now)
            .is_err());
        stop_failing();
        let written: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("-new-"))
            .collect();
        assert!(
            written.is_empty(),
            "the refused backup left nothing: {written:?}"
        );
        // What could be removed was; the one that could not stays counted.
        assert!(!store.exists(&old));
        assert!(store.exists(&older));
    }

    #[test]
    fn a_backup_whose_pin_and_removal_both_fail_is_reported_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let now = now_millis();
        let limits = PruneLimits {
            keep: 1,
            grace_millis: 0,
            max_count: 100,
            max_bytes: u64::MAX,
        };
        FAILING_PINS.with(|failing| failing.set(true));
        FAILING_REMOVALS.with(|failing| failing.borrow_mut().push("rollback-me".to_owned()));
        let error = store
            .save_automatic_with(
                record("rollback-me", now),
                b"x",
                Some(now + 60_000),
                &[],
                limits,
                now,
            )
            .unwrap_err();
        stop_failing();
        assert!(
            format!("{error:#}").contains("nor remove it again"),
            "{error:#}"
        );
        // It is still there and counted, so a later prune removes it once it
        // is old.
        let left = automatic_ids(&store);
        assert_eq!(left.len(), 1);
        let later = now + 1;
        store
            .save_automatic_with(record("next", later), b"y", None, &[], limits, later)
            .unwrap();
        assert!(!automatic_ids(&store).contains(&left[0]));
    }

    #[test]
    fn manual_backups_never_get_an_automatic_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let saved = store
            .save(record("auto-sync-deadbeef", 1_000), b"m")
            .unwrap();
        assert!(
            saved.id.contains("-manual-auto-sync-deadbeef"),
            "{}",
            saved.id
        );
        assert_eq!(saved.name, "auto-sync-deadbeef", "its name stays as given");
        let auto = store
            .save_automatic(record("auto-sync", now_millis()), b"a")
            .unwrap();
        assert!(auto.id.contains("-auto-sync-"));
        assert!(
            !store.claim_path(&auto.id).exists(),
            "the record replaces the claim"
        );
    }

    #[test]
    fn only_models_claimed_as_automatic_are_cleaned_up_without_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let now = now_millis();
        // A manual save interrupted between its model and its record, named
        // like an automatic one.
        let manual = "20261107-000000-auto-sync-deadbeef";
        fs::write(store.model_path(manual), b"manual").unwrap();
        // An automatic one interrupted the same way, which left its claim.
        let automatic = "20261107-000001-auto-sync-0123abcd";
        fs::write(store.model_path(automatic), b"auto").unwrap();
        fs::write(store.claim_path(automatic), b"").unwrap();
        // A claim whose save stopped before the model was written.
        let stray = "20261107-000002-auto-sync-00000000";
        fs::write(store.claim_path(stray), b"").unwrap();
        store.save_automatic(record("new", now), b"new").unwrap();
        assert!(
            store.model_path(manual).exists(),
            "an unclaimed model is never removed"
        );
        assert!(store.orphans().unwrap().contains(&manual.to_owned()));
        assert!(!store.model_path(automatic).exists() && !store.claim_path(automatic).exists());
        assert!(!store.claim_path(stray).exists());
    }

    #[test]
    fn a_model_that_cannot_be_deleted_keeps_its_record_and_refuses_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let now = now_millis();
        let limits = PruneLimits {
            keep: 1,
            grace_millis: 0,
            max_count: 100,
            max_bytes: u64::MAX,
        };
        let old = fill(&store, "old", hours_ago(3));
        fail_removing(store.model_path(&old));
        let refused = store.save_automatic_with(record("new", now), b"new", None, &[], limits, now);
        assert!(refused.is_err());
        let left = automatic_ids(&store);
        assert_eq!(
            left,
            vec![old.clone()],
            "the new one was not kept, and the old one is still counted"
        );
        // Once the model can be deleted, the next save removes it.
        stop_failing();
        let saved = store
            .save_automatic_with(record("new", now), b"new", None, &[], limits, now)
            .unwrap();
        assert_eq!(automatic_ids(&store), vec![saved.id]);
        assert!(!store.model_path(&old).exists() && !store.record_path(&old).exists());
    }

    #[test]
    fn leftovers_of_failed_removals_are_counted_and_removed_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let now = now_millis();
        let limits = PruneLimits {
            keep: 1,
            grace_millis: AUTOMATIC_GRACE_MILLIS,
            max_count: 100,
            max_bytes: 64,
        };
        // An automatic model whose record is gone, and a record whose model
        // is gone.
        let orphan = "20261107-000000-auto-sync-0123abcd";
        fs::write(store.model_path(orphan), [0u8; 100]).unwrap();
        fs::write(store.claim_path(orphan), b"").unwrap();
        let mut stray = record("auto-verify-fix", 1);
        stray.automatic = true;
        let stray = store.save(stray, b"x").unwrap().id;
        fs::remove_file(store.model_path(&stray)).unwrap();
        // A manual model without a record is left alone.
        let manual = "20261107-000000-Map";
        fs::write(store.model_path(manual), b"manual").unwrap();

        // While the orphan cannot be deleted, its 100 bytes still count, so a
        // save that would go over 64 bytes is refused.
        fail_removing(store.model_path(orphan));
        assert!(store
            .save_automatic_with(record("new", now), b"new", None, &[], limits, now)
            .is_err());
        assert!(!store.record_path(&stray).exists(), "the stray record went");
        stop_failing();
        store
            .save_automatic_with(record("new", now), b"new", None, &[], limits, now)
            .unwrap();
        assert!(!store.model_path(orphan).exists());
        assert!(store.model_path(manual).exists());
    }

    #[test]
    fn a_record_left_without_its_model_can_still_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let saved = store.save(record("Map", 1), b"model").unwrap();
        fail_removing(store.record_path(&saved.id));
        assert!(store.remove(&saved.id).is_err());
        stop_failing();
        assert!(
            !store.model_path(&saved.id).exists(),
            "the model went first"
        );
        assert!(store.list().unwrap().is_empty());
        store.remove(&saved.id).unwrap();
        assert!(!store.record_path(&saved.id).exists());
    }

    #[test]
    fn a_new_backup_is_refused_when_pinned_backups_fill_the_limits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let now = now_millis();
        let limits = PruneLimits {
            keep: 1,
            grace_millis: AUTOMATIC_GRACE_MILLIS,
            max_count: 2,
            max_bytes: u64::MAX,
        };
        for index in 0..2 {
            store
                .save_automatic_with(
                    record("pinned", now - 10 + index),
                    b"pin",
                    Some(now + 60_000),
                    &[],
                    limits,
                    now,
                )
                .unwrap();
        }
        let error = store
            .save_automatic_with(record("new", now), b"new", None, &[], limits, now)
            .unwrap_err();
        assert!(format!("{error:#}").contains("still rely on"), "{error:#}");
        assert_eq!(automatic_ids(&store).len(), 2, "nothing was saved");
        // Bytes count too.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let bytes = PruneLimits {
            max_count: 100,
            max_bytes: 5,
            ..limits
        };
        store
            .save_automatic_with(
                record("pinned", now),
                b"pin",
                Some(now + 60_000),
                &[],
                bytes,
                now,
            )
            .unwrap();
        assert!(store
            .save_automatic_with(record("new", now), b"new", None, &[], bytes, now)
            .is_err());
    }

    #[test]
    fn records_keep_every_root_and_old_records_load_without_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        let contents: Vec<(String, String)> = (0..300)
            .map(|index| {
                (
                    format!("game/ServerScriptService/Feature{index}/Module"),
                    "ModuleScript".to_owned(),
                )
            })
            .collect();
        let record = automatic_record(
            "auto-verify-fix",
            "auto: before verify --fix",
            &contents,
            None,
            None,
        );
        assert!(
            record.note.as_ref().unwrap().len() <= 512,
            "the note stays a summary"
        );
        let saved = store.save_automatic(record, b"model").unwrap();
        let (loaded, _) = store.get(&saved.id).unwrap();
        assert_eq!(loaded.roots.len(), 300);
        assert_eq!(
            loaded.roots[299].path,
            "game/ServerScriptService/Feature299/Module"
        );
        assert_eq!(loaded.roots[0].class_name, "ModuleScript");
        // A record written before roots existed.
        let old = r#"{"id":"20200101-000000-Map","name":"Map","instance":"game/Workspace/Map","className":"Model","placeId":1,"placeName":"Place","gitHead":null,"gitBranch":null,"createdAt":1,"bytes":5}"#;
        fs::write(dir.path().join("20200101-000000-Map.json"), old).unwrap();
        fs::write(dir.path().join("20200101-000000-Map.rbxm"), b"model").unwrap();
        let (old, _) = store.get("20200101-000000-Map").unwrap();
        assert!(old.roots.is_empty() && !old.automatic);
        let text = fs::read_to_string(store.record_path(&old.id)).unwrap();
        assert!(!text.contains("roots"));
    }

    #[test]
    fn a_prune_by_another_process_keeps_an_operations_young_backups() {
        let dir = tempfile::tempdir().unwrap();
        // Another worktree's server and this one, sharing the folder.
        let other = Store::in_dir(dir.path());
        let store = Store::in_dir(dir.path());
        let old: Vec<String> = (0..10)
            .map(|index| fill(&other, "old", hours_ago(3) + index))
            .collect();
        // A verify --fix saving more backups than are kept.
        let own: Vec<String> = (0..KEEP_AUTOMATIC as u64 + 3)
            .map(|index| {
                store
                    .save_automatic(record("own", hours_ago(0) - 10_000 + index * 100), b"own")
                    .unwrap()
                    .id
            })
            .collect();
        // Meanwhile the other server saves a reconnect backup, which prunes.
        let theirs = other
            .save_automatic(record("theirs", now_millis()), b"theirs")
            .unwrap();
        let left = automatic_ids(&store);
        for id in own.iter().chain([&theirs.id]) {
            assert!(left.contains(id), "{id} was pruned while young");
        }
        for id in &old {
            assert!(!left.contains(id), "{id} is old and beyond the limit");
        }
    }

    #[test]
    fn a_store_left_over_the_limit_is_pruned_by_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_dir(dir.path());
        // An operation that failed part way, or several, left these behind.
        for index in 0..KEEP_AUTOMATIC as u64 + 5 {
            fill(&store, "left", hours_ago(2) + index);
        }
        let next = store
            .save_automatic(record("next", now_millis()), b"next")
            .unwrap();
        let left = automatic_ids(&store);
        assert_eq!(left.len(), KEEP_AUTOMATIC);
        assert_eq!(left[0], next.id);
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
            store
                .prune_automatic_with(
                    PruneLimits {
                        keep: 0,
                        ..PruneLimits::DEFAULT
                    },
                    now_millis()
                )
                .unwrap(),
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
