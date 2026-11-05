//! Advisory leases on shared Roblox Studio windows, for many agents working
//! against a few open Studios at once.
//!
//! Play-testing in a Studio replaces its Edit datamodel for everyone using
//! that window, so agents need to agree on who may do what, even when the
//! Studio is not connected to `rojo serve` and the agents are separate
//! processes. Locks are therefore plain JSON files: one state file per lock
//! name, guarded by a short-lived `.lock` file during each read-modify-write.
//! Nothing enforces them; Studio tools that ignore them still work.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ExitStatus, UsageError};

pub(crate) const DEFAULT_TTL: Duration = Duration::from_secs(600);
pub(crate) const MAX_TTL: Duration = Duration::from_secs(3600);
/// A waiter that has not refreshed its ticket for this long is assumed to be
/// gone, for example killed, so it stops blocking others.
const TICKET_STALE: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long one update waits for another process's update to finish. Updates
/// take milliseconds, so this only runs out when something is stuck.
const MUTEX_WAIT: Duration = Duration::from_secs(5);
/// The event log is rotated into `<name>.log.jsonl.1` past this size, so it
/// stays bounded at about twice this.
const LOG_LIMIT: u64 = 256 * 1024;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
const STATE_VERSION: u32 = 1;
const MAX_OWNER_LEN: usize = 128;
const MAX_TEXT_LEN: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LockMode {
    Shared,
    Exclusive,
}

impl LockMode {
    fn as_str(self) -> &'static str {
        match self {
            LockMode::Shared => "shared",
            LockMode::Exclusive => "exclusive",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Pause {
    pub reason: Option<String>,
    pub since_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Holder {
    pub owner: String,
    pub mode: LockMode,
    pub acquired_ms: u64,
    pub expires_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Waiter {
    pub owner: String,
    pub mode: LockMode,
    pub since_ms: u64,
    pub seen_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    version: u32,
    #[serde(default)]
    paused: Option<Pause>,
    #[serde(default)]
    holders: Vec<Holder>,
    #[serde(default)]
    waiters: Vec<Waiter>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HolderView {
    #[serde(flatten)]
    pub holder: Holder,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WaiterView {
    #[serde(flatten)]
    pub waiter: Waiter,
    pub waiting_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LockStatus {
    pub name: String,
    pub paused: Option<Pause>,
    pub holders: Vec<HolderView>,
    /// Oldest first, which is the order they are served in.
    pub waiters: Vec<WaiterView>,
}

#[derive(Debug, Clone)]
pub(crate) struct AcquireRequest {
    pub name: String,
    pub owner: String,
    pub mode: LockMode,
    /// Most shared holders at once, counting this one. Ignored for exclusive.
    pub limit: Option<u32>,
    pub ttl: Duration,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AcquireOutcome {
    pub granted: bool,
    /// The owner already held the lock in this mode and its expiry was extended.
    pub renewed: bool,
    pub name: String,
    pub owner: String,
    pub mode: LockMode,
    pub expires_ms: Option<u64>,
    pub expires_in_secs: Option<u64>,
    pub waited_secs: u64,
    /// Why the lock was not granted.
    pub reason: Option<String>,
    pub status: LockStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReleaseOutcome {
    pub name: String,
    pub owner: String,
    pub released: bool,
    pub mode: Option<LockMode>,
    pub held_secs: Option<u64>,
    pub dropped_ticket: bool,
}

/// What a single acquire attempt does with the caller's place in line when
/// the lock is not granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// Try once without queueing.
    Once,
    /// Keep (or take) a waiter ticket, so later arrivals do not overtake.
    Waiting,
    /// The final try of a wait that ran out: drop the ticket and log it.
    GiveUp,
}

/// Checks a lock name and returns its canonical, lowercase form. Names are
/// lowercased because they are file names, and `Main` and `main` would share
/// one file on case-insensitive file systems such as the Windows default.
pub(crate) fn validate_name(name: &str) -> anyhow::Result<String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'));
    if !valid {
        bail!("Lock names are 1-64 letters, digits, '_', '.' or '-', such as main or build");
    }
    // Windows drops trailing dots from file names, and leading dots or
    // hyphens make the name look like a hidden file or an option.
    if name.starts_with(['.', '-']) || name.ends_with('.') {
        bail!("Lock names cannot start with '.' or '-' or end with '.'");
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        bail!("Lock name {name} is reserved on Windows");
    }
    Ok(name.to_ascii_lowercase())
}

fn validate_text(kind: &str, text: &str, max: usize) -> anyhow::Result<()> {
    if text.trim().is_empty() || text.chars().count() > max {
        bail!("The {kind} must be 1-{max} characters");
    }
    // Owners and notes are printed in status output, where control
    // characters could rewrite the reader's terminal.
    if text.chars().any(char::is_control) {
        bail!("The {kind} cannot contain control characters");
    }
    Ok(())
}

fn validate_owner(owner: &str) -> anyhow::Result<()> {
    validate_text("owner", owner, MAX_OWNER_LEN)
}

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn secs_until(later_ms: u64, now_ms: u64) -> u64 {
    later_ms.saturating_sub(now_ms).div_ceil(1000)
}

fn secs_since(earlier_ms: u64, now_ms: u64) -> u64 {
    now_ms.saturating_sub(earlier_ms) / 1000
}

/// A directory of lock state files. Cheap to clone; every operation reads
/// and writes the files, so separate stores on one directory, in one process
/// or many, see the same locks.
#[derive(Clone)]
pub(crate) struct LockStore {
    dir: PathBuf,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl std::fmt::Debug for LockStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockStore").field("dir", &self.dir).finish()
    }
}

impl LockStore {
    pub(crate) fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            clock: Arc::new(system_now_ms),
        }
    }

    /// The default store for `start`: `rojo-state/studio-locks` in the Git
    /// common directory, so every worktree of a repository shares its locks,
    /// or a per-user directory for `start` outside a Git repository (see
    /// [`super::project_state_dir`]), never inside the project.
    pub(crate) fn discover(start: &Path) -> anyhow::Result<Self> {
        // Only "not a repository" (or no Git at all) falls back. A repository
        // whose common directory cannot be found is an error, since silently
        // using a per-directory store would let two worktrees both take a
        // lock meant to be exclusive.
        if super::lifecycle::git(start, ["rev-parse", "--git-dir"]).is_ok() {
            let common = super::lifecycle::git_common_dir(start)?;
            // Git prints forward slashes on Windows; rebuilding the path from
            // its components gives one separator style in printed paths.
            let common: PathBuf = common.components().collect();
            Ok(Self::new(common.join("rojo-state").join("studio-locks")))
        } else {
            Ok(Self::new(
                super::project_state_dir(start)?.join("studio-locks"),
            ))
        }
    }

    #[cfg(test)]
    fn with_clock(dir: impl Into<PathBuf>, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            dir: dir.into(),
            clock,
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn state_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    fn log_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.log.jsonl"))
    }

    /// Tries to take the lock once. With `wait` other than zero, keeps
    /// trying about every second until granted or the wait runs out, holding
    /// a place in line meanwhile. `on_busy` is called after each attempt that
    /// was not granted but will be retried.
    pub(crate) fn acquire(
        &self,
        request: &AcquireRequest,
        wait: Duration,
        mut on_busy: impl FnMut(&AcquireOutcome),
    ) -> anyhow::Result<AcquireOutcome> {
        let start = Instant::now();
        loop {
            let remaining = wait.saturating_sub(start.elapsed());
            let attempt = if wait.is_zero() {
                Attempt::Once
            } else if remaining.is_zero() {
                Attempt::GiveUp
            } else {
                Attempt::Waiting
            };
            let mut outcome = self.attempt(request, attempt)?;
            outcome.waited_secs = start.elapsed().as_secs();
            if outcome.granted || attempt != Attempt::Waiting {
                return Ok(outcome);
            }
            on_busy(&outcome);
            thread::sleep(POLL_INTERVAL.min(remaining));
        }
    }

    fn attempt(
        &self,
        request: &AcquireRequest,
        attempt: Attempt,
    ) -> anyhow::Result<AcquireOutcome> {
        let name = validate_name(&request.name)?;
        validate_owner(&request.owner)?;
        if let Some(note) = &request.note {
            validate_text("note", note, MAX_TEXT_LEN)?;
        }
        if request.ttl.is_zero() || request.ttl > MAX_TTL {
            bail!("The TTL must be 1-{} seconds", MAX_TTL.as_secs());
        }
        if request.limit == Some(0) {
            bail!("The limit must be at least 1");
        }
        let owner = request.owner.as_str();
        let mode = request.mode;
        let limit = match mode {
            LockMode::Shared => request.limit,
            LockMode::Exclusive => None,
        };
        let ttl_ms = request.ttl.as_millis() as u64;

        self.update(&name, |state, now, events| {
            let renewing = state
                .holders
                .iter_mut()
                .find(|h| h.owner == owner && h.mode == mode);
            // Renewing is always allowed, even while paused or with someone
            // in line: the holder has the lock until it expires anyway, and
            // refusing would only make it lapse mid-task.
            if let Some(holder) = renewing {
                holder.expires_ms = now + ttl_ms;
                holder.limit = limit;
                if request.note.is_some() {
                    holder.note = request.note.clone();
                }
                let expires_ms = holder.expires_ms;
                state.waiters.retain(|w| w.owner != owner);
                events.push(event(now, "renew", Some(owner), Some(mode), None));
                return Ok((true, true, Some(expires_ms), None));
            }

            let ticket = state
                .waiters
                .iter()
                .find(|w| w.owner == owner && w.mode == mode)
                .cloned();
            let since = ticket.as_ref().map_or(now, |t| t.since_ms);
            match decide(state, owner, mode, limit, since, now) {
                Ok(()) => {
                    state.holders.retain(|h| h.owner != owner);
                    state.waiters.retain(|w| w.owner != owner);
                    state.holders.push(Holder {
                        owner: owner.to_owned(),
                        mode,
                        acquired_ms: now,
                        expires_ms: now + ttl_ms,
                        limit,
                        note: request.note.clone(),
                    });
                    events.push(event(
                        now,
                        "acquire",
                        Some(owner),
                        Some(mode),
                        Some(format!("waited {}s", secs_since(since, now))),
                    ));
                    Ok((true, false, Some(now + ttl_ms), None))
                }
                Err(reason) => {
                    match attempt {
                        Attempt::Waiting => match state
                            .waiters
                            .iter_mut()
                            .find(|w| w.owner == owner && w.mode == mode)
                        {
                            Some(waiter) => {
                                waiter.seen_ms = now;
                                waiter.limit = limit;
                            }
                            None => {
                                // One place in line per owner; asking in another
                                // mode starts over at the back.
                                state.waiters.retain(|w| w.owner != owner);
                                state.waiters.push(Waiter {
                                    owner: owner.to_owned(),
                                    mode,
                                    since_ms: now,
                                    seen_ms: now,
                                    limit,
                                });
                            }
                        },
                        Attempt::Once | Attempt::GiveUp => {
                            state.waiters.retain(|w| w.owner != owner);
                            if attempt == Attempt::GiveUp {
                                events.push(event(
                                    now,
                                    "timeout",
                                    Some(owner),
                                    Some(mode),
                                    Some(format!(
                                        "gave up after {}s: {reason}",
                                        secs_since(since, now)
                                    )),
                                ));
                            }
                        }
                    }
                    Ok((false, false, None, Some(reason)))
                }
            }
        })
        .map(
            |((granted, renewed, expires_ms, reason), status, now)| AcquireOutcome {
                granted,
                renewed,
                name: name.clone(),
                owner: owner.to_owned(),
                mode,
                expires_ms,
                expires_in_secs: expires_ms.map(|e| secs_until(e, now)),
                waited_secs: 0,
                reason,
                status,
            },
        )
    }

    /// Removes the owner's hold and place in line. Releasing a lock the owner
    /// does not hold is not an error, so cleanup can always run.
    pub(crate) fn release(&self, name: &str, owner: &str) -> anyhow::Result<ReleaseOutcome> {
        let name = validate_name(name)?;
        validate_owner(owner)?;
        let ((mode, held_secs, dropped_ticket), _, _) =
            self.update(&name, |state, now, events| {
                let held = state.holders.iter().position(|h| h.owner == owner);
                let held = held.map(|index| state.holders.remove(index));
                let waiters = state.waiters.len();
                state.waiters.retain(|w| w.owner != owner);
                let dropped_ticket = state.waiters.len() != waiters;
                let held_secs = held.as_ref().map(|h| secs_since(h.acquired_ms, now));
                if let Some(holder) = &held {
                    events.push(event(
                        now,
                        "release",
                        Some(owner),
                        Some(holder.mode),
                        Some(format!("held {}s", held_secs.unwrap_or_default())),
                    ));
                }
                Ok((held.map(|h| h.mode), held_secs, dropped_ticket))
            })?;
        Ok(ReleaseOutcome {
            name,
            owner: owner.to_owned(),
            released: mode.is_some(),
            mode,
            held_secs,
            dropped_ticket,
        })
    }

    pub(crate) fn status(&self, name: &str) -> anyhow::Result<LockStatus> {
        let name = validate_name(name)?;
        Ok(self.update(&name, |_, _, _| Ok(()))?.1)
    }

    /// The status of every lock in the store, by name.
    pub(crate) fn status_all(&self) -> anyhow::Result<Vec<LockStatus>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot read {}", self.dir.display()))
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(name) = validate_name(stem) {
                if name == stem {
                    names.push(name);
                }
            }
        }
        names.sort();
        names.iter().map(|name| self.status(name)).collect()
    }

    /// Stops new grants (renewals still work) until [`LockStore::resume`],
    /// for example while Rojo or the Studio is down.
    pub(crate) fn pause(&self, name: &str, reason: Option<&str>) -> anyhow::Result<LockStatus> {
        let name = validate_name(name)?;
        if let Some(reason) = reason {
            validate_text("reason", reason, MAX_TEXT_LEN)?;
        }
        Ok(self
            .update(&name, |state, now, events| {
                state.paused = Some(Pause {
                    reason: reason.map(str::to_owned),
                    since_ms: now,
                });
                events.push(event(now, "pause", None, None, reason.map(str::to_owned)));
                Ok(())
            })?
            .1)
    }

    pub(crate) fn resume(&self, name: &str) -> anyhow::Result<LockStatus> {
        let name = validate_name(name)?;
        Ok(self
            .update(&name, |state, now, events| {
                if state.paused.take().is_some() {
                    events.push(event(now, "resume", None, None, None));
                }
                Ok(())
            })?
            .1)
    }

    /// Deletes a lock's state, dropping every hold and place in line. This is
    /// the way out of a state file that cannot be read, which every other
    /// operation refuses to touch so that holds are never lost silently. The
    /// old contents are kept in the event log.
    pub(crate) fn reset(&self, name: &str) -> anyhow::Result<bool> {
        let name = validate_name(name)?;
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("Cannot create {}", self.dir.display()))?;
        let path = self.state_path(&name);
        let _guard = StateMutex::acquire(&path)?;
        let previous = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot read {}", path.display()))
            }
        };
        fs::remove_file(&path).with_context(|| format!("Cannot delete {}", path.display()))?;
        let previous: String = String::from_utf8_lossy(&previous)
            .chars()
            .take(2000)
            .collect();
        self.append_log(
            &name,
            &[event(
                self.now(),
                "reset",
                None,
                None,
                Some(format!("previous state: {previous}")),
            )],
        );
        Ok(true)
    }

    /// The last `limit` events for a lock, oldest first.
    pub(crate) fn log(&self, name: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
        let name = validate_name(name)?;
        let current = self.log_path(&name);
        let mut rotated = current.clone().into_os_string();
        rotated.push(".1");
        let mut events = Vec::new();
        for path in [PathBuf::from(rotated), current] {
            match fs::read_to_string(&path) {
                Ok(text) => events.extend(
                    text.lines()
                        .filter_map(|line| serde_json::from_str::<Value>(line).ok()),
                ),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("Cannot read {}", path.display()))
                }
            }
        }
        let skip = events.len().saturating_sub(limit);
        Ok(events.split_off(skip))
    }

    /// Runs `change` on the lock's state under the store's guard file, after
    /// dropping expired holders and stale tickets, and writes the result when
    /// anything changed. Returns `change`'s value, the status afterwards and
    /// the time used.
    fn update<T>(
        &self,
        name: &str,
        change: impl FnOnce(&mut State, u64, &mut Vec<Value>) -> anyhow::Result<T>,
    ) -> anyhow::Result<(T, LockStatus, u64)> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("Cannot create {}", self.dir.display()))?;
        let path = self.state_path(name);
        let _guard = StateMutex::acquire(&path)?;
        let now = self.now();
        let mut state = read_state(&path)?;
        let before = serde_json::to_vec(&state)?;
        let mut events = Vec::new();

        state.holders.retain(|holder| {
            let live = holder.expires_ms > now;
            if !live {
                events.push(event(
                    now,
                    "expire",
                    Some(&holder.owner),
                    Some(holder.mode),
                    Some(format!("held {}s", secs_since(holder.acquired_ms, now))),
                ));
            }
            live
        });
        let stale_ms = TICKET_STALE.as_millis() as u64;
        state
            .waiters
            .retain(|waiter| now.saturating_sub(waiter.seen_ms) <= stale_ms);

        let value = change(&mut state, now, &mut events)?;
        state.version = STATE_VERSION;
        if serde_json::to_vec(&state)? != before {
            write_state(&path, &state)?;
        }
        self.append_log(name, &events);
        Ok((value, view(name, &state, now), now))
    }

    /// The log is a convenience for people reading what happened, so failing
    /// to write it never fails the operation.
    fn append_log(&self, name: &str, events: &[Value]) {
        if events.is_empty() {
            return;
        }
        let path = self.log_path(name);
        if fs::metadata(&path).is_ok_and(|m| m.len() > LOG_LIMIT) {
            let mut rotated = path.clone().into_os_string();
            rotated.push(".1");
            let _ = fs::rename(&path, rotated);
        }
        let mut text = String::new();
        for event in events {
            text.push_str(&event.to_string());
            text.push('\n');
        }
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(text.as_bytes()));
        if let Err(error) = result {
            log::warn!("Could not write {}: {error}", path.display());
        }
    }
}

/// Whether `owner` may take the lock in `mode` now, given the time it joined
/// the line (`since`, or now when it has no ticket). The owner's own hold and
/// ticket never block it, so a holder can switch modes.
fn decide(
    state: &State,
    owner: &str,
    mode: LockMode,
    limit: Option<u32>,
    since: u64,
    now: u64,
) -> Result<(), String> {
    if let Some(pause) = &state.paused {
        return Err(match &pause.reason {
            Some(reason) => format!("paused: {reason}"),
            None => "paused".to_owned(),
        });
    }
    let others = || state.holders.iter().filter(|h| h.owner != owner);
    let held_by = |h: &Holder| {
        format!(
            "held by {} ({}, expires in {}s)",
            h.owner,
            h.mode.as_str(),
            secs_until(h.expires_ms, now)
        )
    };
    // Ties on the join time are broken by owner, so every process agrees.
    let ahead = |w: &&Waiter| w.owner != owner && (w.since_ms, w.owner.as_str()) < (since, owner);
    let exclusive_ahead = state
        .waiters
        .iter()
        .filter(ahead)
        .find(|w| w.mode == LockMode::Exclusive);

    match mode {
        LockMode::Exclusive => {
            if let Some(holder) = others().next() {
                return Err(held_by(holder));
            }
            // Any waiter that came first goes first, shared ones included, or
            // a steady stream of exclusive requests would starve them.
            if let Some(waiter) =
                state.waiters.iter().filter(ahead).min_by(|a, b| {
                    (a.since_ms, a.owner.as_str()).cmp(&(b.since_ms, b.owner.as_str()))
                })
            {
                return Err(format!(
                    "{} has been waiting longer for {}",
                    waiter.owner,
                    waiter.mode.as_str()
                ));
            }
        }
        LockMode::Shared => {
            if let Some(holder) = others().find(|h| h.mode == LockMode::Exclusive) {
                return Err(held_by(holder));
            }
            // An exclusive waiter that came first blocks new shared grants,
            // or a steady stream of shared holders would starve it.
            if let Some(waiter) = exclusive_ahead {
                return Err(format!(
                    "{} is waiting for exclusive and came first",
                    waiter.owner
                ));
            }
            if let Some(limit) = limit {
                let held = others().filter(|h| h.mode == LockMode::Shared).count();
                if held >= limit as usize {
                    let holders: Vec<_> = others().map(|h| h.owner.as_str()).collect();
                    return Err(format!(
                        "all {limit} shared slots are taken ({})",
                        holders.join(", ")
                    ));
                }
                let queued = state
                    .waiters
                    .iter()
                    .filter(ahead)
                    .filter(|w| w.mode == LockMode::Shared)
                    .count();
                if held + queued >= limit as usize {
                    return Err(format!(
                        "{queued} shared waiter(s) came first for the {} free slot(s)",
                        limit as usize - held
                    ));
                }
            }
        }
    }
    Ok(())
}

fn view(name: &str, state: &State, now: u64) -> LockStatus {
    let mut waiters: Vec<_> = state
        .waiters
        .iter()
        .map(|waiter| WaiterView {
            waiter: waiter.clone(),
            waiting_secs: secs_since(waiter.since_ms, now),
        })
        .collect();
    waiters.sort_by(|a, b| {
        (a.waiter.since_ms, &a.waiter.owner).cmp(&(b.waiter.since_ms, &b.waiter.owner))
    });
    LockStatus {
        name: name.to_owned(),
        paused: state.paused.clone(),
        holders: state
            .holders
            .iter()
            .map(|holder| HolderView {
                holder: holder.clone(),
                expires_in_secs: secs_until(holder.expires_ms, now),
            })
            .collect(),
        waiters,
    }
}

fn event(
    now: u64,
    kind: &str,
    owner: Option<&str>,
    mode: Option<LockMode>,
    detail: Option<String>,
) -> Value {
    let mut value = serde_json::json!({
        "atMs": now,
        "time": format_time(now),
        "event": kind,
    });
    if let Some(owner) = owner {
        value["owner"] = owner.into();
    }
    if let Some(mode) = mode {
        value["mode"] = mode.as_str().into();
    }
    if let Some(detail) = detail {
        value["detail"] = detail.into();
    }
    value
}

fn format_time(ms: u64) -> String {
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_millis(ms)).to_string()
}

/// A state file that cannot be read is an error rather than an empty lock:
/// treating it as empty would hand out a lock someone may hold.
fn read_state(path: &Path) -> anyhow::Result<State> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(State {
                version: STATE_VERSION,
                ..State::default()
            })
        }
        Err(error) => return Err(error).with_context(|| format!("Cannot read {}", path.display())),
    };
    let reset_hint = || {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        format!(
            "Inspect it, then run `rojo studio lock reset {name}` to start over \
             (this drops every hold on the lock)"
        )
    };
    if bytes.len() as u64 > MAX_STATE_BYTES {
        bail!(
            "{} is larger than 1 MiB, more than any lock state. {}",
            path.display(),
            reset_hint()
        );
    }
    let state: State = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "{} is not valid lock state: {error}. {}",
            path.display(),
            reset_hint()
        )
    })?;
    if state.version > STATE_VERSION {
        bail!(
            "{} was written by a newer Rojo (lock state version {}); update Rojo",
            path.display(),
            state.version
        );
    }
    Ok(state)
}

/// Writes through a temporary file and a rename, so a process killed while
/// writing leaves the previous state, never half a file.
fn write_state(path: &Path, state: &State) -> anyhow::Result<()> {
    let parent = path.parent().context("Missing parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("Cannot write to {}", parent.display()))?;
    serde_json::to_writer_pretty(&mut file, state)?;
    file.flush()?;
    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Cannot write {}", path.display()))?;
    Ok(())
}

/// Held for one read-modify-write of a state file. It is an operating system
/// lock on `<name>.json.lock`, which the system releases when the process
/// exits, even when it is killed mid-update. A lock file that is left behind
/// therefore never blocks anyone, and no process ever has to decide that
/// another one died and remove its lock, which two processes could otherwise
/// both do at once and end up updating the state together.
struct StateMutex {
    // Dropping the file releases the lock. The file itself stays.
    _file: fs::File,
}

impl StateMutex {
    fn acquire(state_path: &Path) -> anyhow::Result<Self> {
        let mut path = state_path.as_os_str().to_owned();
        path.push(".lock");
        let path = PathBuf::from(path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("Cannot create {}", path.display()))?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {
                    if start.elapsed() > MUTEX_WAIT {
                        bail!(
                            "{} has been locked by another update for over {} seconds. \
                             The lock is released when that process exits.",
                            path.display(),
                            MUTEX_WAIT.as_secs(),
                        );
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(error).with_context(|| format!("Cannot lock {}", path.display()))
                }
            }
        }
    }
}

/// Coordinate shared Studio windows between agents with advisory leases.
/// Needs no token or `rojo serve`. See `rojo doc --agent studio-locks`.
#[derive(Debug, Parser)]
pub struct LockCommand {
    /// Directory holding the lock files. Defaults to rojo-state/studio-locks in
    /// the Git common directory, shared by all worktrees, or a per-user folder
    /// for the current directory outside Git, never inside the project.
    #[clap(long, global = true)]
    pub dir: Option<PathBuf>,
    #[clap(subcommand)]
    pub command: LockSubcommand,
}

#[derive(Debug, Parser)]
pub enum LockSubcommand {
    /// Take or renew a lock. Exits with 3 when it is not granted.
    Acquire {
        name: String,
        /// Who holds the lock, such as the agent's name. Release with the same.
        #[clap(long)]
        owner: String,
        /// Take the lock alone (for example for a play test) instead of shared.
        #[clap(long)]
        exclusive: bool,
        /// With a shared lock, the most shared holders at once.
        #[clap(long, conflicts_with = "exclusive")]
        limit: Option<u32>,
        /// Seconds until the hold expires unless renewed, at most 3600.
        #[clap(long, default_value_t = DEFAULT_TTL.as_secs())]
        ttl: u64,
        /// Seconds to keep trying, holding a place in line. 0 tries once.
        #[clap(long, default_value = "0")]
        wait: u64,
        /// What the hold is for, shown in status.
        #[clap(long)]
        note: Option<String>,
    },
    /// Give up a hold or place in line. Releasing what is not held is fine.
    Release {
        name: String,
        #[clap(long)]
        owner: String,
    },
    /// Show holders, waiters and pause state of one lock, or of all.
    Status { name: Option<String> },
    /// Stop granting the lock to anyone new until it is resumed.
    Pause {
        name: String,
        #[clap(long)]
        reason: Option<String>,
    },
    /// Grant the lock again after a pause.
    Resume { name: String },
    /// Show the lock's most recent events.
    Log {
        name: String,
        #[clap(short = 'n', long = "lines", default_value = "20")]
        lines: usize,
    },
    /// Delete the lock's state, dropping every hold. For unreadable state files.
    Reset { name: String },
}

/// Mistakes in the request exit with 2, like other `rojo studio` usage errors.
fn usage<T>(result: anyhow::Result<T>) -> anyhow::Result<T> {
    result.map_err(|error| UsageError(error).into())
}

impl LockCommand {
    pub fn run(self, json: bool) -> anyhow::Result<()> {
        let store = match &self.dir {
            Some(dir) => LockStore::new(super::resolve_path(dir)?.into_owned()),
            None => LockStore::discover(
                &std::env::current_dir().context("Cannot read the current directory")?,
            )?,
        };
        match self.command {
            LockSubcommand::Acquire {
                name,
                owner,
                exclusive,
                limit,
                ttl,
                wait,
                note,
            } => {
                usage(validate_name(&name).and_then(|_| validate_owner(&owner)))?;
                if ttl == 0 || ttl > MAX_TTL.as_secs() {
                    return Err(UsageError(anyhow::anyhow!(
                        "--ttl must be 1-{} seconds",
                        MAX_TTL.as_secs()
                    ))
                    .into());
                }
                if limit == Some(0) {
                    return Err(UsageError(anyhow::anyhow!("--limit must be at least 1")).into());
                }
                if let Some(note) = &note {
                    usage(validate_text("note", note, MAX_TEXT_LEN))?;
                }
                let request = AcquireRequest {
                    name,
                    owner,
                    mode: if exclusive {
                        LockMode::Exclusive
                    } else {
                        LockMode::Shared
                    },
                    limit,
                    ttl: Duration::from_secs(ttl),
                    note,
                };
                let mut announced = false;
                let outcome = store.acquire(&request, Duration::from_secs(wait), |busy| {
                    if !announced && !json {
                        eprintln!(
                            "Waiting up to {wait}s for {}: {}",
                            busy.name,
                            busy.reason.as_deref().unwrap_or("busy")
                        );
                        announced = true;
                    }
                })?;
                if json {
                    println!("{}", serde_json::to_string(&outcome)?);
                } else if outcome.granted {
                    println!(
                        "{} {} ({}) to {}, expires in {}s",
                        if outcome.renewed {
                            "Renewed"
                        } else {
                            "Granted"
                        },
                        outcome.name,
                        outcome.mode.as_str(),
                        outcome.owner,
                        outcome.expires_in_secs.unwrap_or_default()
                    );
                } else {
                    println!(
                        "{} is busy: {}",
                        outcome.name,
                        outcome.reason.as_deref().unwrap_or("not granted")
                    );
                    print_status(&outcome.status);
                }
                if !outcome.granted {
                    return Err(ExitStatus(3).into());
                }
            }
            LockSubcommand::Release { name, owner } => {
                usage(validate_name(&name).and_then(|_| validate_owner(&owner)))?;
                let outcome = store.release(&name, &owner)?;
                if json {
                    println!("{}", serde_json::to_string(&outcome)?);
                } else if let Some(mode) = outcome.mode {
                    println!(
                        "Released {} ({}) held by {} for {}s",
                        outcome.name,
                        mode.as_str(),
                        outcome.owner,
                        outcome.held_secs.unwrap_or_default()
                    );
                } else if outcome.dropped_ticket {
                    println!(
                        "{} was waiting for {} but did not hold it; left the line",
                        outcome.owner, outcome.name
                    );
                } else {
                    println!(
                        "{} did not hold {}; nothing to release",
                        outcome.owner, outcome.name
                    );
                }
            }
            LockSubcommand::Status { name } => {
                let statuses = match name {
                    Some(name) => vec![store.status(&usage(validate_name(&name))?)?],
                    None => store.status_all()?,
                };
                if json {
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "dir": store.dir(),
                            "locks": statuses,
                        }))?
                    );
                } else if statuses.is_empty() {
                    println!("No locks in {}", store.dir().display());
                } else {
                    for status in &statuses {
                        print_status(status);
                    }
                }
            }
            LockSubcommand::Pause { name, reason } => {
                usage(validate_name(&name))?;
                if let Some(reason) = &reason {
                    usage(validate_text("reason", reason, MAX_TEXT_LEN))?;
                }
                let status = store.pause(&name, reason.as_deref())?;
                if json {
                    println!("{}", serde_json::to_string(&status)?);
                } else {
                    print_status(&status);
                }
            }
            LockSubcommand::Resume { name } => {
                let status = store.resume(&usage(validate_name(&name))?)?;
                if json {
                    println!("{}", serde_json::to_string(&status)?);
                } else {
                    print_status(&status);
                }
            }
            LockSubcommand::Log { name, lines } => {
                let events = store.log(&usage(validate_name(&name))?, lines)?;
                if json {
                    println!("{}", serde_json::to_string(&events)?);
                } else if events.is_empty() {
                    println!("No events for {name}");
                } else {
                    for event in &events {
                        let field = |key: &str| event[key].as_str().unwrap_or_default();
                        println!(
                            "{} {:<8} {} {} {}",
                            field("time"),
                            field("event"),
                            field("owner"),
                            field("mode"),
                            field("detail")
                        );
                    }
                }
            }
            LockSubcommand::Reset { name } => {
                let name = usage(validate_name(&name))?;
                let existed = store.reset(&name)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string(
                            &serde_json::json!({"name": name, "reset": existed})
                        )?
                    );
                } else if existed {
                    println!(
                        "Reset {name}; its previous state is in `rojo studio lock log {name}`"
                    );
                } else {
                    println!("{name} had no state; nothing to reset");
                }
            }
        }
        Ok(())
    }
}

fn print_status(status: &LockStatus) {
    let summary = match status.holders.len() {
        0 => "free".to_owned(),
        1 => "1 holder".to_owned(),
        n => format!("{n} holders"),
    };
    match &status.paused {
        Some(pause) => println!(
            "{}: {summary}, PAUSED{}",
            status.name,
            pause
                .reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ),
        None => println!("{}: {summary}", status.name),
    }
    for view in &status.holders {
        let holder = &view.holder;
        println!(
            "  {} {} since {}, expires in {}s{}{}",
            holder.owner,
            holder.mode.as_str(),
            format_time(holder.acquired_ms),
            view.expires_in_secs,
            holder
                .limit
                .map(|l| format!(", limit {l}"))
                .unwrap_or_default(),
            holder
                .note
                .as_deref()
                .map(|n| format!(": {n}"))
                .unwrap_or_default()
        );
    }
    for view in &status.waiters {
        println!(
            "  waiting: {} {} for {}s",
            view.waiter.owner,
            view.waiter.mode.as_str(),
            view.waiting_secs
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    struct Fixture {
        _dir: tempfile::TempDir,
        clock: Arc<AtomicU64>,
        store: LockStore,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let clock = Arc::new(AtomicU64::new(1_000_000));
            let time = clock.clone();
            let store = LockStore::with_clock(
                dir.path().join("locks"),
                Arc::new(move || time.load(Ordering::SeqCst)),
            );
            Self {
                _dir: dir,
                clock,
                store,
            }
        }

        fn advance(&self, duration: Duration) {
            self.clock
                .fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
        }

        fn try_lock(&self, owner: &str, mode: LockMode, limit: Option<u32>) -> AcquireOutcome {
            self.attempt(owner, mode, limit, Attempt::Once)
        }

        /// One poll of a waiting acquire, which keeps a ticket.
        fn wait_lock(&self, owner: &str, mode: LockMode) -> AcquireOutcome {
            self.attempt(owner, mode, None, Attempt::Waiting)
        }

        fn attempt(
            &self,
            owner: &str,
            mode: LockMode,
            limit: Option<u32>,
            attempt: Attempt,
        ) -> AcquireOutcome {
            self.store
                .attempt(&request("main", owner, mode, limit), attempt)
                .unwrap()
        }
    }

    fn request(name: &str, owner: &str, mode: LockMode, limit: Option<u32>) -> AcquireRequest {
        AcquireRequest {
            name: name.to_owned(),
            owner: owner.to_owned(),
            mode,
            limit,
            ttl: DEFAULT_TTL,
            note: None,
        }
    }

    use LockMode::{Exclusive, Shared};

    #[test]
    fn shared_holders_coexist_and_block_exclusive() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Shared, None).granted);
        assert!(f.try_lock("b", Shared, None).granted);
        let busy = f.try_lock("c", Exclusive, None);
        assert!(!busy.granted);
        assert!(busy.reason.unwrap().contains("held by"));
        assert_eq!(busy.status.holders.len(), 2);
    }

    #[test]
    fn exclusive_blocks_everyone_else() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Exclusive, None).granted);
        let busy = f.try_lock("b", Shared, None);
        assert!(!busy.granted);
        assert!(busy.reason.unwrap().contains("held by a (exclusive"));
        assert!(!f.try_lock("c", Exclusive, None).granted);
        f.store.release("main", "a").unwrap();
        assert!(f.try_lock("c", Exclusive, None).granted);
    }

    #[test]
    fn shared_limit_caps_holders() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Shared, Some(2)).granted);
        assert!(f.try_lock("b", Shared, Some(2)).granted);
        let busy = f.try_lock("c", Shared, Some(2));
        assert!(!busy.granted);
        assert!(busy.reason.unwrap().contains("2 shared slots"));
        // Without a limit of its own, a caller is not bound by others' limits.
        assert!(f.try_lock("d", Shared, None).granted);
        f.store.release("main", "a").unwrap();
        f.store.release("main", "d").unwrap();
        assert!(f.try_lock("c", Shared, Some(2)).granted);
    }

    #[test]
    fn shared_waiters_keep_their_place_for_limited_slots() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Shared, Some(1)).granted);
        assert!(!f.attempt("b", Shared, Some(1), Attempt::Waiting).granted);
        f.store.release("main", "a").unwrap();
        // c arrives after b started waiting, so the free slot is b's.
        assert!(!f.try_lock("c", Shared, Some(1)).granted);
        assert!(f.attempt("b", Shared, Some(1), Attempt::Waiting).granted);
    }

    #[test]
    fn an_earlier_exclusive_waiter_blocks_new_shared_grants() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Shared, None).granted);
        let waiting = f.wait_lock("b", Exclusive);
        assert!(!waiting.granted);
        assert_eq!(waiting.status.waiters.len(), 1);
        f.advance(Duration::from_secs(1));
        let busy = f.try_lock("c", Shared, None);
        assert!(!busy.granted);
        assert!(busy.reason.unwrap().contains("b is waiting for exclusive"));
        // a can still renew what it already holds.
        assert!(f.try_lock("a", Shared, None).renewed);
        f.store.release("main", "a").unwrap();
        let granted = f.wait_lock("b", Exclusive);
        assert!(granted.granted);
        assert!(granted.status.waiters.is_empty());
    }

    #[test]
    fn exclusive_waiters_are_served_oldest_first() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Exclusive, None).granted);
        assert!(!f.wait_lock("b", Exclusive).granted);
        f.advance(Duration::from_secs(1));
        assert!(!f.wait_lock("c", Exclusive).granted);
        f.store.release("main", "a").unwrap();
        let busy = f.wait_lock("c", Exclusive);
        assert!(!busy.granted);
        assert!(busy.reason.unwrap().contains("b has been waiting longer"));
        assert!(f.wait_lock("b", Exclusive).granted);
    }

    #[test]
    fn a_later_exclusive_waiter_does_not_overtake_an_earlier_shared_one() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Exclusive, None).granted);
        assert!(!f.wait_lock("shared", Shared).granted);
        f.advance(Duration::from_secs(1));
        assert!(!f.wait_lock("late", Exclusive).granted);
        f.store.release("main", "a").unwrap();
        let busy = f.wait_lock("late", Exclusive);
        assert!(!busy.granted);
        assert!(busy
            .reason
            .unwrap()
            .contains("shared has been waiting longer"));
        assert!(f.wait_lock("shared", Shared).granted);
    }

    #[test]
    fn reacquiring_renews_the_expiry() {
        let f = Fixture::new();
        let first = f.try_lock("a", Exclusive, None);
        assert!(first.granted && !first.renewed);
        f.advance(Duration::from_secs(300));
        let renewed = f.try_lock("a", Exclusive, None);
        assert!(renewed.granted && renewed.renewed);
        assert_eq!(
            renewed.expires_ms.unwrap(),
            first.expires_ms.unwrap() + 300_000
        );
        assert_eq!(renewed.status.holders.len(), 1);
    }

    #[test]
    fn a_sole_holder_can_switch_modes() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Shared, None).granted);
        let upgraded = f.try_lock("a", Exclusive, None);
        assert!(upgraded.granted && !upgraded.renewed);
        assert_eq!(upgraded.status.holders.len(), 1);
        assert_eq!(upgraded.status.holders[0].holder.mode, Exclusive);
    }

    #[test]
    fn expired_holders_are_ignored_and_pruned() {
        let f = Fixture::new();
        assert!(
            f.store
                .attempt(
                    &AcquireRequest {
                        ttl: Duration::from_secs(5),
                        ..request("main", "a", Exclusive, None)
                    },
                    Attempt::Once
                )
                .unwrap()
                .granted
        );
        assert!(!f.try_lock("b", Exclusive, None).granted);
        f.advance(Duration::from_secs(6));
        let granted = f.try_lock("b", Exclusive, None);
        assert!(granted.granted);
        assert_eq!(granted.status.holders.len(), 1);
        assert_eq!(granted.status.holders[0].holder.owner, "b");
        let events = f.store.log("main", 10).unwrap();
        assert!(events
            .iter()
            .any(|e| e["event"] == "expire" && e["owner"] == "a"));
    }

    #[test]
    fn stale_tickets_stop_blocking() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Exclusive, None).granted);
        assert!(!f.wait_lock("b", Exclusive).granted);
        f.store.release("main", "a").unwrap();
        assert!(!f.try_lock("c", Shared, None).granted);
        // b was killed and stopped refreshing its ticket.
        f.advance(TICKET_STALE + Duration::from_secs(1));
        let granted = f.try_lock("c", Shared, None);
        assert!(granted.granted);
        assert!(granted.status.waiters.is_empty());
    }

    #[test]
    fn giving_up_drops_the_ticket() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Exclusive, None).granted);
        assert_eq!(f.wait_lock("b", Exclusive).status.waiters.len(), 1);
        let gave_up = f.attempt("b", Exclusive, None, Attempt::GiveUp);
        assert!(!gave_up.granted);
        assert!(gave_up.status.waiters.is_empty());
        let events = f.store.log("main", 10).unwrap();
        assert_eq!(events.last().unwrap()["event"], "timeout");
    }

    #[test]
    fn waiting_acquire_returns_when_granted_or_timed_out() {
        let dir = tempfile::tempdir().unwrap();
        let store = LockStore::new(dir.path());
        let holder = request("build", "a", Exclusive, None);
        assert!(
            store
                .acquire(&holder, Duration::ZERO, |_| {})
                .unwrap()
                .granted
        );
        let mut polls = 0;
        let timed_out = store
            .acquire(
                &request("build", "b", Exclusive, None),
                Duration::from_millis(1500),
                |_| polls += 1,
            )
            .unwrap();
        assert!(!timed_out.granted);
        assert!(polls >= 1);
        assert!(timed_out.status.waiters.is_empty());

        let releaser = store.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            releaser.release("build", "a").unwrap();
        });
        let granted = store
            .acquire(
                &request("build", "b", Exclusive, None),
                Duration::from_secs(10),
                |_| {},
            )
            .unwrap();
        handle.join().unwrap();
        assert!(granted.granted);
    }

    #[test]
    fn pause_stops_new_grants_until_resumed() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Shared, None).granted);
        let status = f.store.pause("main", Some("Rojo down")).unwrap();
        assert_eq!(status.paused.unwrap().reason.as_deref(), Some("Rojo down"));
        let busy = f.try_lock("b", Shared, None);
        assert!(!busy.granted);
        assert_eq!(busy.reason.as_deref(), Some("paused: Rojo down"));
        assert!(f.try_lock("a", Shared, None).renewed);
        assert!(f.store.resume("main").unwrap().paused.is_none());
        assert!(f.try_lock("b", Shared, None).granted);
    }

    #[test]
    fn release_is_idempotent() {
        let f = Fixture::new();
        let nothing = f.store.release("main", "a").unwrap();
        assert!(!nothing.released && !nothing.dropped_ticket);
        assert!(f.try_lock("a", Shared, None).granted);
        let released = f.store.release("main", "a").unwrap();
        assert!(released.released);
        assert_eq!(released.mode, Some(Shared));
        assert!(!f.store.release("main", "a").unwrap().released);
    }

    #[test]
    fn release_drops_a_waiters_ticket() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Exclusive, None).granted);
        assert!(!f.wait_lock("b", Exclusive).granted);
        let left = f.store.release("main", "b").unwrap();
        assert!(!left.released && left.dropped_ticket);
        assert!(f.store.status("main").unwrap().waiters.is_empty());
    }

    #[test]
    fn names_are_validated_and_lowercased() {
        for good in ["main", "build", "ui", "heavy-2", "a.b_c", "x"] {
            assert_eq!(validate_name(good).unwrap(), good);
        }
        assert_eq!(validate_name("MAIN").unwrap(), "main");
        for bad in [
            "",
            "a/b",
            "..",
            ".hidden",
            "trailing.",
            "-flag",
            "has space",
            "con",
            "COM1",
            "nul.txt",
            &"a".repeat(65),
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_owner("agent 7").is_ok());
        assert!(validate_owner("").is_err());
        assert!(validate_owner("bad\u{1b}[2J").is_err());
    }

    #[test]
    fn corrupt_state_is_an_error_until_reset() {
        let f = Fixture::new();
        assert!(f.try_lock("a", Exclusive, None).granted);
        let path = f.store.state_path("main");
        fs::write(&path, "{ not json").unwrap();
        let error = f
            .store
            .attempt(&request("main", "b", Exclusive, None), Attempt::Once)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not valid lock state"), "{error}");
        assert!(error.contains("rojo studio lock reset main"), "{error}");
        // The file is left for a person to inspect.
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
        assert!(f.store.reset("main").unwrap());
        assert!(f.try_lock("b", Exclusive, None).granted);
        let events = f.store.log("main", 10).unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["event"] == "reset"
                    && e["detail"].as_str().unwrap().contains("{ not json"))
        );
    }

    #[test]
    fn status_all_lists_every_lock() {
        let f = Fixture::new();
        assert!(f.store.status_all().unwrap().is_empty());
        for name in ["main", "build"] {
            f.store
                .attempt(&request(name, "a", Shared, None), Attempt::Once)
                .unwrap();
        }
        let names: Vec<_> = f
            .store
            .status_all()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["build", "main"]);
    }

    #[test]
    fn the_log_is_bounded() {
        let f = Fixture::new();
        for _ in 0..1200 {
            f.try_lock("a", Shared, None);
            f.store.release("main", "a").unwrap();
        }
        let size = |suffix: &str| {
            fs::metadata(f.store.dir().join(format!("main.log.jsonl{suffix}")))
                .unwrap()
                .len()
        };
        assert!(size("") <= LOG_LIMIT + 1024);
        assert!(size(".1") <= LOG_LIMIT + 1024);
        let last = f.store.log("main", 2).unwrap();
        assert_eq!(last.len(), 2);
        assert_eq!(last[1]["event"], "release");
    }

    #[test]
    fn a_leftover_guard_file_does_not_block() {
        let f = Fixture::new();
        fs::create_dir_all(f.store.dir()).unwrap();
        // A process that was killed mid-update leaves the file, but not the
        // lock, behind.
        fs::write(f.store.dir().join("main.json.lock"), "pid 1\n").unwrap();
        assert!(f.try_lock("a", Shared, None).granted);
    }

    #[test]
    fn a_held_guard_blocks_other_updates_until_released() {
        let f = Fixture::new();
        fs::create_dir_all(f.store.dir()).unwrap();
        let state = f.store.dir().join("main.json");
        let held = StateMutex::acquire(&state).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let waiter = thread::spawn(move || {
            let started = Instant::now();
            let _second = StateMutex::acquire(&state).unwrap();
            sender.send(started.elapsed()).unwrap();
        });
        thread::sleep(Duration::from_millis(200));
        assert!(receiver.try_recv().is_err(), "the second update must wait");
        drop(held);
        let waited = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(waited >= Duration::from_millis(150));
        waiter.join().unwrap();
    }

    #[test]
    fn concurrent_exclusive_acquires_never_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let inside = Arc::new(AtomicBool::new(false));
        let grants = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|index| {
                // A store per thread, as separate processes would have.
                let store = LockStore::new(dir.path());
                let inside = inside.clone();
                let grants = grants.clone();
                thread::spawn(move || {
                    let owner = format!("agent-{index}");
                    for _ in 0..15 {
                        let outcome = store
                            .acquire(
                                &request("build", &owner, Exclusive, None),
                                Duration::ZERO,
                                |_| {},
                            )
                            .unwrap();
                        if outcome.granted {
                            assert!(
                                !inside.swap(true, Ordering::SeqCst),
                                "two exclusive holders at once"
                            );
                            grants.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(2));
                            inside.store(false, Ordering::SeqCst);
                            store.release("build", &owner).unwrap();
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(grants.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn lock_commands_parse_without_a_token_file() {
        use crate::cli::{Options, Subcommand};
        let options = Options::try_parse_from([
            "rojo", "studio", "--json", "lock", "acquire", "heavy", "--owner", "a", "--limit", "2",
            "--wait", "30",
        ])
        .unwrap();
        let Subcommand::Studio(studio) = options.subcommand else {
            panic!("expected a studio command");
        };
        assert!(studio.json && studio.token_file.is_none());
        let crate::cli::studio::StudioSubcommand::Lock(lock) = studio.command else {
            panic!("expected a lock command");
        };
        assert!(matches!(
            lock.command,
            LockSubcommand::Acquire {
                limit: Some(2),
                wait: 30,
                ttl: 600,
                exclusive: false,
                ..
            }
        ));
        assert!(Options::try_parse_from([
            "rojo",
            "studio",
            "lock",
            "acquire",
            "build",
            "--owner",
            "a",
            "--exclusive",
            "--limit",
            "2",
        ])
        .is_err());
        assert!(
            Options::try_parse_from(["rojo", "studio", "lock", "status", "--dir", "x"]).is_ok()
        );
    }
}
