//! Opt-in, authenticated local transport between a CLI and connected Studio plugins.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use hyper::{body::HttpBody, header::HOST, Body, Method, Request, Response, StatusCode};
use rbx_dom_weak::WeakDom;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{oneshot, Notify, Semaphore};
use uuid::Uuid;

use super::{model_check, origin::canonical, untrusted_model, util::json as json_response, verify};
use crate::{cli::studio_backup as backup, serve_session::ServeSession};

pub(super) const MAX_BODY: usize = 8 * 1024 * 1024;
const SESSION_TTL: Duration = Duration::from_secs(30);
const MAX_SESSIONS: usize = 16;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a pending command outlives its timeout. The command handler
/// removes it once its own timeout fires.
const PENDING_GRACE: Duration = Duration::from_secs(1);
/// Checkouts hold `index.lock` only briefly. A lock that stays unchanged for
/// longer is stale (a crashed Git) or held by an interactive command such as
/// `git commit` waiting on an editor, and must not stall sync indefinitely.
const GIT_LOCK_WAIT: Duration = Duration::from_secs(10);
/// Git releases `index.lock` before it rewrites `HEAD` during a checkout, so
/// the lock must stay released for a moment before identity is trusted.
const GIT_LOCK_SETTLE: Duration = Duration::from_millis(200);
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// The longest a request waits for Git in total. Each lock is bounded by
/// [`GIT_LOCK_WAIT`], but a series of Git commands can take one lock after
/// another, and callers' own deadlines must cover this wait.
pub(crate) const GIT_WAIT_LIMIT: Duration = Duration::from_secs(15);
/// The longest an import waits for a check slot and its check together.
pub(crate) const IMPORT_CHECK_LIMIT: Duration = Duration::from_secs(15);
/// The longest the server holds a Studio command request: waiting for Git
/// (15 s), reading the body (10 s), checking an import (15 s) and waiting for
/// Studio (15 s), with a margin. Clients must wait longer than this so they see
/// the server's answer.
pub(crate) const MAX_COMMAND_REQUEST: Duration = Duration::from_secs(60);
/// The longest and the default `timeoutSeconds` of a `spec` command, which
/// runs the place's specs in Run mode.
pub(crate) const MAX_SPEC_SECONDS: u64 = 300;
pub(crate) const DEFAULT_SPEC_SECONDS: u64 = 60;
/// Time a spec run spends outside its specs: Studio starting Run mode, which
/// takes several seconds for a large place, and returning to Edit mode.
const SPEC_RUN_MARGIN: Duration = Duration::from_secs(30);
/// How long a checkout read through Git (for the reftable backend) is reused,
/// so frequent requests do not each start Git processes.
const GIT_OBSERVE_CACHE: Duration = Duration::from_millis(250);
/// Directories Git keeps while a rebase, cherry-pick or revert is in progress.
/// HEAD is detached or moves through rewritten commits until it finishes.
const GIT_OPERATION_DIRS: [&str; 3] = ["rebase-merge", "rebase-apply", "sequencer"];
/// Transient read failures (such as a Windows sharing violation while Git
/// replaces a file) are retried this many times before the result is unknown.
const GIT_READ_ATTEMPTS: usize = 3;
/// Stack size of the thread that decodes a model. Nesting is bounded before
/// decoding, but the decoders recurse once per level and need more than the
/// default stack.
pub(crate) const DECODE_STACK_BYTES: usize = 64 * 1024 * 1024;
/// Bounds concurrent import checks. Each runs a child process that may use
/// up to its memory cap.
const MAX_CONCURRENT_DECODES: usize = 2;
/// How long a cancelled request ID is remembered, so a command still waiting
/// for Git or its import check when it was cancelled is not queued afterwards.
const CANCELLED_TTL: Duration = MAX_COMMAND_REQUEST;
/// Bounds the remembered cancellations.
const MAX_CANCELLED: usize = 1024;
/// The plugin rejects responses over 8MiB, so leave headroom for the envelope.
const MAX_POLL_BYTES: usize = 6 * 1024 * 1024;
/// The largest command a poll can deliver alone without exceeding the
/// plugin's 8MiB response limit, leaving room for the envelope.
const MAX_COMMAND_BYTES: usize = MAX_BODY - 64 * 1024;
/// Bounds memory used by commands queued for one Studio session.
const MAX_SESSION_PENDING_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_MODEL_BYTES: usize = 5 * 1024 * 1024;
const MAX_MODEL_INSTANCES: usize = 10_000;
/// How long after `verify` starts it may begin another `--fix` batch. The CLI
/// waits 65 seconds for the whole request, which before `verify` starts may
/// already have spent up to 15 seconds waiting for Git and 10 reading the
/// body, and the last batch may take its full 15-second deadline: 15 + 10 +
/// 20 + 15 leaves 5 seconds to spare.
const VERIFY_FIX_BUDGET: Duration = Duration::from_secs(20);
/// How long the backups `verify --fix` saves stay pinned if it never unpins
/// them, such as when the server crashes. Far longer than a fix runs.
const BACKUP_PIN: Duration = Duration::from_secs(10 * 60);
/// Bound on the sync summary a plugin sends with each poll.
const MAX_SYNC_SUMMARY_BYTES: usize = 16 * 1024;

/// Why requests and sync stop after the checkout changes. Returning to the
/// accepted branch and commit lets the same server continue. It also fits in a
/// WebSocket close reason (123 bytes).
pub(crate) const IDENTITY_CHANGED: &str = "Project Git checkout changed. Restart rojo serve, or return to the original branch/commit, then reconnect Studio.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub project_path: PathBuf,
    pub workspace_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub server_session: String,
}

fn git_value(path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Runs a Git query that exits with 1 when the answer is "none", such as
/// `symbolic-ref --quiet` on a detached HEAD. Other failures are errors.
fn git_query(path: &Path, args: &[&str]) -> io::Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()?;
    match output.status.code() {
        Some(0) => String::from_utf8(output.stdout)
            .map(|value| Some(value.trim().to_owned()))
            .map_err(|_| io::Error::other("Git printed invalid UTF-8")),
        Some(1) => Ok(None),
        _ => Err(io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

/// Accepts a full SHA-1 or SHA-256 object name. Values read from Git's files
/// are passed to Git as arguments, so anything else (such as a corrupt file
/// starting with `-`) is treated as unreadable.
fn object_id(value: String) -> io::Result<String> {
    if matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Git file does not hold an object name",
        ))
    }
}

/// Reads a small Git file, retrying transient errors. `Ok(None)` means the
/// file does not exist (Git replaces files by renaming, so they never vanish
/// briefly); `Err` means it could not be read, which callers treat as unknown
/// rather than as a change.
fn read_git_file(path: &Path) -> io::Result<Option<String>> {
    let mut attempt = 1;
    loop {
        match fs::read_to_string(path) {
            Ok(contents) => return Ok(Some(contents.trim().to_owned())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if attempt >= GIT_READ_ATTEMPTS => return Err(error),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Finds a ref in the contents of a `packed-refs` file.
fn packed_ref(packed: &str, name: &str) -> Option<String> {
    packed
        .lines()
        .filter(|line| !line.starts_with('#') && !line.starts_with('^'))
        .find_map(|line| {
            let (object, reference) = line.split_once(' ')?;
            (reference.trim() == name).then(|| object.to_owned())
        })
}

/// Reads and validates a token file, warning when other users can read it.
pub(crate) fn read_token_file(path: &Path) -> anyhow::Result<String> {
    let token = fs::read_to_string(path).with_context(|| {
        format!(
            "Could not read Studio authentication token {}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o077 != 0) {
            log::warn!(
                "{} can be read by other users. Restrict it with `chmod 600`.",
                path.display()
            );
        }
    }
    Ok(token.trim().to_owned())
}

/// Parses an `address:port` field of `/proc/net/tcp` or `/proc/net/tcp6`.
/// The kernel prints each 32-bit word of the address in host byte order.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_address(text: &str) -> Option<SocketAddr> {
    let (address, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |index: usize| -> Option<[u8; 4]> {
        let hex = address.get(index * 8..index * 8 + 8)?;
        Some(u32::from_str_radix(hex, 16).ok()?.to_ne_bytes())
    };
    let ip = match address.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(0)?)),
        32 => {
            let mut bytes = [0u8; 16];
            for index in 0..4 {
                bytes[index * 4..index * 4 + 4].copy_from_slice(&word(index)?);
            }
            IpAddr::V6(Ipv6Addr::from(bytes))
        }
        _ => return None,
    };
    Some(SocketAddr::new(canonical(ip), port))
}

/// Finds the user owning the client end of a TCP connection from `peer` to
/// `local` in a `/proc/net/tcp`-format listing.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn socket_owner(listing: &str, peer: SocketAddr, local: SocketAddr) -> Option<u32> {
    let peer = SocketAddr::new(canonical(peer.ip()), peer.port());
    let local = SocketAddr::new(canonical(local.ip()), local.port());
    listing.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let from = parse_proc_address(fields.get(1)?)?;
        let to = parse_proc_address(fields.get(2)?)?;
        if from == peer && to == local {
            fields.get(7)?.parse().ok()
        } else {
            None
        }
    })
}

/// The server-side address of the connection a request arrived on. The HTTP
/// server stores it in the request's extensions.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct LocalAddr(pub SocketAddr);

/// Whether a loopback peer is known to belong to another OS user. Only Linux
/// exposes socket owners without extra dependencies, through `/proc`. Other
/// platforms (and unknown owners) are not distinguished.
async fn foreign_peer(remote: SocketAddr, request: &Request<Body>) -> bool {
    #[cfg(target_os = "linux")]
    if let Some(LocalAddr(local)) = request.extensions().get::<LocalAddr>().copied() {
        // The listings grow with every socket on the machine, so read them
        // off the async workers.
        return tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::MetadataExt;
            let owner = ["/proc/net/tcp", "/proc/net/tcp6"]
                .iter()
                .filter_map(|listing| fs::read_to_string(listing).ok())
                .find_map(|listing| socket_owner(&listing, remote, local));
            // `/proc/self` is owned by this process's effective user.
            let current = fs::metadata("/proc/self")
                .ok()
                .map(|metadata| metadata.uid());
            matches!((owner, current), (Some(owner), Some(current)) if owner != current)
        })
        .await
        .unwrap_or(false);
    }
    let _ = (remote, request);
    false
}

/// Canonical lowercase hyphenated form of a `sessionId` field, so different
/// spellings of one UUID name the same session.
fn session_id(value: &Value) -> Option<String> {
    value
        .get("sessionId")
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
        .map(|id| id.hyphenated().to_string())
}

/// The `timeoutSeconds` of a `spec` command's arguments.
pub(crate) fn spec_seconds(args: &Value) -> Result<u64, String> {
    match args.get("timeoutSeconds") {
        None | Some(Value::Null) => Ok(DEFAULT_SPEC_SECONDS),
        Some(value) => value
            .as_u64()
            .filter(|seconds| (1..=MAX_SPEC_SECONDS).contains(seconds))
            .ok_or_else(|| {
                format!("timeoutSeconds must be a whole number from 1 to {MAX_SPEC_SECONDS}")
            }),
    }
}

/// How long the server waits for Studio to complete a command. A spec run
/// lasts as long as its own timeout allows; every other command gets
/// [`COMMAND_TIMEOUT`].
pub(crate) fn studio_deadline(action: &str, args: &Value) -> Result<Duration, String> {
    if action == "spec" {
        Ok(Duration::from_secs(spec_seconds(args)?) + SPEC_RUN_MARGIN)
    } else {
        Ok(COMMAND_TIMEOUT)
    }
}

/// The longest the server holds a command request whose Studio deadline is
/// `deadline`, the counterpart of [`MAX_COMMAND_REQUEST`] for every action.
pub(crate) fn max_command_request(deadline: Duration) -> Duration {
    MAX_COMMAND_REQUEST - COMMAND_TIMEOUT + deadline
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Validates a native model before it is sent to Studio. The plugin repeats
/// these checks, but the server must not rely on every caller using the CLI.
pub(crate) fn validate_import_model(bytes: &[u8], allow_scripts: bool) -> anyhow::Result<WeakDom> {
    anyhow::ensure!(
        bytes.len() <= MAX_MODEL_BYTES,
        "Import exceeds the 5MiB model limit"
    );
    let tree = if bytes.starts_with(b"<roblox!") {
        untrusted_model::check_binary(bytes, MAX_MODEL_INSTANCES)
            .context("Import is not a valid binary model")?;
        rbx_binary::from_reader(bytes).context("Import is not a valid binary model")?
    } else {
        untrusted_model::check_xml_depth(bytes)?;
        rbx_xml::from_reader(bytes, rbx_xml::DecodeOptions::default())
            .context("Import is not a valid model")?
    };
    anyhow::ensure!(!tree.root().children().is_empty(), "Import model is empty");
    let database = rbx_reflection_database::get()?;
    let script = database.classes.get("LuaSourceContainer");
    let mut count = 0usize;
    for instance in tree.descendants() {
        if instance.referent() == tree.root_ref() {
            continue;
        }
        count += 1;
        anyhow::ensure!(
            count <= MAX_MODEL_INSTANCES,
            "Import exceeds 10,000 instances"
        );
        let descriptor = database
            .classes
            .get(instance.class.as_str())
            .context("Import contains an unknown Roblox class")?;
        anyhow::ensure!(
            instance.class.as_str() != "DataModel"
                && !descriptor.tags.contains(&rbx_reflection::ClassTag::Service),
            "Import cannot contain services or DataModel"
        );
        anyhow::ensure!(
            allow_scripts
                || !script.is_some_and(|script| database.has_superclass(descriptor, script)),
            "Import contains scripts. Pass --allow-scripts to import their source; executable scripts remain disabled."
        );
    }
    Ok(tree)
}

struct Session {
    /// The description exactly as registered, so a retried registration can
    /// be recognised.
    registered: Value,
    description: Value,
    last_seen: Instant,
    queue: VecDeque<String>,
    /// The plugin's latest sync health summary, sent with each poll, and
    /// when it arrived.
    sync: Option<(Value, Instant)>,
}

enum Outcome {
    Completed(Value),
    Cancelled,
}

struct Pending {
    session: String,
    command: Value,
    bytes: usize,
    deadline: Instant,
    /// Distinguishes a request from a later one reusing its caller-chosen ID.
    generation: u64,
    result: oneshot::Sender<Outcome>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Session>,
    pending: HashMap<String, Pending>,
    /// Request IDs cancelled before they were queued, with their session and
    /// when the cancellation lapses.
    cancelled: HashMap<String, (String, Instant)>,
    /// The `resync` batch a running `verify --fix` is waiting on, by the
    /// verify's own request ID, so cancelling the verify cancels it too.
    verify_children: HashMap<String, (String, String)>,
    /// Request IDs of verifies still running, which no other command may use.
    active_verifies: HashSet<String>,
    next_generation: u64,
}

/// Modification times of Git's locks and in-progress operation directories,
/// with `None` for each that is absent.
type LockKey = Vec<Option<Option<SystemTime>>>;

struct LockObservation {
    key: LockKey,
    since: Instant,
    warned: bool,
}

#[derive(Default)]
struct LockState {
    observation: Option<LockObservation>,
    /// When the last observed lock disappeared.
    released: Option<Instant>,
}

/// HEAD's contents and, when HEAD names a branch, that branch's commit.
#[derive(Clone, PartialEq, Eq)]
struct Checkout {
    head: Option<String>,
    tip: Option<String>,
}

struct GitWatch {
    git_dir: PathBuf,
    /// Where branch refs live. Linked worktrees keep only HEAD in `git_dir`.
    common_dir: PathBuf,
    workspace: PathBuf,
    /// HEAD as serve started with it. Set by the first successful read, which
    /// is a later one if Git could not be read at startup.
    initial_head: OnceLock<Option<String>>,
    /// The newest branch commit accepted as a continuation of the history
    /// serve started from.
    accepted_tip: Mutex<Option<String>>,
    lock_wait: Duration,
    /// Whether refs use the reftable backend. Its HEAD file is a placeholder,
    /// so the checkout must be read through Git.
    reftable: bool,
    /// The last checkout read through Git, and when.
    observed: Mutex<Option<(Instant, Checkout)>>,
    /// The last observed checkout and whether it matched. HEAD and the branch
    /// ref only change on checkout, commit or reset, so most requests are two
    /// small file reads.
    verdict: Mutex<Option<(Checkout, bool)>>,
    lock: Mutex<LockState>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere must not permanently disable the bridge or sync.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl GitWatch {
    fn git(&self, args: &[&str]) -> Option<String> {
        git_value(&self.workspace, args)
    }

    /// Whether a Git transaction is in progress. Blocks on file system calls.
    fn busy(&self) -> bool {
        let modified = |path: PathBuf| fs::metadata(path).ok().map(|m| m.modified().ok());
        let key: LockKey = ["index.lock", "HEAD.lock"]
            .iter()
            .chain(&GIT_OPERATION_DIRS)
            .map(|name| modified(self.git_dir.join(name)))
            .chain([modified(self.common_dir.join("reftable/tables.list.lock"))])
            .collect();
        let mut state = lock(&self.lock);
        if key.iter().all(Option::is_none) {
            if state.observation.take().is_some() {
                state.released = Some(Instant::now());
            }
            return state
                .released
                .is_some_and(|released| released.elapsed() < GIT_LOCK_SETTLE);
        }
        match state.observation.as_mut() {
            Some(observed) if observed.key == key => {
                if observed.since.elapsed() < self.lock_wait {
                    return true;
                }
                if !observed.warned {
                    observed.warned = true;
                    log::warn!(
                        "A Git lock or operation in {} has been unchanged for over {} seconds; \
                         it looks stale or is waiting on you, such as a paused rebase. \
                         Continuing to sync.",
                        self.git_dir.display(),
                        self.lock_wait.as_secs()
                    );
                }
                false
            }
            _ => {
                state.observation = Some(LockObservation {
                    key,
                    since: Instant::now(),
                    warned: false,
                });
                true
            }
        }
    }

    /// Resolves a branch ref without running Git in the common case.
    fn read_ref(&self, name: &str) -> io::Result<Option<String>> {
        // The name is passed to Git below, so it must not read as an option.
        if !name.starts_with("refs/") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HEAD does not name a branch",
            ));
        }
        match read_git_file(&self.common_dir.join(name))? {
            Some(value) if !value.starts_with("ref:") => return object_id(value).map(Some),
            Some(_) => {}
            None => {
                if let Some(packed) = read_git_file(&self.common_dir.join("packed-refs"))? {
                    if let Some(object) = packed_ref(&packed, name) {
                        return object_id(object).map(Some);
                    }
                }
                // An unborn branch has no commit yet.
                return Ok(None);
            }
        }
        // Symbolic refs need Git itself.
        self.git(&["rev-parse", "--verify", "--quiet", name])
            .map(Some)
            .ok_or_else(|| io::Error::other("Could not resolve the branch"))
    }

    fn observe(&self) -> io::Result<Checkout> {
        if self.reftable {
            return self.observe_with_git();
        }
        let head = read_git_file(&self.git_dir.join("HEAD"))?;
        let tip = match head.as_deref().and_then(|head| head.strip_prefix("ref:")) {
            Some(name) => self.read_ref(name.trim())?,
            None => head.clone().map(object_id).transpose()?,
        };
        Ok(Checkout { head, tip })
    }

    /// Reads the checkout through Git, for the reftable backend, whose HEAD
    /// file always holds `ref: refs/heads/.invalid`. The result is reused for
    /// a moment, since this starts Git processes.
    fn observe_with_git(&self) -> io::Result<Checkout> {
        if let Some((at, checkout)) = lock(&self.observed).as_ref() {
            if at.elapsed() < GIT_OBSERVE_CACHE {
                return Ok(checkout.clone());
            }
        }
        let resolve = || {
            git_query(
                &self.workspace,
                &["rev-parse", "--verify", "--quiet", "HEAD"],
            )?
            .map(object_id)
            .transpose()
        };
        let checkout = match git_query(&self.workspace, &["symbolic-ref", "--quiet", "HEAD"])? {
            // Written like a HEAD file so both backends compare the same way.
            Some(name) => Checkout {
                head: Some(format!("ref: {name}")),
                tip: resolve()?,
            },
            None => {
                let commit = resolve()?;
                Checkout {
                    head: commit.clone(),
                    tip: commit,
                }
            }
        };
        *lock(&self.observed) = Some((Instant::now(), checkout.clone()));
        Ok(checkout)
    }

    /// Whether a rebase, cherry-pick or revert is in progress. HEAD moves
    /// through intermediate commits until it finishes, so it says nothing
    /// about where the workspace will end up.
    fn operation_in_progress(&self) -> bool {
        GIT_OPERATION_DIRS
            .iter()
            .any(|name| self.git_dir.join(name).is_dir())
    }

    /// Whether `new` continues the history of `old`: a descendant (commits,
    /// merges and fast-forwards) or an amend (a commit with the same parent).
    fn continues(&self, old: &str, new: &str) -> bool {
        let ancestor = Command::new("git")
            .arg("-C")
            .arg(&self.workspace)
            .args(["merge-base", "--is-ancestor", old, new])
            .output()
            .is_ok_and(|output| output.status.success());
        ancestor || {
            let parents = self.git(&["rev-parse", &format!("{old}^"), &format!("{new}^")]);
            parents.is_some_and(|parents| {
                let mut lines = parents.lines();
                matches!((lines.next(), lines.next()), (Some(a), Some(b)) if a == b)
            })
        }
    }

    fn judge(&self, checkout: &Checkout) -> bool {
        let initial = self.initial_head.get_or_init(|| {
            *lock(&self.accepted_tip) = checkout.tip.clone();
            checkout.head.clone()
        });
        match (initial, &checkout.head) {
            (Some(initial), Some(head)) if initial == head => {
                if !initial.starts_with("ref:") {
                    return true;
                }
                let mut accepted = lock(&self.accepted_tip);
                let continues = match (accepted.as_deref(), checkout.tip.as_deref()) {
                    (None, None) => true,
                    (Some(old), Some(new)) if old == new => true,
                    // The first commit on a branch that had none.
                    (None, Some(_)) => true,
                    (Some(old), Some(new)) => self.continues(old, new),
                    // The branch was deleted.
                    (Some(_), None) => false,
                };
                if continues {
                    accepted.clone_from(&checkout.tip);
                }
                continues
            }
            // A detached HEAD may only move forward from where serve started.
            (Some(initial), Some(head))
                if !initial.starts_with("ref:") && !head.starts_with("ref:") =>
            {
                Command::new("git")
                    .arg("-C")
                    .arg(&self.workspace)
                    .args(["merge-base", "--is-ancestor", initial, head])
                    .output()
                    .is_ok_and(|output| output.status.success())
            }
            _ => false,
        }
    }

    /// Whether the checkout still matches, or `None` when it could not be read.
    /// Blocks on file system calls and possibly Git.
    fn matches(&self) -> Option<bool> {
        if self.operation_in_progress() {
            log::debug!(
                "A Git rebase or cherry-pick is in progress; judging the checkout after it"
            );
            return None;
        }
        let checkout = match self.observe() {
            Ok(checkout) => checkout,
            Err(error) => {
                log::debug!("Could not read the Git checkout; assuming it is unchanged: {error}");
                return None;
            }
        };
        if let Some((observed, verdict)) = lock(&self.verdict).as_ref() {
            if *observed == checkout {
                return Some(*verdict);
            }
        }
        let verdict = self.judge(&checkout);
        *lock(&self.verdict) = Some((checkout, verdict));
        Some(verdict)
    }
}

pub struct StudioBridge {
    token: String,
    plugin_token: String,
    identity_json: Value,
    /// An opaque stand-in for the identity, sent before authentication, so
    /// unauthenticated responses do not reveal local paths.
    plugin_identity: Value,
    git: Option<Arc<GitWatch>>,
    /// The tree `verify` compares Studio with.
    serve: Arc<ServeSession>,
    state: Mutex<State>,
    changed: Notify,
    decodes: Arc<Semaphore>,
    /// Where automatic backups go instead of the repository's backup folder.
    /// Only tests set it, so they do not write to the user's own folder.
    backup_dir: OnceLock<PathBuf>,
}

fn reply(status: StatusCode, value: Value) -> Response<Body> {
    json_response(value, status)
}
fn failure(status: StatusCode, message: impl Into<String>) -> Response<Body> {
    reply(status, json!({"error": message.into()}))
}

/// Reads back a response built by [`reply`], for actions made of several
/// Studio commands.
async fn response_value(response: Response<Body>) -> (StatusCode, Value) {
    let status = response.status();
    let value = match hyper::body::to_bytes(response.into_body()).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    (status, value)
}

/// Whether the Host header names the loopback interface literally. A DNS
/// rebinding page reaches the server under its own host name, so this keeps
/// pairing secrets away from it even when Host validation is disabled.
fn loopback_host(request: &Request<Body>) -> bool {
    let Some(host) = request
        .headers()
        .get(HOST)
        .and_then(|host| host.to_str().ok())
    else {
        return false;
    };
    let (name, port) = match host.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((name, port)) => (format!("[{name}]"), port),
            None => return false,
        },
        None => {
            let split = host.find(':').unwrap_or(host.len());
            (host[..split].to_owned(), &host[split..])
        }
    };
    let port_valid = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    port_valid && (name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "[::1]")
}

impl StudioBridge {
    pub fn new(session: &Arc<ServeSession>, token_file: &Path) -> anyhow::Result<Arc<Self>> {
        Self::with_lock_wait(session, token_file, GIT_LOCK_WAIT)
    }

    fn with_lock_wait(
        session: &Arc<ServeSession>,
        token_file: &Path,
        lock_wait: Duration,
    ) -> anyhow::Result<Arc<Self>> {
        let token = read_token_file(token_file)?;
        anyhow::ensure!(
            token.len() >= 32
                && token.len() <= 256
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Studio token must contain 32-256 ASCII letters, numbers or hyphens"
        );
        let root = dunce::canonicalize(session.root_dir())?;
        let project_path = dunce::canonicalize(&session.root_project().file_location)?;
        // Identity is serialized as JSON for every client, which requires UTF-8.
        anyhow::ensure!(
            root.to_str().is_some() && project_path.to_str().is_some(),
            "The Studio bridge requires project and workspace paths that are valid UTF-8"
        );
        let git_dir = git_value(&root, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from);
        let git = git_dir.map(|git_dir| {
            // Older Git prints this relative to the working directory.
            let common_dir = git_value(&root, &["rev-parse", "--git-common-dir"])
                .map_or_else(|| git_dir.clone(), |dir| root.join(dir));
            // `extensions.refStorage = reftable` always comes with this
            // directory, which Git creates when the repository is set up.
            let reftable = common_dir.join("reftable").is_dir();
            let watch = GitWatch {
                initial_head: OnceLock::new(),
                accepted_tip: Mutex::new(None),
                lock_wait,
                reftable,
                observed: Mutex::new(None),
                git_dir,
                common_dir,
                workspace: root.clone(),
                verdict: Mutex::new(None),
                lock: Mutex::new(LockState::default()),
            };
            match watch.observe() {
                Ok(checkout) => {
                    let _ = watch.initial_head.set(checkout.head);
                    *lock(&watch.accepted_tip) = checkout.tip;
                }
                Err(error) => log::warn!(
                    "Could not read the Git checkout at startup ({error}); \
                     sync follows the first checkout that can be read"
                ),
            }
            Arc::new(watch)
        });
        let identity = Identity {
            project_path,
            workspace_path: root.clone(),
            git_head: git_value(&root, &["rev-parse", "HEAD"]),
            git_branch: git_value(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"]),
            server_session: session.session_id().to_string(),
        };
        let identity_json =
            serde_json::to_value(&identity).context("Could not serialize Studio identity")?;
        let plugin_identity = json!({
            "serverSession": identity.server_session,
            "digest": blake3::hash(identity_json.to_string().as_bytes()).to_hex().to_string(),
        });
        Ok(Arc::new(Self {
            token,
            plugin_token: Uuid::new_v4().to_string(),
            identity_json,
            plugin_identity,
            git,
            serve: Arc::clone(session),
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            decodes: Arc::new(Semaphore::new(MAX_CONCURRENT_DECODES)),
            backup_dir: OnceLock::new(),
        }))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Whether a Git transaction (such as a checkout) is in progress or ended
    /// less than [`GIT_LOCK_SETTLE`] ago. Locks held longer than
    /// [`GIT_LOCK_WAIT`] are treated as stale.
    pub async fn git_busy(&self) -> bool {
        let Some(git) = self.git.clone() else {
            return false;
        };
        tokio::task::spawn_blocking(move || git.busy())
            .await
            .unwrap_or(false)
    }

    pub async fn wait_for_git(&self) {
        // Retain the request body while Git is busy. In particular, rejecting a
        // two-way write here would discard a change already flushed by Studio.
        // `git_busy` stops reporting one lock after GIT_LOCK_WAIT, but Git can
        // take a new lock after each, so the total wait has its own bound.
        let started = Instant::now();
        while self.git_busy().await {
            if started.elapsed() >= GIT_WAIT_LIMIT {
                log::debug!("Git stayed busy for {GIT_WAIT_LIMIT:?}; continuing");
                break;
            }
            tokio::time::sleep(GIT_POLL_INTERVAL).await;
        }
    }

    /// Whether the workspace is still on the checkout that serve started from.
    /// Commits, merges, fast-forwards and amends on the starting branch keep
    /// the identity. Switching branches, detaching, moving the branch to a
    /// commit that does not continue its history, or moving a detached HEAD to
    /// a non-descendant does not. A checkout that cannot be read is assumed
    /// unchanged and re-read on the next call.
    ///
    /// Call only after waiting for the index transaction to finish. Staging alone
    /// does not change workspace identity and must not disconnect an idle client.
    pub async fn identity_matches(&self) -> bool {
        let Some(git) = self.git.clone() else {
            return true;
        };
        tokio::task::spawn_blocking(move || git.matches())
            .await
            .ok()
            .flatten()
            .unwrap_or(true)
    }

    pub async fn pairing(&self, remote: SocketAddr, request: &Request<Body>) -> Option<Value> {
        if !canonical(remote.ip()).is_loopback()
            || request.headers().contains_key("origin")
            || !loopback_host(request)
            || foreign_peer(remote, request).await
        {
            return None;
        }
        Some(
            json!({"protocolVersion":1,"pluginToken":self.plugin_token,"identity":self.plugin_identity}),
        )
    }

    fn authorized(&self, request: &Request<Body>, plugin: bool) -> bool {
        let expected = if plugin {
            &self.plugin_token
        } else {
            &self.token
        };
        let Some(given) = request
            .headers()
            .get("x-rojo-studio-token")
            .and_then(|value| value.to_str().ok())
        else {
            return false;
        };
        // Do not expose token-prefix matches through an early-exit comparison.
        given.len() == expected.len()
            && given
                .bytes()
                .zip(expected.bytes())
                .fold(0u8, |different, (a, b)| different | (a ^ b))
                == 0
    }

    fn prune(state: &mut State) {
        let now = Instant::now();
        state
            .sessions
            .retain(|_, session| now.duration_since(session.last_seen) < SESSION_TTL);
        state.pending.retain(|_, pending| {
            pending.deadline > now
                && !pending.result.is_closed()
                && state.sessions.contains_key(&pending.session)
        });
        for session in state.sessions.values_mut() {
            session.queue.retain(|id| state.pending.contains_key(id));
        }
        state.cancelled.retain(|_, (_, until)| *until > now);
    }

    pub async fn call(
        self: Arc<Self>,
        remote: SocketAddr,
        request: Request<Body>,
    ) -> Response<Body> {
        if !canonical(remote.ip()).is_loopback() || request.headers().contains_key("origin") {
            return failure(
                StatusCode::FORBIDDEN,
                "Studio control accepts local clients without an Origin header only",
            );
        }
        if foreign_peer(remote, &request).await {
            return failure(
                StatusCode::FORBIDDEN,
                "Studio control accepts clients run by the same OS user only",
            );
        }
        let path = request.uri().path().to_owned();
        let plugin = matches!(
            path.as_str(),
            "/api/studio/register"
                | "/api/studio/poll"
                | "/api/studio/result"
                | "/api/studio/disconnect"
                | "/api/studio/validate"
                | "/api/studio/backup"
        );
        if !self.authorized(&request, plugin) {
            return failure(StatusCode::UNAUTHORIZED, "Invalid Studio control token");
        }
        self.wait_for_git().await;
        if !self.identity_matches().await {
            return failure(StatusCode::CONFLICT, IDENTITY_CHANGED);
        }
        if request.method() == Method::GET && path == "/api/studio/sessions" {
            let mut state = self.state();
            Self::prune(&mut state);
            let mut sessions: Vec<_> = state
                .sessions
                .values()
                .map(|session| {
                    let mut description = session.description.clone();
                    description["lastSeenMs"] =
                        json!(session.last_seen.elapsed().as_millis() as u64);
                    if let Some((sync, at)) = &session.sync {
                        description["sync"] = sync.clone();
                        description["syncAgeMs"] = json!(at.elapsed().as_millis() as u64);
                    }
                    description
                })
                .collect();
            sessions.sort_by_key(|session| session["registeredAt"].as_u64());
            return reply(
                StatusCode::OK,
                json!({"protocolVersion":1,"identity":self.identity_json,"sessions":sessions}),
            );
        }
        if request.method() != Method::POST {
            return failure(StatusCode::NOT_FOUND, "Unknown Studio control route");
        }
        let value = match read_json(request.into_body()).await {
            Ok(value) => value,
            Err(response) => return *response,
        };
        if plugin && value.get("identity") != Some(&self.plugin_identity) {
            return failure(
                StatusCode::CONFLICT,
                "Studio session is paired to a different project/workspace identity",
            );
        }
        match path.as_str() {
            "/api/studio/register" => self.register(value),
            "/api/studio/disconnect" => {
                let Some(id) = session_id(&value) else {
                    return failure(StatusCode::BAD_REQUEST, "A UUID sessionId is required");
                };
                let mut state = self.state();
                state.sessions.remove(&id);
                Self::prune(&mut state);
                reply(StatusCode::OK, json!({"disconnected":true}))
            }
            "/api/studio/poll" => self.poll(value).await,
            "/api/studio/result" => self.result(value),
            "/api/studio/backup" => self.plugin_backup(value).await,
            "/api/studio/validate" => {
                let mut state = self.state();
                Self::prune(&mut state);
                let session = session_id(&value);
                let active = value
                    .get("requestId")
                    .and_then(Value::as_str)
                    .and_then(|id| state.pending.get(id))
                    .is_some_and(|pending| session.as_deref() == Some(pending.session.as_str()));
                // The plugin cannot poll while Studio is in Run mode, so a long
                // spec run validates its request periodically instead, which
                // must keep the session from expiring. This holds even after
                // the request was cancelled or expired: Studio is still in Run
                // mode and polls again once it ends, so letting the session
                // lapse would take its bridge down. The plugin stops these
                // heartbeats once a run outlasts its watchdog, which is what
                // keeps a Run mode that never ends from holding a dead session.
                if let Some(session) = session.and_then(|id| state.sessions.get_mut(&id)) {
                    session.last_seen = Instant::now();
                }
                if active {
                    reply(StatusCode::OK, json!({"active":true}))
                } else {
                    failure(StatusCode::GONE, "Request expired or was cancelled")
                }
            }
            "/api/studio/cancel" => {
                let Some(id) = value.get("requestId").and_then(Value::as_str) else {
                    return failure(StatusCode::BAD_REQUEST, "requestId is required");
                };
                let mut state = self.state();
                Self::prune(&mut state);
                if state
                    .pending
                    .get(id)
                    .is_some_and(|pending| session_id(&value) == Some(pending.session.clone()))
                {
                    if let Some(pending) = state.pending.remove(id) {
                        let _ = pending.result.send(Outcome::Cancelled);
                    }
                    Self::prune(&mut state);
                    reply(StatusCode::OK, json!({"cancelled":true,"requestId":id}))
                } else if let Some((_, child)) = state
                    .verify_children
                    .get(id)
                    .filter(|(session, _)| session_id(&value).as_ref() == Some(session))
                    .cloned()
                {
                    // A verify past its `sources` step: stop its current batch,
                    // whether it is queued or about to be, and the batches after.
                    let session = session_id(&value).unwrap_or_default();
                    match state.pending.remove(&child) {
                        Some(pending) => {
                            let _ = pending.result.send(Outcome::Cancelled);
                        }
                        None => remember(&mut state.cancelled, &child, session.clone()),
                    }
                    remember(&mut state.cancelled, id, session);
                    Self::prune(&mut state);
                    reply(StatusCode::OK, json!({"cancelled":true,"requestId":id}))
                } else {
                    // The command may still be waiting for Git or its import
                    // check, so it must not be queued once it gets there. A
                    // command that already finished cannot be told apart from
                    // one whose request has not been read yet, so the ID is
                    // refused either way until the cancellation lapses.
                    if let Some(session) = session_id(&value) {
                        remember(&mut state.cancelled, id, session);
                    }
                    failure(
                        StatusCode::NOT_FOUND,
                        "No pending request for this target session. If it is still being prepared, it will not be sent to Studio.",
                    )
                }
            }
            "/api/studio/command" => self.command(value).await,
            _ => failure(StatusCode::NOT_FOUND, "Unknown Studio control route"),
        }
    }

    fn register(&self, value: Value) -> Response<Body> {
        let Some(id) = session_id(&value) else {
            return failure(StatusCode::BAD_REQUEST, "A UUID sessionId is required");
        };
        let id = id.as_str();
        let Some(description) = value.get("description").filter(|value| value.is_object()) else {
            return failure(StatusCode::BAD_REQUEST, "description is required");
        };
        if description.to_string().len() > 16 * 1024 {
            return failure(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Session description exceeds 16KiB",
            );
        }
        let mut state = self.state();
        Self::prune(&mut state);
        if let Some(existing) = state.sessions.get_mut(id) {
            // A retry after a lost response must not fail the plugin's bridge.
            if existing.registered == *description {
                existing.last_seen = Instant::now();
                return reply(StatusCode::OK, json!({"registered":true,"sessionId":id}));
            }
            return failure(
                StatusCode::CONFLICT,
                "This Studio session is already registered",
            );
        }
        if state.sessions.len() >= MAX_SESSIONS {
            // Evict instead of rejecting, so stale sessions or sessions that
            // another local process registered cannot permanently lock a new
            // Studio out. An evicted Studio's next poll fails with 404.
            let oldest = state
                .sessions
                .iter()
                .min_by_key(|(_, session)| session.last_seen)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                log::warn!(
                    "More than {MAX_SESSIONS} Studio sessions registered; evicting {oldest}"
                );
                state.sessions.remove(&oldest);
                Self::prune(&mut state);
            }
        }
        let mut stored = description.clone();
        stored["sessionId"] = json!(id);
        stored["identity"] = self.identity_json.clone();
        stored["registeredAt"] = json!(unix_millis(SystemTime::now()));
        state.sessions.insert(
            id.to_owned(),
            Session {
                registered: description.clone(),
                description: stored,
                last_seen: Instant::now(),
                queue: VecDeque::new(),
                sync: None,
            },
        );
        reply(StatusCode::OK, json!({"registered":true,"sessionId":id}))
    }

    async fn poll(&self, value: Value) -> Response<Body> {
        let Some(id) = session_id(&value) else {
            return failure(StatusCode::BAD_REQUEST, "A UUID sessionId is required");
        };
        let id = id.as_str();
        // Older plugins send no summary. A summary is a few hundred bytes, so
        // anything much larger is not one and is ignored.
        // A summary that is too large replaces the last one with a marker,
        // so `sessions` does not keep showing health that is out of date.
        let sync = value.get("sync").filter(|sync| sync.is_object()).map(|sync| {
            if sync.to_string().len() <= MAX_SYNC_SUMMARY_BYTES {
                sync.clone()
            } else {
                json!({"status":"unknown","error":"The plugin sent a sync summary over 16 KiB, which was ignored"})
            }
        });
        // Leave room for Studio's HTTP queue and background UI scheduling before
        // the plugin's request deadline, even when no agent is issuing commands.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let notification = self.changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            {
                let mut state = self.state();
                Self::prune(&mut state);
                let State {
                    sessions, pending, ..
                } = &mut *state;
                let Some(session) = sessions.get_mut(id) else {
                    return failure(
                        StatusCode::NOT_FOUND,
                        "Studio session expired; reconnect Studio",
                    );
                };
                session.last_seen = Instant::now();
                if let Some(sync) = &sync {
                    session.sync = Some((sync.clone(), Instant::now()));
                }
                // Commands are delivered at most once: a delivered command is
                // never resent, so a lost poll response surfaces as a timeout
                // instead of a duplicated import. Larger batches stay queued for
                // the next poll to keep the response under the plugin's limit.
                let mut commands = Vec::new();
                let mut total = 0;
                while let Some(next) = session.queue.front() {
                    let Some(command) = pending.get(next) else {
                        session.queue.pop_front();
                        continue;
                    };
                    if !commands.is_empty() && total + command.bytes > MAX_POLL_BYTES {
                        break;
                    }
                    total += command.bytes;
                    commands.push(command.command.clone());
                    session.queue.pop_front();
                }
                if !commands.is_empty() {
                    return reply(StatusCode::OK, json!({"commands":commands}));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if tokio::time::timeout(remaining, notification).await.is_err() {
                return reply(StatusCode::OK, json!({"commands":[]}));
            }
        }
    }

    fn result(&self, value: Value) -> Response<Body> {
        let Some(id) = value.get("requestId").and_then(Value::as_str) else {
            return failure(StatusCode::BAD_REQUEST, "requestId is required");
        };
        let mut state = self.state();
        Self::prune(&mut state);
        match state.pending.get(id) {
            Some(pending) if session_id(&value).as_ref() != Some(&pending.session) => {
                return failure(
                    StatusCode::FORBIDDEN,
                    "Result belongs to another Studio session",
                );
            }
            Some(_) => {}
            None => return failure(StatusCode::GONE, "Request expired or was cancelled"),
        }
        if let Some(pending) = state.pending.remove(id) {
            let _ = pending.result.send(Outcome::Completed(
                value
                    .get("result")
                    .cloned()
                    .unwrap_or(json!({"ok":false,"error":"Missing result"})),
            ));
        }
        reply(StatusCode::OK, json!({"accepted":true}))
    }

    async fn command(&self, value: Value) -> Response<Body> {
        let Some(session_id) = session_id(&value) else {
            return failure(
                StatusCode::BAD_REQUEST,
                "An explicit target UUID sessionId is required",
            );
        };
        let session_id = session_id.as_str();
        let Some(action) = value.get("action").and_then(Value::as_str) else {
            return failure(StatusCode::BAD_REQUEST, "action is required");
        };
        // `sources` and `resync` are only sent by `verify`, so callers cannot
        // set script sources to anything but the server's own files.
        if !matches!(
            action,
            "status"
                | "tree"
                | "properties"
                | "selection"
                | "logs"
                | "export"
                | "import"
                | "test"
                | "spec"
                | "verify"
        ) {
            return failure(StatusCode::BAD_REQUEST, "Unsupported action. Arbitrary Luau execution, whole-place save, publish and play control are unavailable through this bridge.");
        }
        let id = match value.get("requestId") {
            Some(Value::String(id)) if Uuid::parse_str(id).is_ok() => id.clone(),
            None => Uuid::new_v4().to_string(),
            _ => return failure(StatusCode::BAD_REQUEST, "requestId must be a UUID"),
        };
        let args = value.get("args").cloned().unwrap_or(json!({}));
        if action == "verify" {
            return self.verify(session_id, id, &args).await;
        }
        self.dispatch(session_id, id, action, args).await
    }

    /// Compares the session's scripts with the server's tree and, with
    /// `fix`, sets those that differ back to their files' contents. See
    /// [`super::verify`].
    async fn verify(&self, session_id: &str, id: String, args: &Value) -> Response<Body> {
        let started = Instant::now();
        // The request ID stays reserved until this verify ends, so another
        // command cannot reuse it in between and take its cancellations.
        {
            let mut state = self.state();
            if state.pending.contains_key(&id) || !state.active_verifies.insert(id.clone()) {
                return failure(StatusCode::CONFLICT, "requestId is already pending");
            }
        }
        let _reservation = VerifyReservation {
            bridge: self,
            id: id.clone(),
        };
        // Callers may reuse a request ID, so the plugin's record of what this
        // verify saw gets an ID of its own.
        let verify_id = Uuid::new_v4().to_string();
        let fix = args.get("fix").and_then(Value::as_bool).unwrap_or(false);
        let include_held = args
            .get("includeHeld")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let list_limit = match args.get("listLimit") {
            None => verify::DEFAULT_LISTED,
            Some(limit) => match limit.as_u64() {
                Some(limit @ 1..) if limit as usize <= verify::MAX_LISTED => limit as usize,
                _ => {
                    return failure(
                        StatusCode::BAD_REQUEST,
                        format!("listLimit must be 1 to {}", verify::MAX_LISTED),
                    )
                }
            },
        };
        let response = self
            // The plugin keeps the sources it hashes under this verify's ID,
            // so its fix compares with exactly what this verify saw.
            .dispatch(
                session_id,
                id.clone(),
                "sources",
                json!({"verifyId": verify_id}),
            )
            .await;
        let (status, value) = response_value(response).await;
        if status != StatusCode::OK || value["result"]["ok"] != json!(true) {
            return reply(status, value);
        }
        let scripts = {
            let tree = self.serve.tree();
            verify::tree_scripts(&tree, self.serve.root_dir())
        };
        let comparison = match verify::compare(scripts, &value["result"]["value"]) {
            Ok(comparison) => comparison,
            Err(error) => {
                return failure(
                    StatusCode::BAD_GATEWAY,
                    format!("Could not compare Studio's scripts: {error}"),
                )
            }
        };
        let list_bytes = match args.get("maxBytes") {
            None => verify::DEFAULT_LIST_BYTES,
            Some(bytes) => match bytes.as_u64() {
                Some(bytes @ 1024..) if bytes as usize <= verify::DEFAULT_LIST_BYTES => {
                    bytes as usize
                }
                _ => {
                    return failure(
                        StatusCode::BAD_REQUEST,
                        format!("maxBytes must be 1024 to {}", verify::DEFAULT_LIST_BYTES),
                    )
                }
            },
        };
        let (mut report, mut list_budget) = comparison.report_with_budget(list_limit, list_bytes);
        if fix {
            let fixes = comparison.fixable(include_held);
            // Studio's copies of the scripts are saved before any is
            // overwritten, so a wrong fix can be put back. Without that
            // backup nothing is changed, unless the caller opted out.
            let backing_up = args.get("backup") != Some(&json!(false)) && !fixes.is_empty();
            // Holds the pins on this verify's backups until it returns.
            let mut _pins = None;
            let (backups, backed_up) = if backing_up {
                match self
                    .back_up_fixes(session_id, &id, &fixes, started, &verify_id)
                    .await
                {
                    Ok(done) => {
                        _pins = Some(done.pins);
                        (done.summaries, done.scripts)
                    }
                    Err(error) => {
                        return failure(
                            StatusCode::BAD_GATEWAY,
                            format!(
                                "Changed nothing: could not back up the {} scripts verify --fix would overwrite: {error}. \
                                 Run it again, or pass --no-backup to fix them without a backup.",
                                fixes.len()
                            ),
                        )
                    }
                }
            } else {
                (Vec::new(), HashSet::new())
            };
            report["backups"] = json!(backups);
            // Only scripts whose Studio copy is in a backup are fixed. One the
            // export left out, such as a script removed from the place since
            // the check, would otherwise be overwritten with no copy kept,
            // and so would one whose backup is gone from the store by now.
            let (to_fix, not_backed_up): (Vec<&verify::Different>, Vec<&verify::Different>) =
                if backing_up {
                    fixes
                        .iter()
                        .partition(|fix| backed_up.contains(&fix.script.id))
                } else {
                    (fixes.clone(), Vec::new())
                };
            let batches = {
                let tree = self.serve.tree();
                verify::resync_batches(&tree, &to_fix)
            };
            let (mut updated, mut failed, mut skipped) = (0, 0, 0);
            let mut errors = Vec::new();
            if !not_backed_up.is_empty() {
                errors.push(format!(
                    "{} scripts were not fixed because they are not in a backup: Studio did not \
                     export them, for example because they were removed from the place, or their \
                     backup is gone; run verify again",
                    not_backed_up.len()
                ));
            }
            let mut remaining: u64 = batches.iter().map(|batch| batch.count as u64).sum();
            for batch in batches {
                // The caller stops waiting after its own timeout, and a fix it
                // no longer hears about must not keep changing Studio.
                if started.elapsed() > VERIFY_FIX_BUDGET {
                    errors.push(format!(
                        "Stopped after {} seconds; run verify --fix again for the rest",
                        VERIFY_FIX_BUDGET.as_secs()
                    ));
                    break;
                }
                let child = Uuid::new_v4().to_string();
                // The caller's request ID only covered `sources`, which has
                // finished, so a cancel for it lands in `cancelled` or, once
                // the batch is registered, cancels the batch. Checking one and
                // registering under the same lock leaves no gap between them.
                if !self.begin_child(&id, session_id, &child) {
                    errors.push("Cancelled before every script was fixed".to_owned());
                    break;
                }
                // Removes the record however this future ends, including when
                // the caller disconnects and it is dropped mid-await.
                let _child_record = ChildRecord {
                    bridge: self,
                    id: id.clone(),
                    child: child.clone(),
                };
                let response = self
                    .dispatch(
                        session_id,
                        child,
                        "resync",
                        json!({"patch": batch.patch, "expect": batch.expect, "includeHeld": include_held, "verifyId": verify_id}),
                    )
                    .await;
                let (status, value) = response_value(response).await;
                let result = &value["result"];
                if status == StatusCode::OK && result["ok"] == json!(true) {
                    updated += result["value"]["updated"].as_u64().unwrap_or(0);
                    failed += result["value"]["failed"].as_u64().unwrap_or(0);
                    skipped += result["value"]["skipped"].as_u64().unwrap_or(0);
                    remaining -= batch.count as u64;
                } else {
                    // Later batches would most likely fail the same way, and
                    // the report must say what is still out of date.
                    errors.push(
                        result["error"]
                            .as_str()
                            .or(value["error"].as_str())
                            .unwrap_or("Studio did not apply the resync")
                            .to_owned(),
                    );
                    break;
                }
            }
            // Listed like the report's other lists, within what is left of
            // the same byte budget, so the report stays within the caller's
            // output limit however many scripts and however long their paths.
            let (not_backed_up_listed, _) = verify::list_scripts(
                not_backed_up.iter().map(|fix| &fix.script),
                list_limit,
                &mut list_budget,
            );
            if not_backed_up_listed.len() < not_backed_up.len() {
                report["truncated"] = json!(true);
            }
            report["fixed"] = json!({
                "attempted": fixes.len(),
                "updated": updated,
                "failed": failed + remaining,
                "skippedChanged": skipped,
                "skippedHeld": if include_held { 0 } else { comparison.held_count() },
                "notBackedUp": not_backed_up.len(),
                "notBackedUpScripts": not_backed_up_listed,
                "errors": errors,
            });
        }
        reply(
            StatusCode::OK,
            json!({"sessionId":session_id,"requestId":id,"result":{"ok":true,"value":report}}),
        )
    }

    /// Has Studio export the scripts in `fixes` as they are now and saves
    /// them as automatic backups, one per group from
    /// [`verify::backup_groups`], each checked to be restorable and pinned
    /// until the returned [`BackupPins`] is dropped. Fails on the first group
    /// that cannot be exported, checked or saved, which also unpins the ones
    /// saved before it. Right before returning, checks that every backup is
    /// still in the store, and leaves out the scripts of any that is not.
    async fn back_up_fixes(
        &self,
        session_id: &str,
        id: &str,
        fixes: &[&verify::Different],
        started: Instant,
        verify_id: &str,
    ) -> Result<FixBackups, String> {
        let mut pins = BackupPins {
            dir: self.backup_dir.get().cloned(),
            root: self.serve.root_dir().to_owned(),
            ids: Vec::new(),
        };
        let mut summaries = Vec::new();
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for group in verify::backup_groups(fixes) {
            if started.elapsed() > VERIFY_FIX_BUDGET {
                return Err(format!(
                    "it took longer than {} seconds",
                    VERIFY_FIX_BUDGET.as_secs()
                ));
            }
            let child = Uuid::new_v4().to_string();
            // Registered like a fix batch, so cancelling the verify stops it.
            if !self.begin_child(id, session_id, &child) {
                return Err("the verify was cancelled".to_owned());
            }
            let _child_record = ChildRecord {
                bridge: self,
                id: id.to_owned(),
                child: child.clone(),
            };
            let ids: Vec<&str> = group.iter().map(|fix| fix.script.id.as_str()).collect();
            // The hash each script had when verify checked it. The plugin
            // leaves out a script whose source no longer matches, which is
            // then not fixed: the fix would be checked against that same
            // source, so the backup must hold it, not a later edit.
            let hashes: serde_json::Map<String, Value> = group
                .iter()
                .map(|fix| (fix.script.id.clone(), json!(fix.studio_hash)))
                .collect();
            let response = self
                .dispatch(
                    session_id,
                    child,
                    "backup",
                    // The plugin compares each script with the exact source
                    // this verify saw, as the fix itself does, not only its
                    // hash, which two sources can share.
                    json!({ "ids": ids, "hashes": hashes, "verifyId": verify_id }),
                )
                .await;
            let (status, value) = response_value(response).await;
            let result = &value["result"];
            if status != StatusCode::OK || result["ok"] != json!(true) {
                let error = result["error"]
                    .as_str()
                    .or(value["error"].as_str())
                    .unwrap_or("Studio did not export the scripts");
                return Err(if error.contains("Unsupported Studio action") {
                    "the connected Rojo plugin cannot export them; run `rojo plugin install` and restart Studio"
                        .to_owned()
                } else {
                    error.to_owned()
                });
            }
            let exported = &result["value"];
            let contents = backup_contents(exported)?;
            // The scripts the export holds, of those asked for. The rest are
            // not fixed.
            let asked: HashSet<&str> = ids.iter().copied().collect();
            let held: Vec<String> = exported["ids"]
                .as_array()
                .ok_or("Studio's export does not say which scripts it holds")?
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| asked.contains(id))
                .map(str::to_owned)
                .collect();
            if contents.is_empty() || held.is_empty() {
                continue;
            }
            let model = backup_model_bytes(exported).map_err(|error| format!("{error:#}"))?;
            self.check_restorable(&model).await?;
            let record = backup::automatic_record(
                "auto-verify-fix",
                "auto: before verify --fix",
                &contents,
                exported["placeId"].as_u64(),
                exported["placeName"].as_str(),
            );
            if !backup::record_fits(&record) {
                return Err(RECORD_TOO_LARGE.to_owned());
            }
            let until = backup::now_millis() + BACKUP_PIN.as_millis() as u64;
            let (record, store_dir) = self
                .save_backup(record, model, Some((until, pins.ids.clone())))
                .await
                .map_err(|error| format!("{error:#}"))?;
            pins.ids.push(record.id.clone());
            summaries.push(backup_summary(&record, &store_dir));
            groups.push((record.id, held));
        }
        // Another process may have removed one since, for example from a
        // pin that expired; its scripts must not be overwritten.
        let ids: Vec<String> = groups.iter().map(|(id, _)| id.clone()).collect();
        let present = self.backups_present(ids).await;
        let scripts = groups
            .into_iter()
            .filter(|(id, _)| present.contains(id))
            .flat_map(|(_, held)| held)
            .collect();
        Ok(FixBackups {
            summaries,
            scripts,
            pins,
        })
    }

    /// Which of `ids` are still in the backup store.
    async fn backups_present(&self, ids: Vec<String>) -> HashSet<String> {
        let dir = self.backup_dir.get().cloned();
        let root = self.serve.root_dir().to_owned();
        tokio::task::spawn_blocking(move || {
            let Ok(store) = Self::backup_store(dir, &root) else {
                return HashSet::new();
            };
            ids.into_iter().filter(|id| store.exists(id)).collect()
        })
        .await
        .unwrap_or_default()
    }

    /// Runs the isolated check `backup restore` runs on `model`, so a backup
    /// that could never be restored is not saved as if it could.
    async fn check_restorable(&self, model: &[u8]) -> Result<(), String> {
        // Exactly what restore runs, re-encoding and its size limit included.
        self.in_check_slot(model.to_vec(), |bytes, remaining| {
            model_check::prepare_restore(bytes, true, remaining)
        })
        .await
        .map(|_| ())
        .map_err(|(_, message)| format!("the backup would not be restorable: {message}"))
    }

    /// Saves a backup the plugin took before its first sync after connecting
    /// changed or removed instances in Studio, such as after the server's
    /// branch changed. It comes with the plugin token like the plugin's other
    /// requests, and only ever adds to the backup folder.
    async fn plugin_backup(&self, value: Value) -> Response<Body> {
        let model = match backup_model_bytes(&value) {
            Ok(model) => model,
            Err(error) => return failure(StatusCode::BAD_REQUEST, format!("{error:#}")),
        };
        let contents = match backup_contents(&value) {
            Ok(contents) if !contents.is_empty() => contents,
            Ok(_) => {
                return failure(
                    StatusCode::BAD_REQUEST,
                    "A backup must hold at least one instance",
                )
            }
            Err(error) => return failure(StatusCode::BAD_REQUEST, error),
        };
        if let Err(error) = self.check_restorable(&model).await {
            return failure(StatusCode::UNPROCESSABLE_ENTITY, error);
        }
        let record = backup::automatic_record(
            "auto-sync",
            "auto: before Rojo synced Studio with the files on connecting",
            &contents,
            value["placeId"].as_u64(),
            value["placeName"].as_str(),
        );
        if !backup::record_fits(&record) {
            return failure(StatusCode::PAYLOAD_TOO_LARGE, RECORD_TOO_LARGE);
        }
        match self.save_backup(record, model, None).await {
            Ok((record, store_dir)) => reply(
                StatusCode::OK,
                json!({ "backup": backup_summary(&record, &store_dir) }),
            ),
            Err(error) => failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not save the backup: {error:#}"),
            ),
        }
    }

    /// The backup folder of the project's repository, which
    /// `rojo studio backup list` reads from any of its worktrees.
    fn backup_store(dir: Option<PathBuf>, root: &Path) -> anyhow::Result<backup::Store> {
        Ok(match dir {
            Some(dir) => backup::Store::in_dir(&dir),
            // The same resolution as `rojo studio backup --project`.
            None => backup::Store::for_project(root)?,
        })
    }

    /// Saves an automatic backup in the project's backup folder, which also
    /// prunes old automatic ones (see [`backup::Store::save_automatic`]).
    ///
    /// Pins it until the time `pin` gives, when given, along with the other
    /// backups the same operation pinned. Returns the record and the folder it
    /// is in, which depends on the project `rojo serve` serves rather than on
    /// where a later `rojo studio backup list` runs.
    async fn save_backup(
        &self,
        record: backup::Record,
        model: Vec<u8>,
        pin: Option<(u64, Vec<String>)>,
    ) -> anyhow::Result<(backup::Record, PathBuf)> {
        let dir = self.backup_dir.get().cloned();
        let root = self.serve.root_dir().to_owned();
        let project = root.clone();
        let (record, store_dir) = tokio::task::spawn_blocking(move || {
            let store = Self::backup_store(dir, &root)?;
            let record = match pin {
                Some((until, own)) => store.save_automatic_pinned(record, &model, until, &own)?,
                None => store.save_automatic(record, &model)?,
            };
            anyhow::Ok((record, store.dir().to_owned()))
        })
        .await
        .context("The backup stopped before it was saved")??;
        log::info!(
            "Saved automatic backup {} in {}. `rojo studio backup list --project {}` lists it.",
            record.id,
            store_dir.display(),
            project.display()
        );
        Ok((record, store_dir))
    }

    /// The backup store this bridge saves automatic backups in.
    #[cfg(test)]
    fn store_for_tests(&self) -> anyhow::Result<backup::Store> {
        Self::backup_store(None, self.serve.root_dir())
    }

    /// Registers `child` as the batch the verify `id` waits on, unless a
    /// cancellation of `id` arrived first, which it then uses up.
    fn begin_child(&self, id: &str, session_id: &str, child: &str) -> bool {
        let mut state = self.state();
        Self::prune(&mut state);
        if state
            .cancelled
            .get(id)
            .is_some_and(|(session, _)| session == session_id)
        {
            state.cancelled.remove(id);
            return false;
        }
        state
            .verify_children
            .insert(id.to_owned(), (session_id.to_owned(), child.to_owned()));
        true
    }

    /// Queues one command for the session and waits for Studio's result.
    async fn dispatch(
        &self,
        session_id: &str,
        id: String,
        action: &str,
        args: Value,
    ) -> Response<Body> {
        let deadline = match studio_deadline(action, &args) {
            Ok(deadline) => deadline,
            Err(message) => return failure(StatusCode::BAD_REQUEST, message),
        };
        if action == "import" {
            // Checking an import is the expensive step, so fail fast when
            // there is no session to deliver it to.
            {
                let mut state = self.state();
                Self::prune(&mut state);
                if !state.sessions.contains_key(session_id) {
                    return failure(
                        StatusCode::NOT_FOUND,
                        "Target Studio session does not exist or expired",
                    );
                }
            }
            if let Err(response) = self.check_import(&args).await {
                return *response;
            }
        }
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            + deadline.as_millis();
        let command =
            json!({"requestId":id,"action":action,"args":args,"expiresAt":expires as u64});
        let bytes = command.to_string().len();
        // A poll always delivers at least one command, so a larger one would
        // exceed the plugin's response limit and be lost.
        if bytes > MAX_COMMAND_BYTES {
            return failure(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Studio command is too large to deliver to the plugin",
            );
        }
        let (sender, receiver) = oneshot::channel();
        let generation;
        {
            let mut state = self.state();
            Self::prune(&mut state);
            if !state.sessions.contains_key(session_id) {
                return failure(
                    StatusCode::NOT_FOUND,
                    "Target Studio session does not exist or expired",
                );
            }
            // A running verify reserves its ID; only its own `sources` step,
            // sent with that ID, may use it.
            if state.pending.contains_key(&id)
                || (action != "sources" && state.active_verifies.contains(&id))
            {
                return failure(StatusCode::CONFLICT, "requestId is already pending");
            }
            if state
                .cancelled
                .get(&id)
                .is_some_and(|(session, _)| session == session_id)
            {
                state.cancelled.remove(&id);
                return reply(
                    StatusCode::GONE,
                    json!({"error":"The Studio command was cancelled before it completed.","cancelled":true,"requestId":id}),
                );
            }
            if state.pending.len() >= 32 {
                return failure(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Studio request queue is full",
                );
            }
            let queued: usize = state
                .pending
                .values()
                .filter(|pending| pending.session == session_id)
                .map(|pending| pending.bytes)
                .sum();
            if queued + bytes > MAX_SESSION_PENDING_BYTES {
                return failure(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Studio session has too much pending command data; wait for earlier commands",
                );
            }
            generation = state.next_generation;
            state.next_generation += 1;
            state.pending.insert(
                id.clone(),
                Pending {
                    session: session_id.to_owned(),
                    command,
                    bytes,
                    // `prune` must not drop this before the timeout below
                    // fires, or the caller would see a disconnect (503)
                    // instead of a timeout (504).
                    deadline: Instant::now() + deadline + PENDING_GRACE,
                    generation,
                    result: sender,
                },
            );
            if let Some(session) = state.sessions.get_mut(session_id) {
                session.queue.push_back(id.clone());
            }
        }
        self.changed.notify_waiters();
        let response = match tokio::time::timeout(deadline, receiver).await {
            Ok(Ok(Outcome::Completed(result))) => reply(StatusCode::OK, json!({"sessionId":session_id,"requestId":id,"result":result})),
            Ok(Ok(Outcome::Cancelled)) => reply(StatusCode::GONE, json!({"error":"The Studio command was cancelled before it completed.","cancelled":true,"requestId":id})),
            Ok(Err(_)) => failure(StatusCode::SERVICE_UNAVAILABLE, "The Studio session disconnected or expired before completing the command. Reconnect Studio and retry."),
            Err(_) => failure(StatusCode::GATEWAY_TIMEOUT, format!("Studio did not complete the command within {} seconds. Reconnect Studio and retry.", deadline.as_secs())),
        };
        let mut state = self.state();
        if state
            .pending
            .get(&id)
            .is_some_and(|pending| pending.generation == generation)
        {
            state.pending.remove(&id);
        }
        response
    }
}

/// Ends a verify's reservation of its request ID when dropped.
struct VerifyReservation<'a> {
    bridge: &'a StudioBridge,
    id: String,
}

impl Drop for VerifyReservation<'_> {
    fn drop(&mut self) {
        self.bridge.state().active_verifies.remove(&self.id);
    }
}

/// Removes a `verify --fix` batch's record from `verify_children` when
/// dropped, if it still names that batch.
struct ChildRecord<'a> {
    bridge: &'a StudioBridge,
    id: String,
    child: String,
}

impl Drop for ChildRecord<'_> {
    fn drop(&mut self) {
        let mut state = self.bridge.state();
        if state
            .verify_children
            .get(&self.id)
            .is_some_and(|(_, child)| *child == self.child)
        {
            state.verify_children.remove(&self.id);
        }
    }
}

/// Records a request ID for [`CANCELLED_TTL`], evicting the entry that lapses
/// first once [`MAX_CANCELLED`] are held.
fn remember(ids: &mut HashMap<String, (String, Instant)>, id: &str, session: String) {
    if ids.len() >= MAX_CANCELLED && !ids.contains_key(id) {
        let oldest = ids
            .iter()
            .min_by_key(|(_, (_, until))| *until)
            .map(|(id, _)| id.clone());
        if let Some(oldest) = oldest {
            ids.remove(&oldest);
        }
    }
    ids.insert(id.to_owned(), (session, Instant::now() + CANCELLED_TTL));
}

impl StudioBridge {
    /// Validates an import's model in a child process (see `model_check`),
    /// within [`IMPORT_CHECK_LIMIT`] including the wait for a check slot.
    async fn check_import(&self, args: &Value) -> Result<(), Box<Response<Body>>> {
        let bad_request = |message: String| Box::new(failure(StatusCode::BAD_REQUEST, message));
        let bytes = import_model_bytes(args).map_err(|error| bad_request(format!("{error:#}")))?;
        let allow_scripts = args.get("allowScripts") == Some(&json!(true));
        self.in_check_slot(bytes, move |bytes, remaining| {
            model_check::check_untrusted_model(bytes, allow_scripts, false, remaining)
        })
        .await
        .map(|_| ())
        .map_err(|(status, message)| Box::new(failure(status, message)))
    }

    /// Runs `check` on `bytes` in a model check slot, within
    /// [`IMPORT_CHECK_LIMIT`] including the wait for the slot, which `check`
    /// is given the rest of.
    async fn in_check_slot(
        &self,
        bytes: Vec<u8>,
        check: impl FnOnce(Vec<u8>, Duration) -> anyhow::Result<Vec<u8>> + Send + 'static,
    ) -> Result<Vec<u8>, (StatusCode, String)> {
        let started = Instant::now();
        let slot = match tokio::time::timeout(
            IMPORT_CHECK_LIMIT,
            Arc::clone(&self.decodes).acquire_owned(),
        )
        .await
        {
            Ok(Ok(slot)) => slot,
            Ok(Err(_)) => {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Import validation stopped".to_owned(),
                ))
            }
            Err(_) => {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Other imports are still being checked. Retry shortly.".to_owned(),
                ))
            }
        };
        let remaining = IMPORT_CHECK_LIMIT.saturating_sub(started.elapsed());
        // The slot is released when the check ends, which its time limit
        // bounds even if this request is abandoned first.
        let checked = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            check(bytes, remaining)
        })
        .await;
        match checked {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(error)) => Err((StatusCode::BAD_REQUEST, format!("{error:#}"))),
            Err(_) => Err((
                StatusCode::BAD_REQUEST,
                "Import model could not be decoded".to_owned(),
            )),
        }
    }
}

fn import_model_bytes(args: &Value) -> anyhow::Result<Vec<u8>> {
    let model = args
        .get("model")
        .and_then(Value::as_str)
        .context("Import requires base64 model bytes")?;
    anyhow::ensure!(
        model.len() <= MAX_MODEL_BYTES.div_ceil(3) * 4,
        "Import exceeds the 5MiB model limit"
    );
    data_encoding::BASE64
        .decode(model.as_bytes())
        .context("Import model is not valid base64")
}

const RECORD_TOO_LARGE: &str =
    "the backup lists too many or too long instance paths for its record, so it was not saved";

/// What `verify --fix` saved, with the scripts the backups hold.
struct FixBackups {
    /// A bounded summary of each backup, for the report.
    summaries: Vec<Value>,
    scripts: HashSet<String>,
    pins: BackupPins,
}

/// Unpins the backups of a `verify --fix` when dropped, however the verify
/// ends. A pin left by a crash expires after [`BACKUP_PIN`].
struct BackupPins {
    dir: Option<PathBuf>,
    root: PathBuf,
    ids: Vec<String>,
}

impl Drop for BackupPins {
    fn drop(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        let unpinned = StudioBridge::backup_store(self.dir.clone(), &self.root)
            .and_then(|store| store.unpin(&self.ids));
        if let Err(error) = unpinned {
            log::warn!(
                "Could not unpin automatic backups, which stay pinned until they expire: {error:#}"
            );
        }
    }
}

/// What a report says about a saved backup: enough to find and restore it,
/// and bounded however many roots it holds, which `backup list` shows.
fn backup_summary(record: &backup::Record, store_dir: &Path) -> Value {
    json!({
        "id": record.id,
        "name": record.name,
        "storeDir": store_dir,
        "automatic": record.automatic,
        "rootCount": record.roots.len(),
        "bytes": record.bytes,
    })
}

/// The model bytes of a backup the plugin sent, within the model limit.
fn backup_model_bytes(value: &Value) -> anyhow::Result<Vec<u8>> {
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .context("A backup requires base64 model bytes")?;
    anyhow::ensure!(
        model.len() <= MAX_MODEL_BYTES.div_ceil(3) * 4,
        "Backup exceeds the 5MiB model limit"
    );
    let bytes = data_encoding::BASE64
        .decode(model.as_bytes())
        .context("Backup model is not valid base64")?;
    anyhow::ensure!(
        bytes.len() <= MAX_MODEL_BYTES,
        "Backup exceeds the 5MiB model limit"
    );
    // `backup restore` checks the whole model; this only keeps out anything
    // that is plainly not one.
    anyhow::ensure!(
        bytes.starts_with(b"<roblox!"),
        "Backup is not a binary Roblox model"
    );
    Ok(bytes)
}

/// The path and class of each instance the plugin says a backup holds.
fn backup_contents(value: &Value) -> Result<Vec<(String, String)>, String> {
    let items = value
        .get("instances")
        .and_then(Value::as_array)
        .ok_or("A backup must list its instances")?;
    if items.len() > MAX_MODEL_INSTANCES {
        return Err(format!(
            "A backup holds at most {MAX_MODEL_INSTANCES} instances"
        ));
    }
    items
        .iter()
        .map(|item| {
            let text = |key: &str, max: usize| {
                item.get(key)
                    .and_then(Value::as_str)
                    .map(|text| backup::clip(text, max).to_owned())
            };
            // Paths stay whole: shortened, two deep instances could be
            // recorded as the same path. A record too large is refused.
            Some((
                item.get("path").and_then(Value::as_str)?.to_owned(),
                text("className", 128)?,
            ))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "A backup lists an instance without a path or class".to_owned())
}

async fn read_json(mut body: Body) -> Result<Value, Box<Response<Body>>> {
    let read = async {
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.map_err(|_| {
                Box::new(failure(StatusCode::BAD_REQUEST, "Could not read request"))
            })?;
            if bytes.len() + chunk.len() > MAX_BODY {
                return Err(Box::new(failure(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Studio request exceeds 8MiB",
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| Box::new(failure(StatusCode::BAD_REQUEST, "Expected a JSON object")))
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .unwrap_or_else(|_| {
            Err(Box::new(failure(
                StatusCode::REQUEST_TIMEOUT,
                "Studio request body timed out",
            )))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::poll;
    use memofs::Vfs;
    use rbx_dom_weak::InstanceBuilder;

    const PEER: &str = "127.0.0.1:1234";

    fn project_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.project.json"),
            r#"{"name":"bridge","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("token"), "0123456789abcdef0123456789abcdef").unwrap();
        dir
    }

    fn bridge_for(dir: &Path, lock_wait: Duration) -> Arc<StudioBridge> {
        let session = Arc::new(ServeSession::new(Vfs::new_default().unwrap(), dir).unwrap());
        let bridge = StudioBridge::with_lock_wait(&session, &dir.join("token"), lock_wait).unwrap();
        // Never the user's own backup folder.
        bridge.backup_dir.set(dir.join("backups")).unwrap();
        bridge
    }

    fn fixture() -> (tempfile::TempDir, Arc<StudioBridge>) {
        let dir = project_dir();
        let bridge = bridge_for(dir.path(), GIT_LOCK_WAIT);
        (dir, bridge)
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Rojo Test",
                "-c",
                "user.email=rojo@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_fixture(lock_wait: Duration) -> (tempfile::TempDir, Arc<StudioBridge>) {
        let dir = project_dir();
        git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        git(dir.path(), &["add", "default.project.json"]);
        git(dir.path(), &["commit", "--quiet", "-m", "initial"]);
        let bridge = bridge_for(dir.path(), lock_wait);
        (dir, bridge)
    }

    fn request(method: &str, path: &str, token: &str, body: Option<Value>) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, "127.0.0.1:34872")
            .header("x-rojo-studio-token", token)
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap()
    }

    async fn into_value(response: Response<Body>) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn cli(bridge: &Arc<StudioBridge>, route: &str, body: Value) -> (StatusCode, Value) {
        let request = request("POST", route, &bridge.token, Some(body));
        into_value(bridge.clone().call(PEER.parse().unwrap(), request).await).await
    }

    async fn sessions(bridge: &Arc<StudioBridge>) -> StatusCode {
        let request = request("GET", "/api/studio/sessions", &bridge.token, None);
        bridge
            .clone()
            .call(PEER.parse().unwrap(), request)
            .await
            .status()
    }

    async fn plugin(
        bridge: &Arc<StudioBridge>,
        route: &str,
        mut body: Value,
    ) -> (StatusCode, Value) {
        body["identity"] = bridge.plugin_identity.clone();
        let request = request("POST", route, &bridge.plugin_token, Some(body));
        into_value(bridge.clone().call(PEER.parse().unwrap(), request).await).await
    }

    async fn register(bridge: &Arc<StudioBridge>) -> String {
        let session = Uuid::new_v4().to_string();
        let (status, _) = plugin(
            bridge,
            "/api/studio/register",
            json!({"sessionId":session,"description":{"placeName":"test"}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        session
    }

    fn command_future(
        bridge: &Arc<StudioBridge>,
        body: Value,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Response<Body>>>> {
        let request = request("POST", "/api/studio/command", &bridge.token, Some(body));
        Box::pin(bridge.clone().call(PEER.parse().unwrap(), request))
    }

    fn model(children: &[&str]) -> String {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let folder = dom.insert(root, InstanceBuilder::new("Folder"));
        for class in children {
            dom.insert(folder, InstanceBuilder::new(*class));
        }
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, &[folder]).unwrap();
        data_encoding::BASE64.encode(&bytes)
    }

    #[tokio::test]
    async fn rejects_missing_token_remote_peers_and_browser_origins() {
        let (_dir, bridge) = fixture();
        for (peer, origin, token, expected) in [
            ("127.0.0.1:1234", None, None, StatusCode::UNAUTHORIZED),
            (
                "192.168.1.2:1234",
                None,
                Some(bridge.token.as_str()),
                StatusCode::FORBIDDEN,
            ),
            (
                "127.0.0.1:1234",
                Some("http://localhost"),
                Some(bridge.token.as_str()),
                StatusCode::FORBIDDEN,
            ),
        ] {
            let mut request = Request::builder().uri("/api/studio/sessions");
            if let Some(token) = token {
                request = request.header("x-rojo-studio-token", token);
            }
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            assert_eq!(
                bridge
                    .clone()
                    .call(peer.parse().unwrap(), request.body(Body::empty()).unwrap())
                    .await
                    .status(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn plugin_token_cannot_issue_commands_or_list_sessions() {
        let (_dir, bridge) = fixture();
        let request = request("GET", "/api/studio/sessions", &bridge.plugin_token, None);
        assert_eq!(
            bridge.call(PEER.parse().unwrap(), request).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn pairing_requires_a_literal_loopback_host_and_hides_paths() {
        let (dir, bridge) = fixture();
        let peer = PEER.parse().unwrap();
        for (host, allowed) in [
            ("localhost:34872", true),
            ("LOCALHOST", true),
            ("127.0.0.1:34872", true),
            ("[::1]:34872", true),
            ("evil.example:34872", false),
            ("127.0.0.1.evil.example", false),
            ("localhost:", false),
            ("[::1", false),
        ] {
            let request = Request::builder()
                .header(HOST, host)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                bridge.pairing(peer, &request).await.is_some(),
                allowed,
                "{host}"
            );
        }
        let request = Request::builder().body(Body::empty()).unwrap();
        assert!(bridge.pairing(peer, &request).await.is_none());
        let request = Request::builder()
            .header(HOST, "localhost:34872")
            .body(Body::empty())
            .unwrap();
        let pairing = bridge.pairing(peer, &request).await.unwrap().to_string();
        let name = dir.path().file_name().unwrap().to_str().unwrap();
        assert!(!pairing.contains(name), "{pairing}");
    }

    #[tokio::test]
    async fn rejects_mismatched_result_ownership() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let other = register(&bridge).await;
        let id = Uuid::new_v4().to_string();
        let mut pending = command_future(
            &bridge,
            json!({"sessionId":session,"action":"status","requestId":id}),
        );
        assert!(poll!(&mut pending).is_pending());
        let (status, _) = plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":id,"sessionId":other,"result":{"ok":true}}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn targets_commands_and_delivers_plugin_results() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let mut pending = command_future(&bridge, json!({"sessionId":session,"action":"status"}));
        assert!(poll!(&mut pending).is_pending());
        let (_, value) = plugin(&bridge, "/api/studio/poll", json!({"sessionId":session})).await;
        let id = value["commands"][0]["requestId"].as_str().unwrap();
        let (status, _) = plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":id,"sessionId":session,"result":{"ok":true,"value":{"mode":"Edit"}}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, value) = into_value(pending.await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["result"]["value"]["mode"], "Edit");
        assert!(bridge.state().pending.is_empty());
    }

    #[test]
    fn spec_runs_get_a_deadline_from_their_own_timeout() {
        assert_eq!(
            studio_deadline("spec", &json!({})).unwrap(),
            Duration::from_secs(DEFAULT_SPEC_SECONDS) + SPEC_RUN_MARGIN
        );
        assert_eq!(
            studio_deadline("spec", &json!({"timeoutSeconds":300})).unwrap(),
            Duration::from_secs(330)
        );
        for invalid in [json!(0), json!(301), json!(1.5), json!("60"), json!(-1)] {
            assert!(
                studio_deadline("spec", &json!({ "timeoutSeconds": invalid })).is_err(),
                "{invalid}"
            );
        }
        // Other actions keep the short deadline, whatever their arguments.
        for action in ["status", "test", "import", "logs"] {
            assert_eq!(
                studio_deadline(action, &json!({"timeoutSeconds":300})).unwrap(),
                COMMAND_TIMEOUT
            );
        }
        assert_eq!(max_command_request(COMMAND_TIMEOUT), MAX_COMMAND_REQUEST);
        assert_eq!(
            max_command_request(Duration::from_secs(330)),
            MAX_COMMAND_REQUEST + Duration::from_secs(315)
        );
    }

    #[tokio::test]
    async fn spec_commands_stay_pending_until_their_longer_deadline() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let id = Uuid::new_v4().to_string();
        let mut pending = command_future(
            &bridge,
            json!({"sessionId":session,"action":"spec","requestId":id,"args":{"timeoutSeconds":120}}),
        );
        assert!(poll!(&mut pending).is_pending());
        let (_, value) = plugin(&bridge, "/api/studio/poll", json!({"sessionId":session})).await;
        let command = &value["commands"][0];
        assert_eq!(command["action"], "spec");
        // The plugin checks expiresAt before and after Run mode, so it must
        // cover the whole run rather than the usual 15 seconds.
        let now = unix_millis(SystemTime::now());
        let expires = command["expiresAt"].as_u64().unwrap();
        assert!(expires >= now + 149_000, "{expires} - {now}");
        assert!(expires <= now + 151_000, "{expires} - {now}");
        {
            let state = bridge.state();
            let entry = &state.pending[&id];
            assert!(entry.deadline > Instant::now() + Duration::from_secs(149));
        }
        let (status, _) = plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":id,"sessionId":session,"result":{"ok":true,"value":{"report":{"status":"passed"}}}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, value) = into_value(pending.await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["result"]["value"]["report"]["status"], "passed");
    }

    #[tokio::test]
    async fn spec_commands_with_invalid_timeouts_are_refused() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let (status, value) = cli(
            &bridge,
            "/api/studio/command",
            json!({"sessionId":session,"action":"spec","args":{"timeoutSeconds":301}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(value["error"].as_str().unwrap().contains("timeoutSeconds"));
        assert!(bridge.state().pending.is_empty());
    }

    #[tokio::test]
    async fn validating_a_request_keeps_its_session_alive() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let id = Uuid::new_v4().to_string();
        let mut pending = command_future(
            &bridge,
            json!({"sessionId":session,"action":"spec","requestId":id}),
        );
        assert!(poll!(&mut pending).is_pending());
        plugin(&bridge, "/api/studio/poll", json!({"sessionId":session})).await;
        // As if Studio had been in Run mode, unable to poll, for 25 seconds.
        bridge.state().sessions.get_mut(&session).unwrap().last_seen =
            Instant::now().checked_sub(Duration::from_secs(25)).unwrap();
        let (status, _) = plugin(
            &bridge,
            "/api/studio/validate",
            json!({"sessionId":session,"requestId":id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(bridge.state().sessions[&session].last_seen.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn validating_a_settled_request_still_keeps_its_session_alive() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        // As if Studio were still in Run mode for a request that has since
        // been cancelled or expired, sending heartbeats until Run mode ends.
        bridge.state().sessions.get_mut(&session).unwrap().last_seen =
            Instant::now().checked_sub(Duration::from_secs(25)).unwrap();
        let (status, _) = plugin(
            &bridge,
            "/api/studio/validate",
            json!({"sessionId":session,"requestId":Uuid::new_v4().to_string()}),
        )
        .await;
        assert_eq!(status, StatusCode::GONE);
        assert!(bridge.state().sessions[&session].last_seen.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn validating_without_the_plugin_token_does_not_refresh_a_session() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        bridge.state().sessions.get_mut(&session).unwrap().last_seen =
            Instant::now().checked_sub(Duration::from_secs(25)).unwrap();
        // Without the plugin token the heartbeat is refused outright.
        let (status, _) = cli(
            &bridge,
            "/api/studio/validate",
            json!({"sessionId":session,"requestId":Uuid::new_v4().to_string()}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(bridge.state().sessions[&session].last_seen.elapsed() >= Duration::from_secs(25));
    }

    #[tokio::test]
    async fn cancelling_a_running_spec_keeps_its_session_registered() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let id = Uuid::new_v4().to_string();
        let mut pending = command_future(
            &bridge,
            json!({"sessionId":session,"action":"spec","requestId":id}),
        );
        assert!(poll!(&mut pending).is_pending());
        let (_, value) = plugin(&bridge, "/api/studio/poll", json!({"sessionId":session})).await;
        assert_eq!(value["commands"][0]["action"], "spec");
        let (status, _) = cli(
            &bridge,
            "/api/studio/cancel",
            json!({"sessionId":session,"requestId":id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = into_value(pending.await).await;
        assert_eq!(status, StatusCode::GONE);
        // Studio stays in Run mode well past the session TTL, heartbeating
        // through validate, which answers 410 for the cancelled request.
        for _ in 0..3 {
            bridge.state().sessions.get_mut(&session).unwrap().last_seen =
                Instant::now().checked_sub(Duration::from_secs(25)).unwrap();
            let (status, _) = plugin(
                &bridge,
                "/api/studio/validate",
                json!({"sessionId":session,"requestId":id}),
            )
            .await;
            assert_eq!(status, StatusCode::GONE);
        }
        // Once Run mode ends the late result is refused, but the bridge keeps
        // polling the same session instead of getting a 404.
        let (status, _) = plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":id,"sessionId":session,"result":{"ok":true,"value":{}}}),
        )
        .await;
        assert_eq!(status, StatusCode::GONE);
        let (status, value) =
            plugin(&bridge, "/api/studio/poll", json!({"sessionId":session})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["commands"], json!([]));
    }

    #[tokio::test]
    async fn registering_again_after_expiry_restores_the_session() {
        let (_dir, bridge) = fixture();
        let session = Uuid::new_v4().to_string();
        let body = json!({"sessionId":session,"description":{"placeName":"test"}});
        assert_eq!(
            plugin(&bridge, "/api/studio/register", body.clone())
                .await
                .0,
            StatusCode::OK
        );
        bridge.state().sessions.get_mut(&session).unwrap().last_seen =
            Instant::now().checked_sub(SESSION_TTL).unwrap();
        let (status, _) = plugin(&bridge, "/api/studio/poll", json!({"sessionId":session})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // The plugin re-registers once with the same ID and description.
        assert_eq!(
            plugin(&bridge, "/api/studio/register", body).await.0,
            StatusCode::OK
        );
        let (status, _) = plugin(&bridge, "/api/studio/poll", json!({"sessionId":session})).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn retried_registration_is_idempotent() {
        let (_dir, bridge) = fixture();
        let session = Uuid::new_v4().to_string();
        let body = json!({"sessionId":session,"description":{"placeName":"test"}});
        for _ in 0..2 {
            assert_eq!(
                plugin(&bridge, "/api/studio/register", body.clone())
                    .await
                    .0,
                StatusCode::OK
            );
        }
        let changed = json!({"sessionId":session,"description":{"placeName":"other"}});
        assert_eq!(
            plugin(&bridge, "/api/studio/register", changed).await.0,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn cancelled_commands_are_removed_before_plugin_commit_validation() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let id = Uuid::new_v4().to_string();
        let mut pending = command_future(
            &bridge,
            json!({"sessionId":session,"action":"import","requestId":id,"args":{"parent":"game/Workspace","model":model(&[])}}),
        );
        // Import validation runs on another thread before the command is queued.
        while !bridge.state().pending.contains_key(&id) {
            assert!(poll!(&mut pending).is_pending());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let (status, _) = cli(
            &bridge,
            "/api/studio/cancel",
            json!({"sessionId":session,"requestId":id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = plugin(
            &bridge,
            "/api/studio/validate",
            json!({"sessionId":session,"requestId":id}),
        )
        .await;
        assert_eq!(status, StatusCode::GONE);
        let (status, value) = into_value(pending.await).await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(value["cancelled"], true);
    }

    #[tokio::test]
    async fn cancelled_command_cleanup_keeps_a_newer_request_with_the_same_id() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let id = Uuid::new_v4().to_string();
        let body = json!({"sessionId":session,"action":"status","requestId":id});
        let mut first = command_future(&bridge, body.clone());
        assert!(poll!(&mut first).is_pending());
        let (status, _) = cli(
            &bridge,
            "/api/studio/cancel",
            json!({"sessionId":session,"requestId":id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Queue the retry before the cancelled caller runs its cleanup.
        let mut second = command_future(&bridge, body);
        assert!(poll!(&mut second).is_pending());
        assert_eq!(first.await.status(), StatusCode::GONE);
        let (_, value) = plugin(&bridge, "/api/studio/poll", json!({"sessionId":session})).await;
        assert_eq!(value["commands"][0]["requestId"], id.as_str());
        let (status, _) = plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":id,"sessionId":session,"result":{"ok":true}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(second.await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn poll_responses_are_capped_and_leave_the_rest_queued() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let padding = "x".repeat(5 * 1024 * 1024 / 2);
        let mut pending = Vec::new();
        for _ in 0..3 {
            let mut command = command_future(
                &bridge,
                json!({"sessionId":session,"action":"tree","args":{"padding":padding}}),
            );
            assert!(poll!(&mut command).is_pending());
            pending.push(command);
        }
        let (_, first) = plugin(&bridge, "/api/studio/poll", json!({"sessionId":session})).await;
        assert_eq!(first["commands"].as_array().unwrap().len(), 2);
        assert!(first.to_string().len() <= MAX_POLL_BYTES + 1024);
        let (_, second) = plugin(&bridge, "/api/studio/poll", json!({"sessionId":session})).await;
        assert_eq!(second["commands"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pending_command_bytes_are_bounded_per_session() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let padding = "x".repeat(7 * 1024 * 1024);
        let mut pending = Vec::new();
        let mut rejected = None;
        for _ in 0..10 {
            let mut command = command_future(
                &bridge,
                json!({"sessionId":session,"action":"tree","args":{"padding":padding}}),
            );
            match poll!(&mut command) {
                std::task::Poll::Pending => pending.push(command),
                std::task::Poll::Ready(response) => {
                    rejected = Some(response.status());
                    break;
                }
            }
        }
        assert_eq!(pending.len(), 9);
        assert_eq!(rejected, Some(StatusCode::TOO_MANY_REQUESTS));
    }

    #[tokio::test]
    async fn commands_too_large_for_one_poll_are_rejected() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        // Fits in a request body, but not in a poll response with its envelope.
        let padding = "x".repeat(MAX_BODY - 1024);
        let (status, _) = cli(
            &bridge,
            "/api/studio/command",
            json!({"sessionId":session,"action":"tree","args":{"padding":padding}}),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(bridge.state().pending.is_empty());
    }

    #[tokio::test]
    async fn import_contents_are_validated_by_the_server() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let mut huge_header = b"<roblox!\x89\xff\r\n\x1a\n\0\0".to_vec();
        huge_header.extend_from_slice(&[0xff; 8]);
        huge_header.extend_from_slice(&[0; 8]);
        let deep_xml = format!(
            "<roblox version=\"4\">{}{}</roblox>",
            "<Item class=\"Folder\">".repeat(100_000),
            "</Item>".repeat(100_000)
        );
        for (args, expected) in [
            (json!({"parent":"game/Workspace"}), "base64 model"),
            (json!({"parent":"game/Workspace","model":"%%%"}), "base64"),
            (
                json!({"parent":"game/Workspace","model":data_encoding::BASE64.encode(b"junk")}),
                "valid model",
            ),
            (
                json!({"parent":"game/Workspace","model":model(&["Script"])}),
                "scripts",
            ),
            (
                json!({"parent":"game/Workspace","model":model(&["ModuleScript"]),"allowScripts":false}),
                "scripts",
            ),
            (
                json!({"parent":"game/Workspace","model":model(&["Workspace"])}),
                "services",
            ),
            // A binary header declaring four billion types and instances, which
            // would make the decoder abort the process allocating for them.
            (
                json!({"parent":"game/Workspace","model":data_encoding::BASE64.encode(&huge_header)}),
                "too many instances",
            ),
            (
                json!({"parent":"game/Workspace","model":data_encoding::BASE64.encode(deep_xml.as_bytes())}),
                "levels deep",
            ),
        ] {
            let (status, value) = cli(
                &bridge,
                "/api/studio/command",
                json!({"sessionId":session,"action":"import","args":args}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
            assert!(
                value["error"].as_str().unwrap().contains(expected),
                "{value}"
            );
        }
        assert!(bridge.state().pending.is_empty());
        let mut accepted = command_future(
            &bridge,
            json!({"sessionId":session,"action":"import","args":{"parent":"game/Workspace","model":model(&["Script"]),"allowScripts":true}}),
        );
        assert!(poll!(&mut accepted).is_pending());
    }

    #[tokio::test]
    async fn bounds_request_bodies_before_parsing() {
        let response = read_json(Body::from(vec![b' '; MAX_BODY + 1]))
            .await
            .err()
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn commits_on_the_same_branch_keep_identity_but_checkouts_do_not() {
        let (dir, bridge) = git_fixture(GIT_LOCK_WAIT);
        assert_eq!(sessions(&bridge).await, StatusCode::OK);
        std::fs::write(dir.path().join("file.txt"), "one").unwrap();
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "--quiet", "-m", "second"]);
        assert_eq!(sessions(&bridge).await, StatusCode::OK);
        git(
            dir.path(),
            &["commit", "--quiet", "--amend", "-m", "amended"],
        );
        assert_eq!(sessions(&bridge).await, StatusCode::OK);
        git(dir.path(), &["checkout", "--quiet", "-b", "other"]);
        assert_eq!(sessions(&bridge).await, StatusCode::CONFLICT);
        git(dir.path(), &["checkout", "--quiet", "main"]);
        assert_eq!(sessions(&bridge).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn reftable_checkouts_are_read_through_git() {
        let dir = project_dir();
        let init = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args([
                "init",
                "--quiet",
                "--ref-format=reftable",
                "--initial-branch=main",
            ])
            .output()
            .unwrap();
        if !init.status.success() {
            eprintln!("Skipping: this Git does not support the reftable backend");
            return;
        }
        git(dir.path(), &["add", "default.project.json"]);
        git(dir.path(), &["commit", "--quiet", "-m", "initial"]);
        let bridge = bridge_for(dir.path(), GIT_LOCK_WAIT);
        assert!(bridge.git.as_ref().unwrap().reftable);
        assert_eq!(sessions(&bridge).await, StatusCode::OK);
        commit(dir.path(), "second");
        tokio::time::sleep(GIT_OBSERVE_CACHE * 2).await;
        assert_eq!(sessions(&bridge).await, StatusCode::OK);
        git(dir.path(), &["checkout", "--quiet", "-b", "other"]);
        tokio::time::sleep(GIT_OBSERVE_CACHE * 2).await;
        assert_eq!(sessions(&bridge).await, StatusCode::CONFLICT);
        git(dir.path(), &["checkout", "--quiet", "main"]);
        tokio::time::sleep(GIT_OBSERVE_CACHE * 2).await;
        assert_eq!(sessions(&bridge).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn rebases_in_progress_are_not_judged_until_they_finish() {
        // A short lock wait, so the paused rebase stops holding requests.
        let (dir, _) = git_fixture(Duration::from_millis(100));
        git(dir.path(), &["checkout", "--quiet", "-b", "side"]);
        std::fs::write(dir.path().join("shared.txt"), "side").unwrap();
        git(dir.path(), &["add", "shared.txt"]);
        git(dir.path(), &["commit", "--quiet", "-m", "side"]);
        git(dir.path(), &["checkout", "--quiet", "main"]);
        std::fs::write(dir.path().join("shared.txt"), "main").unwrap();
        git(dir.path(), &["add", "shared.txt"]);
        git(dir.path(), &["commit", "--quiet", "-m", "main"]);
        git(dir.path(), &["checkout", "--quiet", "side"]);
        let bridge = bridge_for(dir.path(), Duration::from_millis(100));
        assert_eq!(sessions(&bridge).await, StatusCode::OK);

        // The rebase stops on a conflict with HEAD detached.
        let rebase = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args([
                "-c",
                "user.name=Rojo Test",
                "-c",
                "user.email=rojo@example.com",
            ])
            .args(["-c", "commit.gpgsign=false", "rebase", "main"])
            .output()
            .unwrap();
        assert!(!rebase.status.success());
        assert!(dir.path().join(".git/rebase-merge").is_dir());
        assert_eq!(sessions(&bridge).await, StatusCode::OK);

        // Aborting returns to the starting commit.
        git(dir.path(), &["rebase", "--abort"]);
        assert_eq!(sessions(&bridge).await, StatusCode::OK);

        // A finished rebase onto new commits rewrites the branch.
        git(dir.path(), &["rebase", "--quiet", "-X", "theirs", "main"]);
        assert_eq!(sessions(&bridge).await, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn refs_that_are_not_object_names_are_unknown() {
        let (dir, bridge) = git_fixture(GIT_LOCK_WAIT);
        let main = dir.path().join(".git/refs/heads/main");
        assert!(bridge.identity_matches().await);
        let contents = std::fs::read(&main).unwrap();
        // Never passed to Git, where it would read as an option.
        std::fs::write(&main, "--output=pwned").unwrap();
        assert!(bridge.git.as_ref().unwrap().observe().is_err());
        assert!(bridge.identity_matches().await);
        std::fs::write(&main, contents).unwrap();
        assert!(bridge.identity_matches().await);
    }

    #[tokio::test]
    async fn detached_heads_may_only_advance_to_descendants() {
        let dir = project_dir();
        git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        git(dir.path(), &["add", "default.project.json"]);
        git(dir.path(), &["commit", "--quiet", "-m", "initial"]);
        std::fs::write(dir.path().join("file.txt"), "one").unwrap();
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "--quiet", "-m", "second"]);
        git(dir.path(), &["checkout", "--quiet", "--detach", "HEAD~1"]);
        let bridge = bridge_for(dir.path(), GIT_LOCK_WAIT);
        assert!(bridge.identity_matches().await);
        git(dir.path(), &["checkout", "--quiet", "--detach", "main"]);
        assert!(bridge.identity_matches().await);
        git(
            dir.path(),
            &["checkout", "--quiet", "--orphan", "unrelated"],
        );
        git(dir.path(), &["commit", "--quiet", "-m", "unrelated"]);
        git(
            dir.path(),
            &["checkout", "--quiet", "--detach", "unrelated"],
        );
        assert!(!bridge.identity_matches().await);
    }

    #[tokio::test]
    async fn staging_pauses_sync_without_changing_identity() {
        let (dir, bridge) = git_fixture(Duration::from_secs(5));
        assert!(bridge.identity_matches().await);
        let lock = dir.path().join(".git/index.lock");
        std::fs::write(&lock, b"").unwrap();
        assert!(bridge.git_busy().await);
        assert!(bridge.identity_matches().await);
        let waiting = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.wait_for_git().await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished());
        std::fs::remove_file(&lock).unwrap();
        waiting.await.unwrap();
        assert!(bridge.identity_matches().await);
        std::fs::write(dir.path().join(".git/HEAD"), b"ref: refs/heads/other\n").unwrap();
        assert!(!bridge.identity_matches().await);
    }

    #[tokio::test]
    async fn stale_index_locks_stop_blocking_after_the_bound() {
        let lock_wait = Duration::from_millis(300);
        let (dir, bridge) = git_fixture(lock_wait);
        let lock = dir.path().join(".git/index.lock");
        let file = std::fs::File::create(&lock).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), bridge.wait_for_git())
            .await
            .expect("a stale index.lock must not block indefinitely");
        assert!(started.elapsed() >= lock_wait);
        // The same stale lock does not delay later requests again.
        let started = Instant::now();
        bridge.wait_for_git().await;
        assert!(started.elapsed() < lock_wait);
        assert_eq!(sessions(&bridge).await, StatusCode::OK);
        // A new Git transaction is waited on again.
        file.set_modified(SystemTime::now()).unwrap();
        assert!(bridge.git_busy().await);
    }

    fn commit(dir: &Path, name: &str) {
        let file = format!("{name}.txt");
        std::fs::write(dir.join(&file), name).unwrap();
        git(dir, &["add", &file]);
        git(dir, &["commit", "--quiet", "-m", name]);
    }

    #[tokio::test]
    async fn rewriting_the_starting_branch_changes_identity() {
        let (dir, bridge) = git_fixture(GIT_LOCK_WAIT);
        let dir = dir.path();
        git(dir, &["branch", "base"]);
        git(dir, &["checkout", "--quiet", "--orphan", "unrelated"]);
        commit(dir, "unrelated");
        git(dir, &["checkout", "--quiet", "main"]);
        commit(dir, "second");
        git(dir, &["tag", "second"]);
        assert!(bridge.identity_matches().await);

        // reset --hard to an unrelated commit keeps HEAD's text unchanged.
        git(dir, &["reset", "--quiet", "--hard", "unrelated"]);
        assert_eq!(sessions(&bridge).await, StatusCode::CONFLICT);
        git(dir, &["reset", "--quiet", "--hard", "second"]);
        assert_eq!(sessions(&bridge).await, StatusCode::OK);

        // Resetting backwards also abandons history the place was synced from.
        git(dir, &["checkout", "--quiet", "-B", "main", "base"]);
        assert_eq!(sessions(&bridge).await, StatusCode::CONFLICT);
        git(dir, &["checkout", "--quiet", "-B", "main", "second"]);
        assert_eq!(sessions(&bridge).await, StatusCode::OK);

        // Moving the branch from elsewhere (as a forced pull would) is caught.
        git(dir, &["checkout", "--quiet", "--detach"]);
        git(dir, &["branch", "--force", "main", "unrelated"]);
        git(dir, &["checkout", "--quiet", "main"]);
        assert_eq!(sessions(&bridge).await, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn merges_and_amends_continue_packed_branches() {
        let (dir, bridge) = git_fixture(GIT_LOCK_WAIT);
        let dir = dir.path();
        commit(dir, "second");
        // Packed refs have no loose ref file to read.
        git(dir, &["pack-refs", "--all"]);
        assert!(!dir.join(".git/refs/heads/main").exists());
        assert!(bridge.identity_matches().await);
        git(dir, &["checkout", "--quiet", "-b", "feature"]);
        commit(dir, "feature");
        git(dir, &["checkout", "--quiet", "main"]);
        commit(dir, "third");
        git(dir, &["merge", "--quiet", "--no-edit", "feature"]);
        assert!(bridge.identity_matches().await);
        git(
            dir,
            &["commit", "--quiet", "--amend", "-m", "amended merge"],
        );
        assert!(bridge.identity_matches().await);
        git(dir, &["pack-refs", "--all"]);
        assert!(bridge.identity_matches().await);
    }

    #[tokio::test]
    async fn unreadable_heads_are_unknown_rather_than_a_branch_change() {
        let (dir, bridge) = git_fixture(GIT_LOCK_WAIT);
        let head = dir.path().join(".git/HEAD");
        let contents = std::fs::read(&head).unwrap();
        // Reading a directory fails with an error other than NotFound.
        std::fs::remove_file(&head).unwrap();
        std::fs::create_dir(&head).unwrap();
        assert!(bridge.identity_matches().await);
        std::fs::remove_dir(&head).unwrap();
        std::fs::write(&head, contents).unwrap();
        assert!(bridge.identity_matches().await);
        std::fs::write(&head, b"ref: refs/heads/other\n").unwrap();
        assert!(!bridge.identity_matches().await);
    }

    #[tokio::test]
    async fn released_locks_settle_before_identity_is_trusted() {
        let (dir, bridge) = git_fixture(GIT_LOCK_WAIT);
        let head_lock = dir.path().join(".git/HEAD.lock");
        std::fs::write(&head_lock, b"").unwrap();
        assert!(bridge.git_busy().await);
        std::fs::remove_file(&head_lock).unwrap();
        // Git may still be about to replace HEAD.
        assert!(bridge.git_busy().await);
        let started = Instant::now();
        bridge.wait_for_git().await;
        assert!(started.elapsed() >= GIT_LOCK_SETTLE / 2);
        assert!(!bridge.git_busy().await);
    }

    #[tokio::test]
    async fn the_least_recently_seen_session_is_evicted_at_the_cap() {
        let (_dir, bridge) = fixture();
        let mut sessions = Vec::new();
        for _ in 0..MAX_SESSIONS {
            sessions.push(register(&bridge).await);
        }
        // The first session stopped polling a while ago.
        bridge
            .state()
            .sessions
            .get_mut(&sessions[0])
            .unwrap()
            .last_seen = Instant::now() - Duration::from_secs(10);
        let newest = register(&bridge).await;
        {
            let state = bridge.state();
            assert_eq!(state.sessions.len(), MAX_SESSIONS);
            assert!(!state.sessions.contains_key(&sessions[0]));
            assert!(state.sessions.contains_key(&newest));
        }
        let (status, _) = plugin(
            &bridge,
            "/api/studio/poll",
            json!({"sessionId":sessions[0]}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Polls as the plugin until a command arrives, so a spawned request can
    /// make progress in between.
    async fn next_command(bridge: &Arc<StudioBridge>, session: &str) -> Value {
        for _ in 0..50 {
            let (_, value) = plugin(bridge, "/api/studio/poll", json!({"sessionId":session})).await;
            if let Some(command) = value["commands"].as_array().and_then(|c| c.first()) {
                return command.clone();
            }
        }
        panic!("no command arrived");
    }

    #[tokio::test]
    async fn cancelling_a_verify_cancels_its_queued_fix_batch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("default.project.json"),
            r#"{"name":"v","tree":{"$className":"DataModel","ReplicatedStorage":{"Shared":{"$path":"src"}}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/Main.luau"),
            "return 'disk'
",
        )
        .unwrap();
        std::fs::write(dir.path().join("token"), "0123456789abcdef0123456789abcdef").unwrap();
        let bridge = bridge_for(dir.path(), GIT_LOCK_WAIT);
        let session = register(&bridge).await;
        let id = Uuid::new_v4().to_string();
        let request = request(
            "POST",
            "/api/studio/command",
            &bridge.token,
            Some(
                json!({"sessionId":session,"action":"verify","requestId":id,"args":{"fix":true,"backup":false}}),
            ),
        );
        let verify = tokio::spawn(bridge.clone().call(PEER.parse().unwrap(), request));

        let sources = next_command(&bridge, &session).await;
        assert_eq!(sources["action"], "sources");
        let script = {
            let tree = bridge.serve.tree();
            verify::tree_scripts(&tree, bridge.serve.root_dir())
                .into_iter()
                .next()
                .unwrap()
        };
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":sources["requestId"],"sessionId":session,"result":{"ok":true,"value":{
                "scripts":[{"id":script.id,"hash":1,"length":1}],"detached":[],"held":[]}}}),
        )
        .await;
        let resync = next_command(&bridge, &session).await;
        assert_eq!(resync["action"], "resync");
        assert_ne!(resync["requestId"], json!(id));

        // While the verify runs, no other command may take its ID.
        let (status, _) = cli(
            &bridge,
            "/api/studio/command",
            json!({"sessionId":session,"action":"status","requestId":id}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // The verify's own ID is no longer pending, but cancelling it must
        // still stop the batch it is waiting on.
        let (status, _) = cli(
            &bridge,
            "/api/studio/cancel",
            json!({"sessionId":session,"requestId":id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let fixed = &value["result"]["value"]["fixed"];
        assert_eq!(fixed["updated"], 0);
        assert_eq!(fixed["failed"], 1);
        assert!(!fixed["errors"].as_array().unwrap().is_empty());
        // Studio's late answer for the cancelled batch is refused.
        let (status, _) = plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":resync["requestId"],"sessionId":session,"result":{"ok":true,"value":{"updated":1}}}),
        )
        .await;
        assert_eq!(status, StatusCode::GONE);
        assert!(bridge.state().verify_children.is_empty());
        assert!(bridge.state().active_verifies.is_empty());
    }

    /// A project with one script, and a verify --fix with `args` that has
    /// got past its `sources` step, where Studio's copy of the script differs.
    async fn fix_past_sources(
        args: Value,
    ) -> (
        tempfile::TempDir,
        Arc<StudioBridge>,
        String,
        verify::TreeScript,
        tokio::task::JoinHandle<Response<Body>>,
    ) {
        let (dir, bridge, session, mut scripts, verify) =
            fix_scripts_past_sources(args, 1, 1).await;
        (dir, bridge, session, scripts.remove(0), verify)
    }

    /// Like [`fix_past_sources`] with `count` scripts, each `length` bytes
    /// long in Studio.
    async fn fix_scripts_past_sources(
        args: Value,
        count: usize,
        length: usize,
    ) -> (
        tempfile::TempDir,
        Arc<StudioBridge>,
        String,
        Vec<verify::TreeScript>,
        tokio::task::JoinHandle<Response<Body>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("default.project.json"),
            r#"{"name":"v","tree":{"$className":"DataModel","ReplicatedStorage":{"Shared":{"$path":"src"}}}}"#,
        )
        .unwrap();
        for index in 0..count {
            let name = if index == 0 {
                "Main".to_owned()
            } else {
                format!("Main{index}")
            };
            std::fs::write(
                dir.path().join(format!("src/{name}.luau")),
                "return 'disk'\n",
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("token"), "0123456789abcdef0123456789abcdef").unwrap();
        let bridge = bridge_for(dir.path(), GIT_LOCK_WAIT);
        let session = register(&bridge).await;
        let request = request(
            "POST",
            "/api/studio/command",
            &bridge.token,
            Some(
                json!({"sessionId":session,"action":"verify","requestId":Uuid::new_v4().to_string(),"args":args}),
            ),
        );
        let verify = tokio::spawn(bridge.clone().call(PEER.parse().unwrap(), request));
        let sources = next_command(&bridge, &session).await;
        assert_eq!(sources["action"], "sources");
        let scripts = {
            let tree = bridge.serve.tree();
            verify::tree_scripts(&tree, bridge.serve.root_dir())
        };
        assert_eq!(scripts.len(), count);
        let entries: Vec<Value> = scripts
            .iter()
            .map(|script| json!({"id": script.id, "hash": 1, "length": length}))
            .collect();
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":sources["requestId"],"sessionId":session,"result":{"ok":true,"value":{
                "scripts":entries,"detached":[],"held":[]}}}),
        )
        .await;
        (dir, bridge, session, scripts, verify)
    }

    /// Answers a `backup` command as the plugin, exporting `ids`.
    async fn answer_backup(
        bridge: &Arc<StudioBridge>,
        session: &str,
        command: &Value,
        ids: &[&str],
    ) {
        let instances: Vec<Value> = ids
            .iter()
            .map(|id| json!({"path": format!("game/ReplicatedStorage/Shared/{id}"), "className": "ModuleScript"}))
            .collect();
        plugin(
            bridge,
            "/api/studio/result",
            json!({"requestId":command["requestId"],"sessionId":session,"result":{"ok":true,"value":{
                "model":model(&["ModuleScript"]),"instances":instances,"ids":ids,"placeId":5,"placeName":"Place"}}}),
        )
        .await;
    }

    #[tokio::test]
    async fn verify_fix_backs_up_studio_scripts_before_overwriting_them() {
        let (dir, bridge, session, script, verify) = fix_past_sources(json!({"fix":true})).await;
        let backup = next_command(&bridge, &session).await;
        assert_eq!(backup["action"], "backup");
        assert_eq!(backup["args"]["ids"], json!([script.id]));
        // Studio's hash from the check, which `fix_past_sources` reports as 1,
        // and the verify whose exact sources the plugin compares with.
        assert_eq!(backup["args"]["hashes"][&script.id], 1);
        assert!(backup["args"]["verifyId"].is_string());
        // The fix waits for the backup: nothing else is queued yet.
        assert_eq!(bridge.state().pending.len(), 1);
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":backup["requestId"],"sessionId":session,"result":{"ok":true,"value":{
                "model":model(&["ModuleScript"]),
                "instances":[{"path":"game/ReplicatedStorage/Shared/Main","className":"ModuleScript"}],
                "ids":[script.id],"placeId":5,"placeName":"Place"}}}),
        )
        .await;
        let resync = next_command(&bridge, &session).await;
        assert_eq!(resync["action"], "resync");
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":resync["requestId"],"sessionId":session,"result":{"ok":true,"value":{"updated":1,"failed":0,"skipped":0}}}),
        )
        .await;
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let report = &value["result"]["value"];
        assert_eq!(report["fixed"]["updated"], 1);
        let saved = &report["backups"][0];
        assert_eq!(saved["automatic"], true);
        assert_eq!(
            saved["storeDir"],
            json!(dir.path().join("backups")),
            "the report says where the backup is"
        );
        assert_eq!(saved["rootCount"], 1);
        assert!(
            saved.get("roots").is_none(),
            "the report holds only a summary"
        );
        // The fix is over, so its backup is no longer pinned.
        assert!(!dir
            .path()
            .join("backups")
            .join(format!("{}.pin", saved["id"].as_str().unwrap()))
            .exists());
        let listed = backup::Store::in_dir(&dir.path().join("backups"))
            .list()
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(json!(listed[0].id), saved["id"]);
        assert_eq!(listed[0].instance, "game/ReplicatedStorage/Shared/Main");
        assert_eq!(listed[0].place_id, Some(5));
        assert!(listed[0]
            .note
            .as_deref()
            .unwrap()
            .starts_with("auto: before verify --fix"));
    }

    #[tokio::test]
    async fn verify_fix_changes_nothing_when_the_backup_fails() {
        for (error, expected) in [
            ("Export exceeds 5MiB", "Export exceeds 5MiB"),
            (
                "StudioBridge:1070: Unsupported Studio action. Inspect capabilities for the precise availability contract.",
                "rojo plugin install",
            ),
        ] {
            let (dir, bridge, session, _, verify) = fix_past_sources(json!({"fix":true})).await;
            let backup = next_command(&bridge, &session).await;
            assert_eq!(backup["action"], "backup");
            plugin(
                &bridge,
                "/api/studio/result",
                json!({"requestId":backup["requestId"],"sessionId":session,"result":{"ok":false,"error":error}}),
            )
            .await;
            let (status, value) = into_value(verify.await.unwrap()).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
            let message = value["error"].as_str().unwrap();
            assert!(message.starts_with("Changed nothing"), "{message}");
            assert!(message.contains(expected), "{message}");
            assert!(message.contains("--no-backup"), "{message}");
            // No resync was ever queued, and nothing was saved.
            let state = bridge.state();
            assert!(state.pending.is_empty());
            assert!(state.sessions[&session].queue.is_empty());
            assert!(state.verify_children.is_empty());
            drop(state);
            assert!(backup::Store::in_dir(&dir.path().join("backups"))
                .list()
                .unwrap()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn verify_fix_with_a_model_that_is_not_one_changes_nothing() {
        let (_dir, bridge, session, script, verify) = fix_past_sources(json!({"fix":true})).await;
        let backup = next_command(&bridge, &session).await;
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":backup["requestId"],"sessionId":session,"result":{"ok":true,"value":{
                "model":data_encoding::BASE64.encode(b"not a model"),
                "instances":[{"path":"game/ReplicatedStorage/Shared/Main","className":"ModuleScript"}],
                "ids":[script.id]}}}),
        )
        .await;
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
        assert!(bridge.state().pending.is_empty());
    }

    #[tokio::test]
    async fn verify_fix_only_fixes_scripts_the_backup_holds() {
        let (_dir, bridge, session, scripts, verify) =
            fix_scripts_past_sources(json!({"fix":true}), 2, 1).await;
        let backup = next_command(&bridge, &session).await;
        assert_eq!(backup["action"], "backup");
        assert_eq!(backup["args"]["ids"].as_array().unwrap().len(), 2);
        // The second script left the place after the check, so Studio did not
        // export it.
        answer_backup(&bridge, &session, &backup, &[&scripts[0].id]).await;
        let resync = next_command(&bridge, &session).await;
        assert_eq!(resync["action"], "resync");
        let updated = resync["args"]["patch"]["updated"].as_array().unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0]["id"], json!(scripts[0].id));
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":resync["requestId"],"sessionId":session,"result":{"ok":true,"value":{"updated":1,"failed":0,"skipped":0}}}),
        )
        .await;
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let fixed = &value["result"]["value"]["fixed"];
        assert_eq!(fixed["updated"], 1);
        assert_eq!(fixed["notBackedUp"], 1);
        assert_eq!(fixed["notBackedUpScripts"][0]["id"], json!(scripts[1].id));
        assert!(fixed["errors"][0]
            .as_str()
            .unwrap()
            .contains("not fixed because they are not in a backup"));
    }

    #[tokio::test]
    async fn verify_fix_leaves_scripts_whose_source_changed_since_the_check_unfixed() {
        // Studio has a newer edit than the one verify hashed, so the plugin
        // refuses to export it, and the script must not be overwritten.
        let (_dir, bridge, session, scripts, verify) =
            fix_scripts_past_sources(json!({"fix":true}), 2, 1).await;
        let backup = next_command(&bridge, &session).await;
        assert_eq!(backup["args"]["hashes"].as_object().unwrap().len(), 2);
        answer_backup(&bridge, &session, &backup, &[&scripts[1].id]).await;
        let resync = next_command(&bridge, &session).await;
        let updated = resync["args"]["patch"]["updated"].as_array().unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0]["id"], json!(scripts[1].id));
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":resync["requestId"],"sessionId":session,"result":{"ok":true,"value":{"updated":1,"failed":0,"skipped":0}}}),
        )
        .await;
        let (_, value) = into_value(verify.await.unwrap()).await;
        let fixed = &value["result"]["value"]["fixed"];
        assert_eq!(fixed["notBackedUp"], 1);
        assert_eq!(fixed["notBackedUpScripts"][0]["id"], json!(scripts[0].id));
    }

    #[tokio::test]
    async fn a_verify_that_fails_part_way_still_leaves_the_store_pruned() {
        let (dir, bridge, session, _scripts, verify) =
            fix_scripts_past_sources(json!({"fix":true}), 2, verify::MAX_BACKUP_SOURCE_BYTES).await;
        let store = backup::Store::in_dir(&dir.path().join("backups"));
        // Old automatic backups, more than are kept, left by earlier runs.
        for index in 0..backup::KEEP_AUTOMATIC as u64 + 5 {
            let mut record = backup::automatic_record(
                "auto-sync",
                "auto: old",
                &[("game/Old".to_owned(), "Folder".to_owned())],
                None,
                None,
            );
            record.created_at = 1_000 + index;
            store.save(record, b"<roblox!old").unwrap();
        }
        let first = next_command(&bridge, &session).await;
        let ids: Vec<String> = first["args"]["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        answer_backup(&bridge, &session, &first, &refs).await;
        let second = next_command(&bridge, &session).await;
        assert_eq!(second["action"], "backup");
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":second["requestId"],"sessionId":session,"result":{"ok":false,"error":"Export exceeds 5MiB"}}),
        )
        .await;
        let (status, _) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let automatic: Vec<backup::Record> = store
            .list()
            .unwrap()
            .into_iter()
            .filter(|record| record.automatic)
            .collect();
        assert_eq!(automatic.len(), backup::KEEP_AUTOMATIC);
        assert!(automatic[0].name.starts_with("auto-verify-fix"));
    }

    #[test]
    fn the_server_and_backup_project_open_the_same_store() {
        for git_repository in [false, true] {
            let (dir, bridge) = if git_repository {
                git_fixture(GIT_LOCK_WAIT)
            } else {
                fixture()
            };
            // Only folders are resolved here; nothing is written.
            let server = bridge.store_for_tests().unwrap();
            let by_folder = backup::Store::for_project(dir.path()).unwrap();
            let by_file =
                backup::Store::for_project(&dir.path().join("default.project.json")).unwrap();
            assert_eq!(server.dir(), by_folder.dir());
            assert_eq!(server.dir(), by_file.dir());
        }
        assert!(backup::Store::for_project(Path::new("/does/not/exist")).is_err());
    }

    #[tokio::test]
    async fn verify_fix_with_an_empty_export_fixes_nothing() {
        let (_dir, bridge, session, _script, verify) = fix_past_sources(json!({"fix":true})).await;
        let backup = next_command(&bridge, &session).await;
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":backup["requestId"],"sessionId":session,"result":{"ok":true,"value":{
                "instances":[],"ids":[],"missing":1}}}),
        )
        .await;
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let fixed = &value["result"]["value"]["fixed"];
        assert_eq!(fixed["updated"], 0);
        assert_eq!(fixed["notBackedUp"], 1);
        assert_eq!(value["result"]["value"]["backups"], json!([]));
        // No resync was sent.
        assert!(bridge.state().pending.is_empty());
    }

    /// Answers a `backup` command as the plugin with `value`.
    async fn answer_export(
        bridge: &Arc<StudioBridge>,
        session: &str,
        command: &Value,
        value: Value,
    ) {
        plugin(
            bridge,
            "/api/studio/result",
            json!({"requestId":command["requestId"],"sessionId":session,"result":{"ok":true,"value":value}}),
        )
        .await;
    }

    fn command_ids(command: &Value) -> Vec<String> {
        command["args"]["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect()
    }

    /// A model small enough to send but with a chunk larger than restoring
    /// accepts once decompressed, as a script of repeated text makes.
    fn unrestorable_model() -> String {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let script = dom.insert(
            root,
            InstanceBuilder::new("ModuleScript")
                .with_property("Source", "a".repeat(65 * 1024 * 1024)),
        );
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, &[script]).unwrap();
        assert!(bytes.len() <= MAX_MODEL_BYTES, "{} bytes", bytes.len());
        data_encoding::BASE64.encode(&bytes)
    }

    #[tokio::test]
    async fn backups_that_could_not_be_restored_are_not_saved() {
        let unrestorable = unrestorable_model();
        // The sync route refuses it, so the plugin warns and syncs anyway.
        let (dir, bridge) = fixture();
        let (status, value) = plugin(
            &bridge,
            "/api/studio/backup",
            json!({"model": unrestorable, "instances": [{"path":"game/Workspace/S","className":"ModuleScript"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{value}");
        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("not be restorable"));
        assert!(backup::Store::in_dir(&dir.path().join("backups"))
            .list()
            .unwrap()
            .is_empty());

        // verify --fix changes nothing.
        let (dir, bridge, session, script, verify) = fix_past_sources(json!({"fix":true})).await;
        let command = next_command(&bridge, &session).await;
        answer_export(
            &bridge,
            &session,
            &command,
            json!({"model": unrestorable, "ids": [script.id],
                "instances": [{"path":"game/ReplicatedStorage/Shared/Main","className":"ModuleScript"}]}),
        )
        .await;
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("not be restorable"),
            "{value}"
        );
        assert!(bridge.state().pending.is_empty(), "no resync was sent");
        assert!(backup::Store::in_dir(&dir.path().join("backups"))
            .list()
            .unwrap()
            .is_empty());
    }

    /// A small zstd model of 100 scripts with long repeated sources, which
    /// passes the import check but grows past 5 MiB when restore re-encodes
    /// it.
    fn too_large_once_reencoded() -> String {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let folder = dom.insert(root, InstanceBuilder::new("Folder"));
        // The same 100 KB of hexadecimal noise in every script: zstd's long
        // window sees the repeats, while LZ4's 64 KB window, which re-encoding
        // uses, does not.
        let mut state: u32 = 0x1234_5678;
        let noise: String = (0..100 * 1024)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                char::from_digit((state >> 28) & 0xf, 16).unwrap()
            })
            .collect();
        for index in 0..100 {
            dom.insert(
                folder,
                InstanceBuilder::new("ModuleScript")
                    .with_name(format!("Module{index}"))
                    .with_property("Source", format!("-- {index}\n{noise}")),
            );
        }
        let mut bytes = Vec::new();
        rbx_binary::Serializer::new()
            .compression_type(rbx_binary::CompressionType::Zstd)
            .serialize(&mut bytes, &dom, &[folder])
            .unwrap();
        assert!(bytes.len() < 256 * 1024, "{} bytes", bytes.len());
        // It passes the check that does not re-encode, and fails the one
        // restore runs.
        assert!(
            model_check::check_untrusted_model(bytes.clone(), true, false, IMPORT_CHECK_LIMIT)
                .is_ok()
        );
        let error =
            model_check::prepare_restore(bytes.clone(), true, IMPORT_CHECK_LIMIT).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds 5MiB"), "{error:#}");
        data_encoding::BASE64.encode(&bytes)
    }

    #[tokio::test]
    async fn backups_that_restore_would_refuse_after_reencoding_are_not_saved() {
        let model = too_large_once_reencoded();
        let (dir, bridge) = fixture();
        let (status, value) = plugin(
            &bridge,
            "/api/studio/backup",
            json!({"model": model, "instances": [{"path":"game/Workspace/Folder","className":"Folder"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{value}");
        assert!(
            value["error"].as_str().unwrap().contains("exceeds 5MiB"),
            "{value}"
        );
        assert!(backup::Store::in_dir(&dir.path().join("backups"))
            .list()
            .unwrap()
            .is_empty());

        let (dir, bridge, session, script, verify) = fix_past_sources(json!({"fix":true})).await;
        let command = next_command(&bridge, &session).await;
        answer_export(
            &bridge,
            &session,
            &command,
            json!({"model": model, "ids": [script.id],
                "instances": [{"path":"game/ReplicatedStorage/Shared/Main","className":"Folder"}]}),
        )
        .await;
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
        let error = value["error"].as_str().unwrap();
        assert!(
            error.starts_with("Changed nothing") && error.contains("exceeds 5MiB"),
            "{error}"
        );
        assert!(bridge.state().pending.is_empty(), "no resync was sent");
        assert!(backup::Store::in_dir(&dir.path().join("backups"))
            .list()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn verify_fix_does_not_fix_scripts_whose_backup_is_gone() {
        let (dir, bridge, session, scripts, verify) =
            fix_scripts_past_sources(json!({"fix":true}), 2, verify::MAX_BACKUP_SOURCE_BYTES).await;
        let store = backup::Store::in_dir(&dir.path().join("backups"));
        let first = next_command(&bridge, &session).await;
        let first_ids = command_ids(&first);
        let refs: Vec<&str> = first_ids.iter().map(String::as_str).collect();
        answer_backup(&bridge, &session, &first, &refs).await;
        let second = next_command(&bridge, &session).await;
        // Something removes the first backup while the second is exported.
        let saved = store.list().unwrap();
        assert_eq!(saved.len(), 1);
        assert!(dir
            .path()
            .join("backups")
            .join(format!("{}.pin", saved[0].id))
            .exists());
        store.remove(&saved[0].id).unwrap();
        let second_ids = command_ids(&second);
        let refs: Vec<&str> = second_ids.iter().map(String::as_str).collect();
        answer_backup(&bridge, &session, &second, &refs).await;
        let resync = next_command(&bridge, &session).await;
        let updated = resync["args"]["patch"]["updated"].as_array().unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0]["id"], json!(second_ids[0]));
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":resync["requestId"],"sessionId":session,"result":{"ok":true,"value":{"updated":1,"failed":0,"skipped":0}}}),
        )
        .await;
        let (_, value) = into_value(verify.await.unwrap()).await;
        let fixed = &value["result"]["value"]["fixed"];
        assert_eq!(fixed["notBackedUp"], 1);
        assert_eq!(fixed["notBackedUpScripts"][0]["id"], json!(first_ids[0]));
        assert_eq!(scripts.len(), 2);
    }

    #[tokio::test]
    async fn verify_fix_with_more_groups_than_the_hard_cap_keeps_all_of_them() {
        let groups = backup::MAX_AUTOMATIC + 1;
        let (dir, bridge, session, _scripts, verify) =
            fix_scripts_past_sources(json!({"fix":true}), groups, verify::MAX_BACKUP_SOURCE_BYTES)
                .await;
        let backups = dir.path().join("backups");
        let store = backup::Store::in_dir(&backups);
        for _ in 0..groups {
            let command = next_command(&bridge, &session).await;
            assert_eq!(command["action"], "backup");
            let ids = command_ids(&command);
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            answer_backup(&bridge, &session, &command, &refs).await;
        }
        let resync = next_command(&bridge, &session).await;
        assert_eq!(resync["action"], "resync");
        // Over the hard cap, yet every group is still there and pinned
        // before any script is overwritten.
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), groups);
        assert!(listed
            .iter()
            .all(|record| backups.join(format!("{}.pin", record.id)).exists()));
        assert_eq!(
            resync["args"]["patch"]["updated"].as_array().unwrap().len(),
            groups
        );
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":resync["requestId"],"sessionId":session,"result":{"ok":true,"value":{"updated":groups,"failed":0,"skipped":0}}}),
        )
        .await;
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(
            value["result"]["value"]["backups"]
                .as_array()
                .unwrap()
                .len(),
            groups
        );
        // The verify is over, so its pins are gone and the next prune brings
        // the folder back under the cap.
        assert!(store
            .list()
            .unwrap()
            .iter()
            .all(|record| !backups.join(format!("{}.pin", record.id)).exists()));
        let mut record = backup::automatic_record(
            "auto-sync",
            "auto: later",
            &[("game/Later".to_owned(), "Folder".to_owned())],
            None,
            None,
        );
        record.created_at = backup::now_millis();
        store.save_automatic(record, b"<roblox!later").unwrap();
        assert_eq!(store.list().unwrap().len(), backup::MAX_AUTOMATIC);
    }

    #[tokio::test]
    async fn the_verify_report_lists_a_summary_of_each_backup() {
        let (_dir, bridge, session, script, verify) = fix_past_sources(json!({"fix":true})).await;
        let command = next_command(&bridge, &session).await;
        let instances: Vec<Value> = (0..2_000)
            .map(|index| json!({"path": format!("game/ServerScriptService/Feature{index}/Module{}", "x".repeat(150)), "className": "ModuleScript"}))
            .collect();
        answer_export(
            &bridge,
            &session,
            &command,
            json!({"model": model(&["ModuleScript"]), "ids": [script.id], "instances": instances}),
        )
        .await;
        let resync = next_command(&bridge, &session).await;
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":resync["requestId"],"sessionId":session,"result":{"ok":true,"value":{"updated":1,"failed":0,"skipped":0}}}),
        )
        .await;
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let backups = &value["result"]["value"]["backups"];
        assert_eq!(backups[0]["rootCount"], 2_000);
        assert!(backups.to_string().len() < 1024, "{backups}");
        assert!(value.to_string().len() < 16 * 1024);
    }

    #[tokio::test]
    async fn verify_fix_with_more_backup_groups_than_kept_keeps_all_of_its_own() {
        let groups = backup::KEEP_AUTOMATIC + 3;
        // Each script's Studio source fills a backup group by itself.
        let (dir, bridge, session, scripts, verify) =
            fix_scripts_past_sources(json!({"fix":true}), groups, verify::MAX_BACKUP_SOURCE_BYTES)
                .await;
        let store = backup::Store::in_dir(&dir.path().join("backups"));
        // Older automatic backups, which make way for this verify's.
        for index in 0..3 {
            let mut record = backup::automatic_record(
                "auto-sync",
                "auto: old",
                &[("game/Old".to_owned(), "Folder".to_owned())],
                None,
                None,
            );
            record.created_at = 1_000 + index;
            store.save_automatic(record, b"<roblox!old").unwrap();
        }
        let mut manual_record = backup::automatic_record(
            "manual",
            "kept",
            &[("game/M".to_owned(), "Folder".to_owned())],
            None,
            None,
        );
        manual_record.automatic = false;
        manual_record.created_at = 500;
        let manual = store.save(manual_record, b"<roblox!manual").unwrap();

        let mut answered = 0;
        while answered < groups {
            let command = next_command(&bridge, &session).await;
            assert_eq!(
                command["action"], "backup",
                "every backup comes before any fix"
            );
            let ids: Vec<String> = command["args"]["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(ids.len(), 1);
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            answer_backup(&bridge, &session, &command, &refs).await;
            answered += 1;
        }
        let resync = next_command(&bridge, &session).await;
        assert_eq!(resync["action"], "resync");
        // Before any fix runs, every backup this verify saved is still there,
        // though there are more than are kept: they are young.
        let listed = store.list().unwrap();
        assert_eq!(listed.iter().filter(|r| r.automatic).count(), groups);
        assert!(listed.iter().any(|r| r.id == manual.id));
        plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":resync["requestId"],"sessionId":session,"result":{"ok":true,"value":{"updated":groups,"failed":0,"skipped":0}}}),
        )
        .await;
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let reported: HashSet<String> = value["result"]["value"]["backups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(reported.len(), groups);
        let left: HashSet<String> = store
            .list()
            .unwrap()
            .into_iter()
            .filter(|r| r.automatic)
            .map(|r| r.id)
            .collect();
        assert_eq!(
            left, reported,
            "the older automatic ones were pruned, not these"
        );
        assert_eq!(value["result"]["value"]["fixed"]["notBackedUp"], 0);
        assert_eq!(scripts.len(), groups);
    }

    #[tokio::test]
    async fn plugin_backups_are_saved_as_automatic_with_the_plugin_token_only() {
        let (dir, bridge) = fixture();
        let body = json!({
            "model": model(&["Part"]),
            "instances": [{"path":"game/Workspace/Folder","className":"Folder"}],
            "placeId": 9,
            "placeName": "Place",
        });
        // The CLI's token is not the plugin's.
        let (status, _) = cli(&bridge, "/api/studio/backup", body.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // Nor is a plugin paired with another server.
        let mut foreign = body.clone();
        foreign["identity"] = json!({"serverSession":"other","digest":"other"});
        let request = request(
            "POST",
            "/api/studio/backup",
            &bridge.plugin_token,
            Some(foreign),
        );
        let (status, _) =
            into_value(bridge.clone().call(PEER.parse().unwrap(), request).await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let store = backup::Store::in_dir(&dir.path().join("backups"));
        assert!(store.list().unwrap().is_empty());

        // The bridge does not have to be registered: the backup is taken
        // before the plugin starts it.
        let (status, value) = plugin(&bridge, "/api/studio/backup", body).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["backup"]["automatic"], true);
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].instance, "game/Workspace/Folder");
        assert_eq!(listed[0].place_name.as_deref(), Some("Place"));
        assert!(listed[0]
            .note
            .as_deref()
            .unwrap()
            .starts_with("auto: before Rojo synced Studio"));
    }

    #[tokio::test]
    async fn backups_keep_long_root_paths_whole_and_refuse_records_too_large() {
        let (dir, bridge) = fixture();
        let store = backup::Store::in_dir(&dir.path().join("backups"));
        let deep = "Nested/".repeat(200);
        let first = format!("game/Workspace/{deep}A");
        let second = format!("game/Workspace/{deep}B");
        assert!(first.len() > 1024);
        let (status, value) = plugin(
            &bridge,
            "/api/studio/backup",
            json!({"model": model(&["Part"]), "instances": [
                {"path": first, "className": "Part"},
                {"path": second, "className": "Part"},
            ]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let (record, _) = store.get(value["backup"]["id"].as_str().unwrap()).unwrap();
        assert_eq!(record.roots[0].path, first);
        assert_eq!(record.roots[1].path, second);

        // 10,000 roots with long paths make a record over the 4 MiB limit.
        let long = "x".repeat(500);
        let instances: Vec<Value> = (0..10_000)
            .map(|index| json!({"path": format!("game/Workspace/{long}{index}"), "className": "Part"}))
            .collect();
        let (status, value) = plugin(
            &bridge,
            "/api/studio/backup",
            json!({"model": model(&["Part"]), "instances": instances.clone()}),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{value}");
        assert_eq!(store.list().unwrap().len(), 1, "nothing more was saved");

        // verify --fix changes nothing when its backup's record is too large.
        let (dir, bridge, session, script, verify) = fix_past_sources(json!({"fix":true})).await;
        let command = next_command(&bridge, &session).await;
        answer_export(
            &bridge,
            &session,
            &command,
            json!({"model": model(&["ModuleScript"]), "ids": [script.id], "instances": instances}),
        )
        .await;
        let (status, value) = into_value(verify.await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{value}");
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("too many or too long"),
            "{value}"
        );
        assert!(bridge.state().pending.is_empty(), "no resync was sent");
        assert!(backup::Store::in_dir(&dir.path().join("backups"))
            .list()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn plugin_backups_outside_the_limits_are_refused() {
        let (dir, bridge) = fixture();
        let instances = json!([{"path":"game/Workspace/Folder","className":"Folder"}]);
        let too_large = "A".repeat(MAX_MODEL_BYTES.div_ceil(3) * 4 + 4);
        for body in [
            json!({"instances": instances}),
            json!({"model": "not base64!", "instances": instances}),
            json!({"model": data_encoding::BASE64.encode(b"hello"), "instances": instances}),
            json!({"model": too_large, "instances": instances}),
            json!({"model": model(&[]), "instances": []}),
            json!({"model": model(&[]), "instances": [{"path": "game/Workspace"}]}),
        ] {
            let (status, value) = plugin(&bridge, "/api/studio/backup", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        }
        assert!(backup::Store::in_dir(&dir.path().join("backups"))
            .list()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_cancel_that_arrives_before_queueing_stops_the_command() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let other = register(&bridge).await;
        let id = Uuid::new_v4().to_string();
        // The command is still waiting for Git or its import check.
        let (status, _) = cli(
            &bridge,
            "/api/studio/cancel",
            json!({"sessionId":session,"requestId":id}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Another session's command with the same ID is unaffected.
        let mut unrelated = command_future(
            &bridge,
            json!({"sessionId":other,"action":"status","requestId":id}),
        );
        assert!(poll!(&mut unrelated).is_pending());
        drop(unrelated);
        let (status, value) = cli(
            &bridge,
            "/api/studio/command",
            json!({"sessionId":session,"action":"status","requestId":id}),
        )
        .await;
        assert_eq!(status, StatusCode::GONE, "{value}");
        assert_eq!(value["cancelled"], true);
        assert!(bridge
            .state()
            .pending
            .values()
            .all(|pending| pending.session != session));
        // The cancellation is used up, so the ID can be retried.
        let mut retry = command_future(
            &bridge,
            json!({"sessionId":session,"action":"status","requestId":id}),
        );
        assert!(poll!(&mut retry).is_pending());
    }

    #[tokio::test]
    async fn imports_for_unknown_sessions_fail_before_their_check() {
        let (_dir, bridge) = fixture();
        let (status, value) = cli(
            &bridge,
            "/api/studio/command",
            json!({"sessionId":Uuid::new_v4().to_string(),"action":"import","args":{"parent":"game/Workspace","model":"not base64"}}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{value}");
    }

    #[tokio::test]
    async fn a_checkout_unreadable_at_startup_is_adopted_on_first_read() {
        let (dir, mut bridge) = git_fixture(GIT_LOCK_WAIT);
        // As if the startup read had failed.
        let watch = Arc::get_mut(&mut bridge)
            .unwrap()
            .git
            .as_mut()
            .and_then(Arc::get_mut)
            .unwrap();
        watch.initial_head = OnceLock::new();
        *lock(&watch.accepted_tip) = None;
        *lock(&watch.verdict) = None;
        assert_eq!(sessions(&bridge).await, StatusCode::OK);
        git(dir.path(), &["checkout", "--quiet", "-b", "other"]);
        assert_eq!(sessions(&bridge).await, StatusCode::CONFLICT);
        git(dir.path(), &["checkout", "--quiet", "main"]);
        assert_eq!(sessions(&bridge).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn sessions_report_registration_time_and_activity() {
        let (_dir, bridge) = fixture();
        let session = register(&bridge).await;
        let request = request("GET", "/api/studio/sessions", &bridge.token, None);
        let (_, value) =
            into_value(bridge.clone().call(PEER.parse().unwrap(), request).await).await;
        let listed = &value["sessions"][0];
        assert_eq!(listed["sessionId"], session.as_str());
        assert!(listed["registeredAt"].as_u64().unwrap() > 0);
        assert!(listed["lastSeenMs"].is_u64());
    }

    #[tokio::test]
    async fn session_ids_are_canonicalized() {
        let (_dir, bridge) = fixture();
        let session = Uuid::new_v4();
        let upper = session.hyphenated().to_string().to_uppercase();
        let (status, value) = plugin(
            &bridge,
            "/api/studio/register",
            json!({"sessionId":upper,"description":{"placeName":"test"}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["sessionId"], session.hyphenated().to_string());
        let simple = session.simple().to_string();
        let mut pending = command_future(&bridge, json!({"sessionId":simple,"action":"status"}));
        assert!(poll!(&mut pending).is_pending());
        let braced = session.braced().to_string();
        let (_, value) = plugin(&bridge, "/api/studio/poll", json!({"sessionId":braced})).await;
        let id = value["commands"][0]["requestId"].as_str().unwrap();
        let (status, _) = plugin(
            &bridge,
            "/api/studio/result",
            json!({"requestId":id,"sessionId":upper,"result":{"ok":true}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(pending.await.status(), StatusCode::OK);
    }

    #[test]
    fn packed_refs_are_resolved() {
        let packed = "# pack-refs with: peeled fully-peeled sorted\n\
                      1111111111111111111111111111111111111111 refs/heads/main\n\
                      2222222222222222222222222222222222222222 refs/tags/v1\n\
                      ^3333333333333333333333333333333333333333\n";
        assert_eq!(
            packed_ref(packed, "refs/heads/main").as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(packed_ref(packed, "refs/heads/other"), None);
    }

    // `/proc/net/tcp` prints address words in host byte order, so this sample
    // is what a little-endian kernel prints.
    #[cfg(target_endian = "little")]
    #[test]
    fn proc_net_tcp_owners_are_matched_by_both_endpoints() {
        let listing = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
           0: 0100007F:8828 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1\n\
           1: 0100007F:D431 0100007F:8828 01 00000000:00000000 00:00000000 00000000  1001        0 2 1\n\
           2: 0100007F:8828 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 3 1\n";
        let server: SocketAddr = "127.0.0.1:34856".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:54321".parse().unwrap();
        assert_eq!(socket_owner(listing, peer, server), Some(1001));
        assert_eq!(socket_owner(listing, server, peer), Some(1000));
        let other: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(socket_owner(listing, other, server), None);

        let listing6 = "  sl  local_address                         remote_address                        st\n\
           0: 0000000000000000FFFF00000100007F:D431 0000000000000000FFFF00000100007F:8828 01 00000000:00000000 00:00000000 00000000  1002        0 4 1\n\
           1: 00000000000000000000000001000000:D432 00000000000000000000000001000000:8828 01 00000000:00000000 00:00000000 00000000  1003        0 5 1\n";
        // An IPv4 client of a dual-stack socket appears as an IPv4-mapped peer.
        assert_eq!(socket_owner(listing6, peer, server), Some(1002));
        let peer6: SocketAddr = "[::1]:54322".parse().unwrap();
        let server6: SocketAddr = "[::1]:34856".parse().unwrap();
        assert_eq!(socket_owner(listing6, peer6, server6), Some(1003));
    }
}
