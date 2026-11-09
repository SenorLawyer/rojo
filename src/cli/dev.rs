//! `rojo dev`: the daily loop in one process. It runs doctor's checks once,
//! picks a port that suits the worktree, serves with the Studio bridge on,
//! keeps `sourcemap.json` current for luau-lsp, and records where it listens
//! so `rojo studio`, `rojo test` and `rojo mcp` in the same worktree find the
//! server and its token without flags.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    mem,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::Context;
use clap::Parser;
use memofs::Vfs;
use serde::{Deserialize, Serialize};

use crate::{serve_session::ServeSession, web::LiveServer};

use super::{
    doctor::{self, Category, Status},
    lifecycle, resolve_path,
    serve::{DEFAULT_BIND_ADDRESS, DEFAULT_PORT},
    sourcemap, studio, ExitStatus,
};

/// The file in a worktree's state folder that says where its `rojo dev`
/// listens. It lives outside the project, like the locks and backups, so
/// `rojo dev` never syncs it into the place.
const STATE_FILE: &str = "dev.json";
/// The Studio bridge token `rojo dev` creates, in the repository's shared
/// state folder, so one token serves every worktree and one `rojo mcp`
/// configuration works in all of them.
const TOKEN_FILE: &str = "studio-token";
const SOURCEMAP_FILE: &str = "sourcemap.json";
/// The address `rojo studio`, `rojo test` and `rojo mcp` use when neither a
/// flag nor a running `rojo dev` says otherwise.
pub(super) const DEFAULT_STUDIO_ADDRESS: &str = "http://127.0.0.1:34872";
/// How long confirming a recorded `rojo dev` may take: longer than the
/// bridge holds a request while Git is busy.
const VERIFY_TIMEOUT: Duration =
    Duration::from_secs(crate::web::studio::GIT_WAIT_LIMIT.as_secs() + 5);
/// The error for a Studio command that found no token file.
pub(super) const MISSING_TOKEN: &str = "Pass --token-file with the secret file used by rojo serve --studio-token-file, or start `rojo dev`, which creates one these commands find on their own";
/// Linked worktrees get ports from this range, starting at a slot chosen by
/// their path, so the same worktree tends to get the same port and Studio's
/// plugin keeps working after a restart.
const WORKTREE_PORT_FIRST: u16 = DEFAULT_PORT + 1;
const WORKTREE_PORTS: u16 = 64;

/// Serve the project with the Studio bridge, keep sourcemap.json current and
/// print where everything is. One command for `rojo serve`, `rojo sourcemap
/// --watch` and `rojo doctor`. Stop it with Ctrl+C.
#[derive(Debug, Parser)]
pub struct DevCommand {
    /// Path to the project to serve. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// The IP address to listen on. Defaults to the project's `serveAddress`,
    /// or `127.0.0.1`.
    #[clap(long)]
    pub address: Option<IpAddr>,

    /// The port to listen on. Defaults to the project's `servePort`; without
    /// one, 34872 in a repository's main checkout and a port picked from the
    /// worktree's path in a linked worktree, or the next free one.
    #[clap(long)]
    pub port: Option<u16>,

    /// Extra `Host`/`Origin` values the server accepts, as for `rojo serve`.
    #[clap(long, value_delimiter = ',')]
    pub allowed_hosts: Vec<String>,

    /// Token file for the Studio bridge. Defaults to one in the repository's
    /// per-user state folder, created when missing, which `rojo studio`,
    /// `rojo test` and `rojo mcp` find on their own.
    #[clap(long)]
    pub studio_token_file: Option<PathBuf>,

    /// Do not write or update sourcemap.json.
    #[clap(long)]
    pub no_sourcemap: bool,

    /// Start even when doctor's checks find errors.
    #[clap(long)]
    pub force: bool,

    /// Check this plugins folder instead of discovering Studio's.
    #[clap(long)]
    pub plugins_dir: Option<PathBuf>,
}

impl DevCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let project_path = resolve_path(&self.project)?.into_owned();

        let report = doctor::preflight(&project_path, self.plugins_dir.clone());
        let problems = doctor::render_problems(&report);
        if !problems.is_empty() {
            println!("{problems}");
        }
        if !report.ok() && !self.force {
            eprintln!(
                "rojo dev did not start because of the error(s) above. Fix them, or pass --force to start anyway."
            );
            return Err(ExitStatus(1).into());
        }

        let vfs = Vfs::new_default()?;
        let session = Arc::new(ServeSession::new(vfs, &project_path)?);

        let checkout = Checkout::discover(session.root_dir()).unwrap_or_else(|err| {
            log::warn!(
                "{err:#}\nrojo dev keeps its state next to this project instead, where `rojo studio` run elsewhere in the repository will not find it."
            );
            Checkout::outside_git(session.root_dir())
        });

        let ip = self
            .address
            .or_else(|| session.serve_address())
            .unwrap_or(DEFAULT_BIND_ADDRESS.into());
        // Bound now and held until the server starts, so the setup below
        // cannot lose the port to another launch. The companion holds the
        // same port on the other IP version's loopback address for as long
        // as the server runs (see `bind_with_companion`).
        let ((listener, companion), port, port_source) = bind_port(
            &PortRequest {
                flag: self.port,
                project: session.project_port(),
                prefer_default: checkout.primary,
                slot: worktree_slot(&checkout.root),
            },
            |port| port_is_free(ip, port),
            |port| bind_with_companion(ip, port),
        )?;

        let token_file = match &self.studio_token_file {
            Some(path) => resolve_path(path)?.into_owned(),
            None => checkout.shared_state.join(TOKEN_FILE),
        };
        ensure_token(&token_file, self.studio_token_file.is_none())?;

        let allowed_hosts = if self.allowed_hosts.is_empty() {
            session.serve_allowed_hosts().to_vec()
        } else {
            self.allowed_hosts
        };

        let sourcemap = if self.no_sourcemap {
            None
        } else {
            let writer = Arc::new(SourcemapWriter::new(
                session.root_dir().join(SOURCEMAP_FILE),
            ));
            // Take the cursor before the first write, so a change made while
            // it is generated is still seen by the watcher.
            let cursor = session.message_queue().cursor();
            if let Err(err) = writer.update(&session) {
                log::warn!("Could not write {}: {err:#}", writer.path.display());
            }
            spawn_sourcemap_watcher(Arc::clone(&session), Arc::clone(&writer), cursor);
            spawn_sourcemap_retry(Arc::clone(&session), Arc::clone(&writer));
            Some(writer)
        };

        let studio_address = studio_address(ip, port);
        let state_path = checkout.worktree_state.join(STATE_FILE);
        let project_file = &session.root_project().file_location;
        // Recorded whatever the address, so that commands run here without
        // flags never fall back to the default port: for an address they
        // cannot reach, the record says so and they stop with an error.
        let state = Some(DevState {
            pid: std::process::id(),
            address: studio_address
                .clone()
                .unwrap_or_else(|| format!("http://{}:{port}", display_host(ip))),
            token_file: token_file.clone(),
            project: dunce::canonicalize(project_file).unwrap_or_else(|_| project_file.clone()),
            server_session: session.session_id().to_string(),
            discoverable: studio_address.is_some(),
        });

        let mut place_ids: Option<Vec<u64>> = session
            .serve_place_ids()
            .map(|ids| ids.iter().copied().collect());
        if let Some(ids) = &mut place_ids {
            ids.sort_unstable();
        }
        let status = StatusBlock {
            project_name: session.project_name().to_owned(),
            project_file: session.root_project().file_location.clone(),
            url: format!("http://{}:{port}/", display_host(ip)),
            port,
            port_source,
            place_ids,
            plugin: plugin_summary(report.checks()),
            token_file: token_file.clone(),
            sourcemap: sourcemap.as_ref().map(|writer| writer.path.clone()),
            studio_address: studio_address.clone(),
            non_loopback: !ip.is_loopback(),
        };

        let server = LiveServer::new(Arc::clone(&session)).with_studio_token_file(&token_file)?;
        // Written before serving, while the port is already held, and
        // required: without a record, `rojo studio`, `rojo test` and
        // `rojo mcp` run here without flags would use the default port and
        // the shared token, often another worktree's server.
        if let Some(state) = &state {
            state.write(&state_path).with_context(|| {
                format!(
                    "rojo dev did not start because it could not record this server in {}, \
                     which `rojo studio`, `rojo test` and `rojo mcp` use to find it",
                    state_path.display()
                )
            })?;
        }
        let result = server.start_on_until(
            listener,
            allowed_hosts,
            || {
                print!("{}", render_status(&status));
                let _ = std::io::stdout().flush();
            },
            shutdown_signal(),
        );

        // Hold the sourcemap lock until the process exits, so it cannot end
        // halfway through replacing the file.
        if let Some(writer) = &sourcemap {
            mem::forget(writer.lock());
        }
        if let Some(state) = &state {
            state.remove_own(&state_path);
        }
        // Held until the server stopped, so no other server could take this
        // port on the other loopback address meanwhile.
        drop(companion);
        if result.is_ok() {
            println!("rojo dev stopped.");
        }

        // Dropping a ServeSession can be very slow and the process is about
        // to exit anyway.
        mem::forget(session);
        result
    }
}

/// Where a checkout's `rojo dev` keeps its files.
#[derive(Debug, Clone)]
struct Checkout {
    /// The worktree's top folder, or the project folder outside Git.
    root: PathBuf,
    /// Whether this is a repository's main checkout, or not in Git at all.
    /// Only those prefer the default port; linked worktrees start in their
    /// own slot so they do not race the main checkout for it.
    primary: bool,
    in_git: bool,
    /// Shared by every worktree of the repository.
    shared_state: PathBuf,
    /// This worktree's own.
    worktree_state: PathBuf,
}

impl Checkout {
    fn discover(start: &Path) -> anyhow::Result<Checkout> {
        match lifecycle::git(start, ["rev-parse", "--show-toplevel"]) {
            Ok(top) => {
                // Git prints forward slashes on Windows; rebuilding the paths
                // from their components gives one separator style.
                let root: PathBuf = PathBuf::from(top).components().collect();
                let root = dunce::canonicalize(&root).unwrap_or(root);
                let common: PathBuf = lifecycle::git_common_dir(start)?.components().collect();
                let git_dir: PathBuf =
                    PathBuf::from(lifecycle::git(start, ["rev-parse", "--absolute-git-dir"])?)
                        .components()
                        .collect();
                let primary = same_path(&git_dir, &common);
                Ok(Checkout {
                    primary,
                    in_git: true,
                    shared_state: super::repository_state_dir(&common)?,
                    worktree_state: super::project_state_dir(&root)?,
                    root,
                })
            }
            Err(err) if super::has_git_marker(start) => Err(err.context(format!(
                "{} is in a Git repository, but Git could not read it",
                start.display()
            ))),
            Err(_) => Ok(Checkout::outside_git(start)),
        }
    }

    fn outside_git(folder: &Path) -> Checkout {
        let root = dunce::canonicalize(folder).unwrap_or_else(|_| folder.to_owned());
        // Without a per-user folder (no LOCALAPPDATA or HOME), the state goes
        // nowhere useful; fall back to the system temp folder rather than
        // into the project, where `rojo dev` could sync it into the place.
        let state = super::project_state_dir(&root).unwrap_or_else(|_| {
            std::env::temp_dir()
                .join("rojo-dev")
                .join(&blake3::hash(root.to_string_lossy().as_bytes()).to_hex()[..16])
        });
        Checkout {
            root,
            primary: true,
            in_git: false,
            shared_state: state.clone(),
            worktree_state: state,
        }
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let a = dunce::canonicalize(a).unwrap_or_else(|_| a.to_owned());
    let b = dunce::canonicalize(b).unwrap_or_else(|_| b.to_owned());
    a == b
}

/// What a running `rojo dev` records for the commands that talk to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevState {
    pid: u32,
    /// The loopback URL of the server, as `rojo studio --address` takes it.
    address: String,
    token_file: PathBuf,
    /// The canonical project file, as the bridge reports it.
    project: PathBuf,
    /// The serve session's random ID, which the bridge reports only to
    /// callers holding the token. A server that later takes the port, even
    /// one serving the same project with the shared token, reports another.
    server_session: String,
    /// Whether `rojo studio` can reach the server through `address`. It
    /// cannot when the server listens on one non-loopback address; the
    /// record then only stops flagless commands from using the default
    /// port. Records written before this field existed were reachable.
    #[serde(default = "reachable")]
    discoverable: bool,
}

fn reachable() -> bool {
    true
}

impl DevState {
    /// Writes the record while holding its lock (see [`lock_beside`]).
    fn write(&self, path: &Path) -> anyhow::Result<()> {
        let parent = path.parent().context("State file has no folder")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("Could not create {}", parent.display()))?;
        let _lock = lock_beside(path)?;
        write_atomically(path, &serde_json::to_vec_pretty(self)?)
    }

    /// The record at `path`: `None` only when there is no file. A file that
    /// cannot be read or is not a record is an error, because treating it as
    /// "no rojo dev here" would send commands to the default address, which
    /// is often another worktree's server.
    fn read(path: &Path) -> Result<Option<DevState>, String> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|err| format!("it is not a valid record ({err})")),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("it could not be read ({err})")),
        }
    }

    /// Removes the record if it is still exactly this one. The check and the
    /// removal happen under the record's lock, so a `rojo dev` that is
    /// stopping cannot delete the record of one that started meanwhile.
    fn remove_own(&self, path: &Path) {
        let Ok(_lock) = lock_beside(path) else {
            return;
        };
        if DevState::read(path).ok().flatten().as_ref() == Some(self) {
            let _ = fs::remove_file(path);
        }
    }
}

/// Takes an exclusive lock on a hidden file next to `path`, released when
/// the returned file is dropped or the process dies. Every writer of `path`
/// holds it, so checking the file and changing it cannot interleave with
/// another process doing the same.
fn lock_beside(path: &Path) -> anyhow::Result<fs::File> {
    let name = path
        .file_name()
        .context("Path has no file name")?
        .to_string_lossy();
    let lock_path = path.with_file_name(format!(".{name}.lock"));
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("Could not open {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("Could not lock {}", lock_path.display()))?;
    Ok(file)
}

/// The Studio bridge address and token a command should use.
///
/// When either flag is given, the command behaves as it did before `rojo
/// dev` existed: the other one keeps its old default (the default address,
/// or no token), so an explicit invocation never goes somewhere new. With
/// neither, a `rojo dev` recorded for the worktree around `start` is used
/// once it is confirmed to be live (see [`verify`]); without a record, the
/// default address and the token `rojo dev` creates.
///
/// A record that cannot be confirmed is an error rather than a reason to try
/// the default address: that server is often another worktree's, the main
/// checkout's for example, and a command meant for this worktree's Studio
/// must not run against it.
pub(super) fn bridge_target(
    address: Option<&str>,
    token_file: Option<&Path>,
    start: &Path,
) -> anyhow::Result<(String, Option<PathBuf>)> {
    if address.is_some() || token_file.is_some() {
        return Ok((
            address.unwrap_or(DEFAULT_STUDIO_ADDRESS).to_owned(),
            token_file.map(Path::to_owned),
        ));
    }
    // `rojo mcp --project` may name the project file rather than its folder.
    let start = match start.parent() {
        Some(parent) if start.is_file() => parent,
        _ => start,
    };
    let checkout = Checkout::discover(start).ok();
    if let Some((path, state)) = checkout
        .as_ref()
        .and_then(|checkout| find_running(checkout, start))
    {
        return use_record(&path, state);
    }
    let token_file = checkout.and_then(|checkout| {
        let default = checkout.shared_state.join(TOKEN_FILE);
        default.is_file().then_some(default)
    });
    use_default(DEFAULT_STUDIO_ADDRESS, token_file)
}

/// The default bridge, with the shared token when there is one. Nothing
/// says which server should be there, so any serve session and project
/// will do, but the server must prove it holds the token before it is sent:
/// whatever listens on the default port may be anything.
fn use_default(
    address: &str,
    token_file: Option<PathBuf>,
) -> anyhow::Result<(String, Option<PathBuf>)> {
    let Some(token_file) = token_file else {
        return Ok((address.to_owned(), None));
    };
    match prove(address, &token_file) {
        Ok(_) => Ok((address.to_owned(), Some(token_file))),
        Err(reason) => Err(super::UsageError(anyhow::anyhow!(
            "No rojo dev is recorded for this worktree, and the server on {address} could not \
             be confirmed to hold the shared Studio token {}: {reason}. Pass --address and \
             --token-file for the server to use.",
            token_file.display()
        ))
        .into()),
    }
}

/// The bridge a confirmed record names, or an error when it cannot be read
/// or confirmed (see [`bridge_target`]).
fn use_record(
    path: &Path,
    state: Result<DevState, String>,
) -> anyhow::Result<(String, Option<PathBuf>)> {
    if let Ok(state) = &state {
        if !state.discoverable {
            return Err(super::UsageError(anyhow::anyhow!(
                "This worktree's rojo dev, recorded in {}, listens on {}, which is not a \
                 loopback address, so commands cannot find it by themselves. Pass --address \
                 and --token-file for the server to use.",
                path.display(),
                state.address
            ))
            .into());
        }
    }
    match state.and_then(|state| verify(&state).map(|()| state)) {
        Ok(state) => Ok((state.address, Some(state.token_file))),
        Err(reason) => Err(super::UsageError(anyhow::anyhow!(
            "This worktree's rojo dev, recorded in {}, could not be confirmed: {reason}. \
             Restart `rojo dev` here, or pass --address and --token-file for the server to use. \
             Delete that file if no rojo dev should run here.",
            path.display()
        ))
        .into()),
    }
}

/// The state a `rojo dev` recorded for this worktree, and where. Outside Git
/// it is keyed by the project folder, so the folders above `start` are tried
/// too, for commands run in a subfolder of the project.
fn find_running(checkout: &Checkout, start: &Path) -> Option<(PathBuf, Result<DevState, String>)> {
    let read = |path: PathBuf| DevState::read(&path).transpose().map(|state| (path, state));
    if checkout.in_git {
        return read(checkout.worktree_state.join(STATE_FILE));
    }
    let start = dunce::canonicalize(start).unwrap_or_else(|_| start.to_owned());
    start
        .ancestors()
        .find_map(|dir| read(super::project_state_dir(dir).ok()?.join(STATE_FILE)))
}

/// Confirms that `state` still describes a live server: its process runs,
/// and the server on its address proves it holds the token, without being
/// sent it, and reports the same serve session and project. A `rojo dev` that was
/// killed leaves its record behind, and another server, perhaps another
/// worktree's with the shared token, may since have taken the port.
fn verify(state: &DevState) -> Result<(), String> {
    // Skips the request for a dead process, which matters on Windows,
    // where a refused loopback connection takes about two seconds.
    if process_alive(state.pid) == Some(false) {
        return Err(format!("its process {} is no longer running", state.pid));
    }
    let (session, project) = prove(&state.address, &state.token_file)?;
    let same_session = session == state.server_session;
    let same_project = Path::new(&project) == state.project;
    if same_session && same_project {
        Ok(())
    } else {
        Err(format!(
            "the server on {} is not the one rojo dev recorded",
            state.address
        ))
    }
}

/// The endpoints a request to `base` may have arrived on. `localhost` may
/// resolve to either loopback address; both are this machine's own.
fn dialed_endpoints(base: &reqwest::Url, port: u16) -> Vec<std::net::SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    let host = base.host_str().unwrap_or_default().trim_matches(['[', ']']);
    match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
        ],
    }
}

/// Has the server on `address` prove that it holds the token in
/// `token_file`, without sending it the token, and returns the serve session
/// and project it reports.
fn prove(address: &str, token_file: &Path) -> Result<(String, String), String> {
    let base = studio::local_address(address).map_err(|err| format!("{err:#}"))?;
    let token =
        crate::web::studio::read_token_file(token_file).map_err(|err| format!("{err:#}"))?;
    // The bridge may hold a request while Git is busy, so a shorter wait
    // would reject a live server whose repository is mid-checkout.
    let client = studio::bridge_client(VERIFY_TIMEOUT).map_err(|err| err.to_string())?;
    // The token is never sent here: whatever now listens on the recorded
    // port, perhaps a process that took it after the recorded server ended,
    // must first prove it holds the token, for a nonce of this request.
    let mut nonce = [0u8; 32];
    nonce[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    nonce[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let url = base
        .join(&format!(
            "api/studio/prove?nonce={}",
            data_encoding::HEXLOWER.encode(&nonce)
        ))
        .map_err(|err| err.to_string())?;
    let response = client.get(url).send().map_err(|err| {
        if err.is_timeout() {
            format!("the server on {address} did not answer in time")
        } else {
            format!("nothing answered on {address}")
        }
    })?;
    // Redirects are not followed (see `bridge_client`), and one is never
    // the bridge's answer. An older bridge without the route answers 404.
    if !response.status().is_success() {
        return Err(format!(
            "the server on {address} answered with HTTP {}",
            response.status()
        ));
    }
    // Read with a limit before anything is parsed: until the proof is
    // checked, whatever answers may be any process, and could send without end.
    const PROVE_LIMIT: u64 = 64 * 1024;
    let mut body = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(response, PROVE_LIMIT + 1),
        &mut body,
    )
    .map_err(|_| format!("the server on {address} is not its Studio bridge"))?;
    if body.len() as u64 > PROVE_LIMIT {
        return Err(format!("the server on {address} is not its Studio bridge"));
    }
    let identity: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|_| format!("the server on {address} is not its Studio bridge"))?;
    let session = identity["serverSession"].as_str().unwrap_or_default();
    // The proof covers the address and port the answering server saw the
    // connection arrive on, which must be the ones dialed: a relay passing on
    // another bridge's answer gives that bridge's endpoint, even one on the
    // same port at another loopback address.
    let port = base.port_or_known_default().unwrap_or_default();
    let endpoints = dialed_endpoints(&base, port);
    let proof = identity["proof"].as_str();
    if identity["port"].as_u64() != Some(u64::from(port))
        || !endpoints.iter().any(|endpoint| {
            proof
                == Some(
                    crate::web::studio::token_proof(&token, &nonce, session, *endpoint).as_str(),
                )
        })
    {
        return Err(format!(
            "the server on {address} could not prove it holds this worktree's Studio token"
        ));
    }
    Ok((
        session.to_owned(),
        identity["projectPath"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    ))
}

/// Whether process `pid` runs, or None when that cannot be told.
fn process_alive(pid: u32) -> Option<bool> {
    if pid == std::process::id() {
        return Some(true);
    }
    #[cfg(target_os = "linux")]
    {
        Some(Path::new(&format!("/proc/{pid}")).exists())
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // Another user's process fails too, and is not this user's rojo dev.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .map(|status| status.success())
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Why the server listens on the port it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortSource {
    Flag,
    Project,
    Default,
    Worktree,
}

#[derive(Debug, Clone)]
struct PortRequest {
    flag: Option<u16>,
    project: Option<u16>,
    prefer_default: bool,
    /// Where in the worktree range to start looking.
    slot: u16,
}

/// The worktree's slot in the range of worktree ports, from its path, so the
/// same worktree tends to get the same port each time.
fn worktree_slot(root: &Path) -> u16 {
    let hash = blake3::hash(root.to_string_lossy().as_bytes());
    let bytes = hash.as_bytes();
    u16::from_le_bytes([bytes[0], bytes[1]]) % WORKTREE_PORTS
}

/// How many automatically chosen ports `bind_port` tries before giving up.
const BIND_ATTEMPTS: usize = 8;

/// Picks a port with [`pick_port`] and binds it with `bind`. Another launch
/// can take an automatically chosen port between the probe and the bind; that
/// port is then skipped and the next one tried. A port from `--port` or
/// `servePort` is never swapped, so its bind failing is an error.
fn bind_port<L>(
    request: &PortRequest,
    is_free: impl Fn(u16) -> bool,
    mut bind: impl FnMut(u16) -> std::io::Result<L>,
) -> anyhow::Result<(L, u16, PortSource)> {
    let mut lost = Vec::new();
    loop {
        let (port, source) = pick_port(request, |port| !lost.contains(&port) && is_free(port))?;
        match bind(port) {
            Ok(listener) => return Ok((listener, port, source)),
            Err(err)
                if matches!(source, PortSource::Default | PortSource::Worktree)
                    && matches!(
                        err.kind(),
                        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                    )
                    && lost.len() + 1 < BIND_ATTEMPTS =>
            {
                log::debug!("Port {port} was taken before rojo dev could bind it; trying another");
                lost.push(port);
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "Could not listen on port {port}. Another rojo serve or rojo dev may have taken it; pass another --port."
                    )
                })
            }
        }
    }
}

/// Picks the port to serve on. A port from `--port` or the project's
/// `servePort` is used as given, and an error when taken, since the user or
/// the project asked for exactly that port and the plugin is set up for it.
/// Otherwise the default port (when preferred), then the worktree's slot and
/// the ports after it, wrapping around the range.
fn pick_port(
    request: &PortRequest,
    is_free: impl Fn(u16) -> bool,
) -> anyhow::Result<(u16, PortSource)> {
    // Port 0 asks the system for any free port, which rojo dev would only
    // learn after binding, too late to probe both IP versions for it or to
    // have set the plugin up for it. Leaving the port out picks a free one.
    if request.flag == Some(0) {
        return Err(super::UsageError(anyhow::anyhow!(
            "rojo dev does not support --port 0. Leave --port out to have it pick a free port, or name one."
        ))
        .into());
    }
    if request.flag.is_none() && request.project == Some(0) {
        return Err(super::UsageError(anyhow::anyhow!(
            "The project sets servePort 0, which rojo dev does not support. Remove servePort to have it pick a free port, or pass --port."
        ))
        .into());
    }
    if let Some(port) = request.flag {
        anyhow::ensure!(
            is_free(port),
            "Port {port} is in use. Another rojo serve or rojo dev may be using it; stop that one or pass another --port."
        );
        return Ok((port, PortSource::Flag));
    }
    if let Some(port) = request.project {
        anyhow::ensure!(
            is_free(port),
            "The project sets servePort {port}, which is in use. Another rojo serve or rojo dev may be using it; stop that one or pass --port."
        );
        return Ok((port, PortSource::Project));
    }
    if request.prefer_default && is_free(DEFAULT_PORT) {
        return Ok((DEFAULT_PORT, PortSource::Default));
    }
    (0..WORKTREE_PORTS)
        .map(|offset| WORKTREE_PORT_FIRST + (request.slot + offset) % WORKTREE_PORTS)
        .find(|port| is_free(*port))
        .map(|port| (port, PortSource::Worktree))
        .with_context(|| {
            format!(
                "Ports {DEFAULT_PORT} to {} are all in use. Pass --port.",
                WORKTREE_PORT_FIRST + WORKTREE_PORTS - 1
            )
        })
}

/// Whether nothing listens on `port` for `ip`, nor on the loopback and
/// wildcard addresses of either IP version. The plugin connects to
/// `localhost`, which may resolve to 127.0.0.1 or ::1, so a server on the
/// other version's loopback would make it reach that server instead. The
/// wildcards are tried because on Windows, binding 127.0.0.1 can succeed
/// while another server holds 0.0.0.0 on the same port.
fn port_is_free(ip: IpAddr, port: u16) -> bool {
    // The listen address must be bindable. Each listener is dropped before
    // the next bind, so this process never conflicts with itself.
    if TcpListener::bind(SocketAddr::new(ip, port)).is_err() {
        return false;
    }
    let others: [IpAddr; 4] = [
        Ipv4Addr::LOCALHOST.into(),
        Ipv4Addr::UNSPECIFIED.into(),
        Ipv6Addr::LOCALHOST.into(),
        Ipv6Addr::UNSPECIFIED.into(),
    ];
    others
        .into_iter()
        .filter(|other| *other != ip)
        .all(
            |other| match TcpListener::bind(SocketAddr::new(other, port)) {
                Ok(_) => true,
                // In use, or reserved (Windows reports an excluded port range
                // as access denied).
                Err(err) => !matches!(
                    err.kind(),
                    std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                ),
            },
        )
}

/// Binds the server's listener on `ip` and `port`, and reserves the same port
/// on the other IP version's loopback address with a socket that is bound
/// but not listening.
///
/// The plugin connects to `localhost`, which may resolve to either version.
/// [`port_is_free`] checked that nothing listens on the other one, but
/// another server could take it later, and `localhost` would then reach that
/// server instead of this one. The reservation never accepts connections, so
/// a client trying it is refused and moves on to this server's address.
///
/// The other address being in use means another launch took the port first,
/// and is reported as the bind failing, so [`bind_port`] treats it as a lost
/// race. A machine without the other IP version gets no reservation.
fn bind_with_companion(
    ip: IpAddr,
    port: u16,
) -> std::io::Result<(TcpListener, Option<tokio::net::TcpSocket>)> {
    let listener = TcpListener::bind(SocketAddr::new(ip, port))?;
    // Deprecated because setting the option after binding does nothing;
    // reading it, as here, is what tells the two cases apart.
    #[allow(deprecated)]
    let only_v6 = || listener.only_v6();
    // `::` also accepts IPv4 connections unless the socket is IPv6-only,
    // which it is by default on Windows. Either way 127.0.0.1 is reserved
    // too: an IPv6-only listener leaves it free, and on macOS a more specific
    // bind may take it even from a dual-stack one. Where the listener does
    // hold it, as on Linux, reserving it fails against the listener itself,
    // which is not another launch.
    let mut held_by_listener = false;
    let other = match ip {
        IpAddr::V6(v6) if v6.is_unspecified() => {
            held_by_listener = match only_v6() {
                Ok(only) => !only,
                Err(err) => {
                    log::debug!("Could not tell whether [::]:{port} accepts IPv4: {err}");
                    true
                }
            };
            Some(Ipv4Addr::LOCALHOST.into())
        }
        _ => companion_address(ip),
    };
    let Some(other) = other else {
        return Ok((listener, None));
    };
    let socket = match other {
        IpAddr::V4(_) => tokio::net::TcpSocket::new_v4(),
        IpAddr::V6(_) => tokio::net::TcpSocket::new_v6(),
    };
    // Not setting SO_REUSEADDR, so that no other socket can share the port.
    match socket.and_then(|socket| socket.bind(SocketAddr::new(other, port)).map(|()| socket)) {
        Ok(socket) => Ok((listener, Some(socket))),
        Err(err) if held_by_listener && err.kind() == std::io::ErrorKind::AddrInUse => {
            Ok((listener, None))
        }
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            Err(err)
        }
        Err(err) => {
            log::debug!("Not reserving port {port} on {other}: {err}");
            Ok((listener, None))
        }
    }
}

/// The loopback address of the other IP version, which `localhost` may
/// resolve to instead of the one the server listens on.
fn companion_address(ip: IpAddr) -> Option<IpAddr> {
    match ip {
        IpAddr::V4(v4) if v4.is_loopback() || v4.is_unspecified() => {
            Some(Ipv6Addr::LOCALHOST.into())
        }
        IpAddr::V6(v6) if v6.is_loopback() => Some(Ipv4Addr::LOCALHOST.into()),
        // `::` is decided by `bind_with_companion`, from the listener. Other
        // addresses are not reached through `localhost`.
        _ => None,
    }
}

/// The URL `rojo studio` reaches the server on, which must be loopback. A
/// server bound to one non-loopback address cannot be reached that way.
fn studio_address(ip: IpAddr, port: u16) -> Option<String> {
    match ip {
        IpAddr::V4(v4) if v4.is_loopback() => Some(format!("http://{v4}:{port}")),
        IpAddr::V4(v4) if v4.is_unspecified() => Some(format!("http://127.0.0.1:{port}")),
        IpAddr::V6(v6) if v6.is_loopback() || v6.is_unspecified() => {
            Some(format!("http://[::1]:{port}"))
        }
        _ => None,
    }
}

fn display_host(ip: IpAddr) -> String {
    match ip {
        ip if ip.is_loopback() => "localhost".to_owned(),
        IpAddr::V6(v6) => format!("[{v6}]"),
        IpAddr::V4(v4) => v4.to_string(),
    }
}

/// One line on the installed plugin, from doctor's plugin checks.
fn plugin_summary(checks: &[doctor::Check]) -> String {
    let mut plugin = checks
        .iter()
        .filter(|check| check.category() == Category::Plugin);
    let Some(first) = plugin.next() else {
        return "not checked".to_owned();
    };
    let mut summary = match first.code() {
        "plugin-current" => "installed, the same version as this CLI".to_owned(),
        "plugin-missing" => "not installed; run `rojo plugin install`".to_owned(),
        _ => first.message().to_owned(),
    };
    let more = plugin
        .filter(|check| check.status() >= Status::Warning)
        .count();
    if more > 0 {
        summary.push_str(&format!(" ({more} more warning(s) above)"));
    }
    summary
}

/// What the Studio bridge accepts as a token.
fn valid_token(token: &str) -> bool {
    (32..=256).contains(&token.len())
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Whether `contents` is exactly a token as [`studio::create_token`] writes
/// it: two UUIDs as 64 lowercase hex digits, nothing before or after.
fn is_created_token(contents: &str) -> bool {
    contents.len() == 64
        && contents
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Makes sure `path` holds a valid token, creating one when it does not
/// exist.
///
/// Several worktrees may start `rojo dev` at once for the first time, so a
/// new token is written whole, permissions first, to a file of this
/// process's own, and renamed into place only while no token exists, under
/// a lock every `rojo dev` takes for it. Readers therefore only ever see no
/// file or a complete one, and every process ends up with the same token.
///
/// `created` says the file is one `rojo dev` makes (the default), whose
/// contents must then be exactly what it writes. A file passed with
/// `--studio-token-file` may be any token the bridge accepts.
fn ensure_token(path: &Path, created: bool) -> anyhow::Result<()> {
    let check = || -> anyhow::Result<bool> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(err).with_context(|| format!("Could not read {}", path.display()))
            }
        };
        let valid = if created {
            is_created_token(&contents)
        } else {
            valid_token(contents.trim())
        };
        anyhow::ensure!(
            valid,
            "{} is not a valid Studio token ({}). Delete it to have rojo dev create a new one, or pass --studio-token-file.",
            path.display(),
            if created {
                "64 lowercase hex digits, as rojo dev writes it"
            } else {
                "32-256 letters, digits or hyphens"
            }
        );
        Ok(true)
    };
    if check()? {
        return Ok(());
    }

    let parent = path.parent().context("Token path has no folder")?;
    fs::create_dir_all(parent).with_context(|| format!("Could not create {}", parent.display()))?;
    let name = path
        .file_name()
        .context("Token path has no file name")?
        .to_string_lossy();
    let temporary = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    studio::create_token(&temporary)
        .with_context(|| format!("Could not create the Studio token {}", temporary.display()))?;
    let published = (|| {
        let _lock = lock_beside(path)?;
        if !path.exists() {
            fs::rename(&temporary, path)
                .with_context(|| format!("Could not create the Studio token {}", path.display()))?;
        }
        anyhow::Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    published?;

    anyhow::ensure!(
        check()?,
        "{} disappeared right after it was created",
        path.display()
    );
    Ok(())
}

struct StatusBlock {
    project_name: String,
    project_file: PathBuf,
    url: String,
    port: u16,
    port_source: PortSource,
    place_ids: Option<Vec<u64>>,
    plugin: String,
    token_file: PathBuf,
    sourcemap: Option<PathBuf>,
    /// None when the server is not reachable on loopback.
    studio_address: Option<String>,
    non_loopback: bool,
}

fn render_status(status: &StatusBlock) -> String {
    let port_note = match status.port_source {
        PortSource::Flag => "from --port",
        PortSource::Project => "the project's servePort",
        PortSource::Default => "the default port",
        PortSource::Worktree => "picked for this worktree",
    };
    let place_ids = match &status.place_ids {
        None => "not set, so any place can sync".to_owned(),
        Some(ids) if ids.is_empty() => "empty, so no place can sync".to_owned(),
        Some(ids) => ids
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    };
    let mut out = format!(
        "rojo dev is serving {} ({})\n",
        status.project_name,
        status.project_file.display()
    );
    out.push_str(&format!(
        "  Server:        {} (port {}, {port_note})\n",
        status.url, status.port
    ));
    out.push_str(&format!("  servePlaceIds: {place_ids}\n"));
    out.push_str(&format!("  Studio plugin: {}\n", status.plugin));
    out.push_str(&format!(
        "  Studio token:  {}\n",
        status.token_file.display()
    ));
    match &status.sourcemap {
        Some(path) => out.push_str(&format!(
            "  Sourcemap:     {} (kept up to date)\n",
            path.display()
        )),
        None => out.push_str("  Sourcemap:     off (--no-sourcemap)\n"),
    }
    match &status.studio_address {
        Some(address) => {
            out.push_str(&format!(
                "  Studio bridge: rojo studio --address {address} --token-file {} sessions\n",
                doctor::shell_quote(&status.token_file)
            ));
            out.push_str(
                "                 In this worktree, `rojo studio sessions` finds both by itself.\n",
            );
        }
        None => out.push_str(
            "  Studio bridge: `rojo studio` needs a loopback address; serve on 127.0.0.1 to use it.\n",
        ),
    }
    if status.port != DEFAULT_PORT {
        out.push_str(&format!(
            "  In Studio, set the Rojo plugin's port to {} before connecting.\n",
            status.port
        ));
    }
    if status.non_loopback {
        out.push_str(
            "  WARNING: this server is reachable from the network, and its serve API is unauthenticated.\n",
        );
    }
    out.push_str("Press Ctrl+C to stop.\n");
    out
}

/// Writes `sourcemap.json` when its contents change, never leaving a partly
/// written file for luau-lsp to read.
struct SourcemapWriter {
    path: PathBuf,
    /// The bytes last written or found on disk. Holding the lock also marks a
    /// write in progress.
    last: Mutex<Option<Vec<u8>>>,
    /// Set when the last attempt failed, so the retry thread tries again
    /// instead of waiting for the next change that affects the sourcemap.
    dirty: AtomicBool,
}

impl SourcemapWriter {
    fn new(path: PathBuf) -> Self {
        SourcemapWriter {
            path,
            last: Mutex::new(None),
            dirty: AtomicBool::new(false),
        }
    }

    fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    /// When the last attempt failed, regenerates with `generate` and writes
    /// again. Returns None when nothing was pending.
    fn retry_if_dirty(
        &self,
        generate: impl FnOnce() -> anyhow::Result<Vec<u8>>,
    ) -> Option<anyhow::Result<bool>> {
        let mut last = self.lock();
        // Checked under the lock: a write that just finished may have
        // cleared it.
        if !self.is_dirty() {
            return None;
        }
        Some(self.generate_and_write_locked(&mut last, generate))
    }

    /// Generates and writes under one lock, so two writers cannot cross: a
    /// map generated earlier can never land after, and replace, a newer one.
    fn generate_and_write(
        &self,
        generate: impl FnOnce() -> anyhow::Result<Vec<u8>>,
    ) -> anyhow::Result<bool> {
        let mut last = self.lock();
        self.generate_and_write_locked(&mut last, generate)
    }

    fn generate_and_write_locked(
        &self,
        last: &mut Option<Vec<u8>>,
        generate: impl FnOnce() -> anyhow::Result<Vec<u8>>,
    ) -> anyhow::Result<bool> {
        let result = generate().and_then(|bytes| self.write_locked(last, bytes));
        self.dirty.store(result.is_err(), Ordering::SeqCst);
        result
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Vec<u8>>> {
        self.last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Regenerates the sourcemap and writes it when it differs from the last
    /// one. Returns whether the file was written.
    fn update(&self, session: &ServeSession) -> anyhow::Result<bool> {
        self.generate_and_write(|| generate_sourcemap(session))
    }

    #[cfg(test)]
    fn write_if_changed(&self, bytes: Vec<u8>) -> anyhow::Result<bool> {
        self.generate_and_write(|| Ok(bytes))
    }

    fn write_locked(&self, last: &mut Option<Vec<u8>>, bytes: Vec<u8>) -> anyhow::Result<bool> {
        if last.is_none() {
            // An unchanged file from an earlier run is left alone, so editors
            // watching it do not reload for nothing.
            *last = fs::read(&self.path).ok();
        }
        if last.as_deref() == Some(bytes.as_slice()) {
            return Ok(false);
        }
        write_atomically(&self.path, &bytes)?;
        *last = Some(bytes);
        Ok(true)
    }
}

fn generate_sourcemap(session: &ServeSession) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    sourcemap::write_sourcemap_json(session, &mut bytes, false, &HashMap::new())?;
    Ok(bytes)
}

/// How often a failed sourcemap write is tried again.
const SOURCEMAP_RETRY: Duration = Duration::from_secs(1);

/// Retries a failed write every [`SOURCEMAP_RETRY`] until one succeeds, for
/// example once an editor that held the file open lets go. Failures after
/// the first are only logged at debug level, since the watcher already
/// warned.
fn spawn_sourcemap_retry(session: Arc<ServeSession>, writer: Arc<SourcemapWriter>) {
    thread::Builder::new()
        .name("rojo dev sourcemap retry".to_owned())
        .spawn(move || loop {
            thread::sleep(SOURCEMAP_RETRY);
            match writer.retry_if_dirty(|| generate_sourcemap(&session)) {
                Some(Ok(_)) => log::info!("Updated {} after retrying", writer.path.display()),
                Some(Err(err)) => {
                    log::debug!("Still cannot update {}: {err:#}", writer.path.display())
                }
                None => {}
            }
        })
        .expect("Could not start the sourcemap retry thread");
}

/// Rewrites the sourcemap after changes that affect it. `cursor` must be
/// taken before the initial sourcemap is generated, so nothing in between
/// is missed.
fn spawn_sourcemap_watcher(
    session: Arc<ServeSession>,
    writer: Arc<SourcemapWriter>,
    mut cursor: u32,
) {
    thread::Builder::new()
        .name("rojo dev sourcemap".to_owned())
        .spawn(move || {
            loop {
                let affected = match session.message_queue().subscribe(cursor) {
                    Ok(receiver) => match futures::executor::block_on(receiver) {
                        Ok((new_cursor, messages)) => {
                            cursor = new_cursor;
                            let patches: Vec<_> =
                                messages.into_iter().map(|message| message.patch).collect();
                            sourcemap::patch_set_affects_sourcemap(
                                &session,
                                &patches,
                                sourcemap::filter_non_scripts,
                            )
                        }
                        // The queue was dropped: the server is shutting down.
                        Err(_) => return,
                    },
                    // Changes were trimmed from history before this thread
                    // saw them, so rewrite from the current tree.
                    Err(_) => {
                        cursor = session.message_queue().cursor();
                        true
                    }
                };
                if affected {
                    if let Err(err) = writer.update(&session) {
                        log::warn!("Could not update {}: {err:#}", writer.path.display());
                    }
                }
            }
        })
        .expect("Could not start the sourcemap thread");
}

/// Writes `bytes` to a temporary file next to `path`, then renames it over
/// `path`, so readers see the old file or the new one, never part of one.
///
/// A symlinked `path` keeps its link: the file it points to is replaced, as
/// `rojo build` does, so an editor reading through the link keeps up.
fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let path = &super::build::resolve_output(path)
        .with_context(|| format!("Could not resolve symlink {}", path.display()))?;
    let name = path
        .file_name()
        .context("Path has no file name")?
        .to_string_lossy();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&temporary, bytes)
        .with_context(|| format!("Could not write {}", temporary.display()))?;
    // On Windows, a reader holding the file open without sharing deletion
    // makes the rename fail for a moment, so try a few times.
    let mut attempt = 0;
    loop {
        match fs::rename(&temporary, path) {
            Ok(()) => return Ok(()),
            Err(_) if attempt < 5 => {
                attempt += 1;
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                let _ = fs::remove_file(&temporary);
                return Err(err).with_context(|| format!("Could not replace {}", path.display()));
            }
        }
    }
}

/// Completes on Ctrl+C, and on the other ways a terminal asks a program to
/// stop: SIGTERM and SIGHUP on Unix, Ctrl+Break and closing the console
/// window on Windows. A signal that cannot be listened for is skipped rather
/// than treated as received.
async fn shutdown_signal() {
    async fn ctrl_c() {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        async fn wait(kind: SignalKind) {
            match signal(kind) {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(_) => std::future::pending().await,
            }
        }
        tokio::select! {
            () = ctrl_c() => {}
            () = wait(SignalKind::terminate()) => {}
            () = wait(SignalKind::hangup()) => {}
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::windows;
        async fn ctrl_break() {
            match windows::ctrl_break() {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(_) => std::future::pending().await,
            }
        }
        async fn ctrl_close() {
            match windows::ctrl_close() {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(_) => std::future::pending().await,
            }
        }
        tokio::select! {
            () = ctrl_c() => {}
            () = ctrl_break() => {}
            () = ctrl_close() => {}
        }
    }

    #[cfg(not(any(unix, windows)))]
    ctrl_c().await;
}

#[cfg(test)]
mod test {
    use super::*;
    use std::{collections::HashSet, time::Instant};

    fn request(flag: Option<u16>, project: Option<u16>, prefer_default: bool) -> PortRequest {
        PortRequest {
            flag,
            project,
            prefer_default,
            slot: 10,
        }
    }

    #[test]
    fn the_port_flag_wins_and_fails_when_taken() {
        let picked = pick_port(&request(Some(5000), Some(6000), true), |_| true).unwrap();
        assert_eq!(picked, (5000, PortSource::Flag));
        let error = pick_port(&request(Some(5000), None, true), |port| port != 5000).unwrap_err();
        assert!(error.to_string().contains("Port 5000 is in use"), "{error}");
    }

    #[test]
    fn a_pinned_project_port_is_never_swapped_for_another() {
        let picked = pick_port(&request(None, Some(6000), true), |_| true).unwrap();
        assert_eq!(picked, (6000, PortSource::Project));
        let error = pick_port(&request(None, Some(6000), true), |port| port != 6000).unwrap_err();
        assert!(error.to_string().contains("servePort 6000"), "{error}");
    }

    #[test]
    fn the_main_checkout_gets_the_default_port_when_free() {
        let picked = pick_port(&request(None, None, true), |_| true).unwrap();
        assert_eq!(picked, (DEFAULT_PORT, PortSource::Default));
    }

    #[test]
    fn a_linked_worktree_starts_at_its_own_slot() {
        let picked = pick_port(&request(None, None, false), |_| true).unwrap();
        assert_eq!(picked, (WORKTREE_PORT_FIRST + 10, PortSource::Worktree));
    }

    #[test]
    fn a_taken_default_falls_back_to_the_worktree_slot() {
        let picked = pick_port(&request(None, None, true), |port| port != DEFAULT_PORT).unwrap();
        assert_eq!(picked, (WORKTREE_PORT_FIRST + 10, PortSource::Worktree));
    }

    #[test]
    fn taken_ports_are_skipped_and_the_range_wraps_around() {
        let mut slot = request(None, None, false);
        slot.slot = WORKTREE_PORTS - 1;
        let last = WORKTREE_PORT_FIRST + WORKTREE_PORTS - 1;
        let picked = pick_port(&slot, |port| port != last).unwrap();
        assert_eq!(picked, (WORKTREE_PORT_FIRST, PortSource::Worktree));
    }

    #[test]
    fn a_full_range_is_an_error() {
        let error = pick_port(&request(None, None, true), |_| false).unwrap_err();
        assert!(error.to_string().contains("Pass --port"), "{error}");
    }

    #[test]
    fn worktree_slots_are_stable_and_spread_out() {
        let a = Path::new("/repo/worktrees/a");
        assert_eq!(worktree_slot(a), worktree_slot(a));
        assert!(worktree_slot(a) < WORKTREE_PORTS);
        let slots: HashSet<_> = (0..20)
            .map(|index| worktree_slot(Path::new(&format!("/repo/worktrees/{index}"))))
            .collect();
        assert!(slots.len() > 10, "{slots:?}");
    }

    #[test]
    fn a_port_someone_listens_on_is_not_free() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free(Ipv4Addr::LOCALHOST.into(), port));
        drop(listener);
        assert!(port_is_free(Ipv4Addr::LOCALHOST.into(), port));
    }

    #[test]
    fn a_wildcard_listener_makes_the_loopback_port_busy() {
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free(Ipv4Addr::LOCALHOST.into(), port));
    }

    #[test]
    fn studio_addresses_are_loopback_only() {
        assert_eq!(
            studio_address(Ipv4Addr::LOCALHOST.into(), 1).as_deref(),
            Some("http://127.0.0.1:1")
        );
        assert_eq!(
            studio_address(Ipv4Addr::UNSPECIFIED.into(), 1).as_deref(),
            Some("http://127.0.0.1:1")
        );
        assert_eq!(
            studio_address(Ipv6Addr::LOCALHOST.into(), 1).as_deref(),
            Some("http://[::1]:1")
        );
        assert_eq!(
            studio_address(Ipv4Addr::new(192, 168, 1, 2).into(), 1),
            None
        );
    }

    fn status() -> StatusBlock {
        StatusBlock {
            project_name: "Game".to_owned(),
            project_file: PathBuf::from("/work/game/default.project.json"),
            url: "http://localhost:34872/".to_owned(),
            port: DEFAULT_PORT,
            port_source: PortSource::Default,
            place_ids: Some(vec![1, 22]),
            plugin: "installed, the same version as this CLI".to_owned(),
            token_file: PathBuf::from("/state/studio-token"),
            sourcemap: Some(PathBuf::from("/work/game/sourcemap.json")),
            studio_address: Some("http://127.0.0.1:34872".to_owned()),
            non_loopback: false,
        }
    }

    #[test]
    fn status_lists_everything_needed_to_connect() {
        let text = render_status(&status());
        for expected in [
            "rojo dev is serving Game (",
            "Server:        http://localhost:34872/ (port 34872, the default port)",
            "servePlaceIds: 1, 22",
            "Studio plugin: installed, the same version as this CLI",
            "Studio token:  ",
            "studio-token",
            "Sourcemap:     ",
            "(kept up to date)",
            "rojo studio --address http://127.0.0.1:34872 --token-file '",
            "' sessions",
            "Press Ctrl+C to stop.",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        // The plugin connects to the default port on its own.
        assert!(!text.contains("set the Rojo plugin's port"), "{text}");
        assert!(!text.contains("WARNING"), "{text}");
    }

    #[test]
    fn status_explains_a_worktree_port_and_missing_settings() {
        let mut status = status();
        status.port = 34900;
        status.port_source = PortSource::Worktree;
        status.url = "http://localhost:34900/".to_owned();
        status.place_ids = None;
        status.sourcemap = None;
        status.studio_address = None;
        status.non_loopback = true;
        let text = render_status(&status);
        for expected in [
            "(port 34900, picked for this worktree)",
            "servePlaceIds: not set, so any place can sync",
            "Sourcemap:     off (--no-sourcemap)",
            "`rojo studio` needs a loopback address",
            "set the Rojo plugin's port to 34900",
            "WARNING",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    #[test]
    fn the_sourcemap_is_written_only_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOURCEMAP_FILE);
        fs::write(&path, b"{\"a\":1}").unwrap();
        let writer = SourcemapWriter::new(path.clone());
        // Matches what an earlier run left on disk.
        assert!(!writer.write_if_changed(b"{\"a\":1}".to_vec()).unwrap());
        assert!(writer.write_if_changed(b"{\"a\":2}".to_vec()).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"{\"a\":2}");
        assert!(!writer.write_if_changed(b"{\"a\":2}".to_vec()).unwrap());
        // No temporary file is left behind.
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(SOURCEMAP_FILE)]);
    }

    #[test]
    fn the_state_file_is_only_removed_by_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(STATE_FILE);
        let mut state = DevState {
            pid: std::process::id().wrapping_add(1),
            address: "http://127.0.0.1:34900".to_owned(),
            token_file: PathBuf::from("token"),
            project: PathBuf::from("default.project.json"),
            server_session: "session".to_owned(),
            discoverable: true,
        };
        state.write(&path).unwrap();
        assert_eq!(DevState::read(&path), Ok(Some(state.clone())));
        let mine = DevState {
            pid: std::process::id(),
            ..state.clone()
        };
        mine.remove_own(&path);
        assert!(path.exists(), "another process's state was removed");

        // A newer rojo dev in the same process (or one whose PID was reused)
        // recorded another server since: still not this record.
        mine.write(&path).unwrap();
        state.pid = std::process::id();
        state.server_session = "newer".to_owned();
        state.write(&path).unwrap();
        mine.remove_own(&path);
        assert_eq!(DevState::read(&path), Ok(Some(state.clone())));

        state.remove_own(&path);
        assert_eq!(DevState::read(&path), Ok(None));
    }

    #[test]
    fn only_a_missing_record_means_no_rojo_dev() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        assert_eq!(DevState::read(&path), Ok(None));

        fs::write(&path, "{\"pid\": 1").unwrap();
        let reason = DevState::read(&path).unwrap_err();
        assert!(reason.contains("not a valid record"), "{reason}");
        // An unreadable record (here a folder in its place) fails closed too.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let reason = DevState::read(&path).unwrap_err();
        assert!(reason.contains("could not be read"), "{reason}");

        let checkout = Checkout {
            root: dir.path().to_owned(),
            primary: true,
            in_git: true,
            shared_state: dir.path().to_owned(),
            worktree_state: dir.path().to_owned(),
        };
        let (found, state) = find_running(&checkout, dir.path()).unwrap();
        assert_eq!(found, path);
        let error = use_record(&found, state).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("could not be confirmed"), "{text}");
        assert!(text.contains("could not be read"), "{text}");
        assert_eq!(super::super::exit_status(&error).0, 2);
    }

    #[test]
    fn concurrent_record_writes_and_removals_never_lose_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join(STATE_FILE));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let state = |session: &str| DevState {
            pid: std::process::id(),
            address: "http://127.0.0.1:34900".to_owned(),
            token_file: PathBuf::from("token"),
            project: PathBuf::from("default.project.json"),
            server_session: session.to_owned(),
            discoverable: true,
        };
        let old = state("old");
        old.write(&path).unwrap();
        let new = state("new");
        let threads = [
            {
                let (path, barrier, old) = (Arc::clone(&path), Arc::clone(&barrier), old.clone());
                thread::spawn(move || {
                    barrier.wait();
                    old.remove_own(&path);
                })
            },
            {
                let (path, barrier, new) = (Arc::clone(&path), Arc::clone(&barrier), new.clone());
                thread::spawn(move || {
                    barrier.wait();
                    new.write(&path).unwrap();
                })
            },
        ];
        for thread in threads {
            thread.join().unwrap();
        }
        // Whichever ran first, the new record survives.
        assert_eq!(DevState::read(&path), Ok(Some(new)));
    }

    #[test]
    fn explicit_flags_skip_discovery() {
        let (address, token) = bridge_target(
            Some("http://127.0.0.1:1"),
            Some(Path::new("token")),
            Path::new("/does/not/exist"),
        )
        .unwrap();
        assert_eq!(address, "http://127.0.0.1:1");
        assert_eq!(token, Some(PathBuf::from("token")));
    }

    #[test]
    fn one_explicit_flag_keeps_the_old_default_for_the_other() {
        // The current directory is this repository's worktree, where a
        // `rojo dev` record could exist; neither call may consult it.
        let here = std::env::current_dir().unwrap();
        let (address, token) = bridge_target(None, Some(Path::new("token")), &here).unwrap();
        assert_eq!(address, DEFAULT_STUDIO_ADDRESS);
        assert_eq!(token, Some(PathBuf::from("token")));
        let (address, token) = bridge_target(Some("http://127.0.0.1:1"), None, &here).unwrap();
        assert_eq!(address, "http://127.0.0.1:1");
        assert_eq!(token, None);
    }

    #[test]
    fn a_loopback_listener_of_the_other_ip_version_makes_the_port_busy() {
        let Ok(listener) = TcpListener::bind("[::1]:0") else {
            // No IPv6 on this machine, so `localhost` cannot be ambiguous.
            return;
        };
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free(Ipv4Addr::LOCALHOST.into(), port));
        drop(listener);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free(Ipv6Addr::LOCALHOST.into(), port));
    }

    /// A bridge answering `/api/studio/prove` once with `session` and
    /// `project`, proving with `token` for the nonce it was sent, after
    /// `delay`. Returns the address and the request it got.
    fn answer_proof(
        token: &'static str,
        session: &'static str,
        project: PathBuf,
        delay: Duration,
    ) -> (String, thread::JoinHandle<String>) {
        answer_with(delay, move |request, port| {
            let nonce = request
                .split(['?', '&', ' '])
                .find_map(|part| part.strip_prefix("nonce="))
                .and_then(|nonce| data_encoding::HEXLOWER.decode(nonce.as_bytes()).ok())
                .unwrap_or_default();
            let body = serde_json::json!({
                "serverSession": session,
                "projectPath": project,
                "port": port,
                "proof": crate::web::studio::token_proof(token, &nonce, session, std::net::SocketAddr::from(([127, 0, 0, 1], port))),
            })
            .to_string();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        })
    }

    const RECORDED_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// Answers one HTTP request with `response` after `delay`, and returns
    /// the address to reach it and the request it got.
    fn answer_raw(response: String, delay: Duration) -> (String, thread::JoinHandle<String>) {
        answer_with(delay, move |_, _| response)
    }

    /// Answers one HTTP request with what `respond` makes of it, after
    /// `delay`, and returns the address to reach it and the request it got.
    fn answer_with(
        delay: Duration,
        respond: impl FnOnce(&str, u16) -> String + Send + 'static,
    ) -> (String, thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            thread::sleep(delay);
            let mut stream = stream;
            stream
                .write_all(respond(&request, port).as_bytes())
                .unwrap();
            request
        });
        (address, handle)
    }

    /// A listener that records whether anything connected to it.
    fn redirect_target() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        (listener, address)
    }

    fn was_contacted(listener: &TcpListener) -> bool {
        // Give a followed redirect time to arrive.
        thread::sleep(Duration::from_millis(200));
        listener.accept().is_ok()
    }

    fn recorded(dir: &Path, address: String, pid: u32) -> DevState {
        let token_file = dir.join(TOKEN_FILE);
        fs::write(&token_file, RECORDED_TOKEN).unwrap();
        DevState {
            pid,
            address,
            token_file,
            project: dir.join("default.project.json"),
            server_session: "recorded-session".to_owned(),
            discoverable: true,
        }
    }

    #[test]
    fn a_record_whose_process_died_is_ignored_without_asking_the_port() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        // Nothing listens here; asking would fail differently.
        let state = recorded(dir.path(), "http://127.0.0.1:9".to_owned(), pid);
        let reason = verify(&state).unwrap_err();
        assert!(reason.contains("no longer running"), "{reason}");
    }

    #[test]
    fn another_server_on_the_recorded_port_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        // Another worktree's rojo dev, serving with the shared token on the
        // port the dead one used.
        let (address, request) = answer_proof(
            RECORDED_TOKEN,
            "another-session",
            dir.path().join("default.project.json"),
            Duration::ZERO,
        );
        let state = recorded(dir.path(), address, std::process::id());
        let reason = verify(&state).unwrap_err();
        assert!(reason.contains("not the one rojo dev recorded"), "{reason}");
        let request = request.join().unwrap().to_ascii_lowercase();
        assert!(
            request.starts_with("get /api/studio/prove?nonce="),
            "{request}"
        );
        assert!(!request.contains("x-rojo-studio-token"), "{request}");
        assert!(!request.contains(RECORDED_TOKEN), "{request}");
    }

    #[test]
    fn an_oversized_prove_answer_is_rejected_unread() {
        let dir = tempfile::tempdir().unwrap();
        let (server, _) = answer_with(Duration::ZERO, move |_, _| {
            let body = " ".repeat(128 * 1024) + "{}";
            format!(
                "HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
                body.len()
            )
        });
        let state = recorded(dir.path(), server, std::process::id());
        let reason = verify(&state).unwrap_err();
        assert!(reason.contains("is not its Studio bridge"), "{reason}");
    }

    #[test]
    fn a_symlinked_sourcemap_keeps_its_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        let link = dir.path().join("sourcemap.json");
        fs::write(&real, b"old").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("real.json", &link).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file("real.json", &link).is_err() {
            eprintln!("skipping: cannot create symlinks here");
            return;
        }

        write_atomically(&link, b"new").unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&real).unwrap(), b"new");
    }

    #[test]
    fn a_proof_from_another_loopback_address_on_the_same_port_is_rejected() {
        // What a relay on 127.0.0.1 gets back from a bridge listening on
        // 127.0.0.2 at the same port: a valid proof, but for that endpoint.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("default.project.json");
        let (relay, _) = answer_with(Duration::ZERO, move |request, port| {
            let nonce = request
                .split(['?', '&', ' '])
                .find_map(|part| part.strip_prefix("nonce="))
                .and_then(|nonce| data_encoding::HEXLOWER.decode(nonce.as_bytes()).ok())
                .unwrap_or_default();
            let body = serde_json::json!({
                "serverSession": "other-session",
                "projectPath": project,
                "port": port,
                "proof": crate::web::studio::token_proof(
                    RECORDED_TOKEN,
                    &nonce,
                    "other-session",
                    std::net::SocketAddr::from(([127, 0, 0, 2], port)),
                ),
            })
            .to_string();
            format!(
                "HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
                body.len()
            )
        });
        let state = recorded(dir.path(), relay, std::process::id());
        let reason = verify(&state).unwrap_err();
        assert!(reason.contains("could not prove"), "{reason}");
    }

    #[test]
    fn the_default_port_must_prove_it_holds_the_shared_token() {
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join(TOKEN_FILE);
        fs::write(&token_file, RECORDED_TOKEN).unwrap();

        // Something without the token listens where the token would go.
        let (address, request) = answer_proof(
            "not-the-token-not-the-token-not-the",
            "any-session",
            dir.path().join("default.project.json"),
            Duration::ZERO,
        );
        let error = use_default(&address, Some(token_file.clone())).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("could not prove"), "{text}");
        assert!(text.contains("--address and --token-file"), "{text}");
        assert_eq!(super::super::exit_status(&error).0, 2);
        let request = request.join().unwrap();
        assert!(!request.to_ascii_lowercase().contains("x-rojo-studio-token"));
        assert!(!request.contains(RECORDED_TOKEN), "{request}");

        // An older bridge without the route fails closed too.
        let (address, _) = answer_raw(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
            Duration::ZERO,
        );
        assert!(use_default(&address, Some(token_file.clone())).is_err());

        // A bridge holding the token is used, whatever it serves.
        let (address, _) = answer_proof(
            RECORDED_TOKEN,
            "any-session",
            dir.path().join("other.project.json"),
            Duration::ZERO,
        );
        let (found, found_token) = use_default(&address, Some(token_file.clone())).unwrap();
        assert_eq!(found, address);
        assert_eq!(found_token, Some(token_file));

        // Without a shared token, nothing is sent and nothing is asked.
        assert_eq!(
            use_default("http://127.0.0.1:1", None).unwrap(),
            ("http://127.0.0.1:1".to_owned(), None)
        );
    }

    /// A process on the recorded port passes the nonce on to a real bridge
    /// on another port and returns its answer. The proof covers that
    /// bridge's port, so the relay is not trusted and never gets the token.
    #[test]
    fn a_relayed_proof_from_another_port_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (bridge, _) = answer_proof(
            RECORDED_TOKEN,
            "recorded-session",
            dir.path().join("default.project.json"),
            Duration::ZERO,
        );
        let (relay, relayed) = answer_with(Duration::ZERO, move |request, _| {
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let body = reqwest::blocking::get(format!("{bridge}{path}"))
                .unwrap()
                .text()
                .unwrap();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        });
        let state = recorded(dir.path(), relay.clone(), std::process::id());
        let reason = verify(&state).unwrap_err();
        assert!(reason.contains("could not prove"), "{reason}");
        let request = relayed.join().unwrap();
        assert!(!request.to_ascii_lowercase().contains("x-rojo-studio-token"));
        assert!(!request.contains(RECORDED_TOKEN), "{request}");

        // The fallback to the default port checks the same way.
        let (bridge, _) = answer_proof(
            RECORDED_TOKEN,
            "any-session",
            dir.path().join("default.project.json"),
            Duration::ZERO,
        );
        let (relay, _) = answer_with(Duration::ZERO, move |request, _| {
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let body = reqwest::blocking::get(format!("{bridge}{path}"))
                .unwrap()
                .text()
                .unwrap();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        });
        assert!(use_default(&relay, Some(state.token_file.clone())).is_err());
    }

    #[test]
    fn a_process_on_the_recorded_port_never_receives_the_token() {
        let dir = tempfile::tempdir().unwrap();
        // Something without the token took the port and repeats what the
        // record says, which anyone who can read it knows.
        let (address, request) = answer_proof(
            "not-the-token-not-the-token-not-the",
            "recorded-session",
            dir.path().join("default.project.json"),
            Duration::ZERO,
        );
        let state = recorded(dir.path(), address, std::process::id());
        let reason = verify(&state).unwrap_err();
        assert!(reason.contains("could not prove"), "{reason}");
        let request = request.join().unwrap();
        assert!(!request.to_ascii_lowercase().contains("x-rojo-studio-token"));
        assert!(!request.contains(RECORDED_TOKEN), "{request}");
    }

    #[test]
    fn another_project_with_a_matching_session_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let (address, _) = answer_proof(
            RECORDED_TOKEN,
            "recorded-session",
            dir.path().join("other.project.json"),
            Duration::ZERO,
        );
        let state = recorded(dir.path(), address, std::process::id());
        assert!(verify(&state).is_err());
    }

    #[test]
    fn the_recorded_server_is_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let (address, _) = answer_proof(
            RECORDED_TOKEN,
            "recorded-session",
            dir.path().join("default.project.json"),
            Duration::ZERO,
        );
        let state = recorded(dir.path(), address, std::process::id());
        assert_eq!(verify(&state), Ok(()));
    }

    #[test]
    fn concurrent_first_launches_agree_on_one_complete_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("state").join(TOKEN_FILE));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    ensure_token(&path, true).unwrap();
                    fs::read_to_string(&*path).unwrap()
                })
            })
            .collect();
        let tokens: HashSet<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(tokens.len(), 1, "{tokens:?}");
        assert!(is_created_token(tokens.iter().next().unwrap()));
        // Only the token and its lock are left, no temporary files.
        let mut names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [format!(".{TOKEN_FILE}.lock"), TOKEN_FILE.to_owned()]
        );
    }

    #[test]
    fn an_existing_token_is_kept_and_an_invalid_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        // A token passed with --studio-token-file may be any the bridge
        // accepts.
        fs::write(&path, "0123456789abcdef0123456789abcdef\n").unwrap();
        ensure_token(&path, false).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "0123456789abcdef0123456789abcdef\n"
        );
        fs::write(&path, "short").unwrap();
        let error = ensure_token(&path, false).unwrap_err();
        assert!(
            error.to_string().contains("not a valid Studio token"),
            "{error}"
        );
    }

    #[test]
    fn a_created_token_must_be_exactly_what_rojo_dev_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        let full = "0123456789abcdef".repeat(4);
        fs::write(&path, &full).unwrap();
        ensure_token(&path, true).unwrap();
        // A prefix of one, as a write cut short would leave, is valid for the
        // bridge but not what rojo dev writes, and is refused at once.
        for contents in [&full[..40], &format!("{full}\n"), &full.to_uppercase()] {
            fs::write(&path, contents).unwrap();
            let started = Instant::now();
            let error = ensure_token(&path, true).unwrap_err();
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(
                error.to_string().contains("not a valid Studio token"),
                "{error}"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        }
    }

    #[test]
    fn a_failed_sourcemap_write_is_retried_until_it_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        // The folder does not exist yet, so writing fails.
        let folder = dir.path().join("later");
        let writer = SourcemapWriter::new(folder.join(SOURCEMAP_FILE));
        assert!(writer.retry_if_dirty(|| unreachable!()).is_none());

        assert!(writer
            .generate_and_write(|| Ok(b"{\"a\":1}".to_vec()))
            .is_err());
        assert!(writer.is_dirty());
        // Still failing: stays dirty.
        assert!(writer
            .retry_if_dirty(|| Ok(b"{\"a\":1}".to_vec()))
            .unwrap()
            .is_err());
        assert!(writer.is_dirty());

        fs::create_dir(&folder).unwrap();
        let retried = writer.retry_if_dirty(|| Ok(b"{\"a\":2}".to_vec()));
        assert!(retried.unwrap().unwrap());
        assert!(!writer.is_dirty());
        assert_eq!(fs::read(folder.join(SOURCEMAP_FILE)).unwrap(), b"{\"a\":2}");
        assert!(writer.retry_if_dirty(|| unreachable!()).is_none());
    }

    #[test]
    fn a_failed_generation_also_marks_the_sourcemap_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let writer = SourcemapWriter::new(dir.path().join(SOURCEMAP_FILE));
        assert!(writer
            .generate_and_write(|| anyhow::bail!("tree is busy"))
            .is_err());
        assert!(writer.is_dirty());
    }

    #[test]
    fn an_unconfirmed_record_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let state = recorded(dir.path(), "http://127.0.0.1:9".to_owned(), pid);
        let error = use_record(&dir.path().join(STATE_FILE), Ok(state)).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("could not be confirmed"), "{text}");
        assert!(text.contains("--address and --token-file"), "{text}");
        assert!(text.contains("Restart `rojo dev`"), "{text}");
        // A usage problem, so the command exits with 2.
        assert_eq!(super::super::exit_status(&error).0, 2);
    }

    #[test]
    fn a_record_of_a_server_on_another_address_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = recorded(
            dir.path(),
            "http://192.168.1.20:34900".to_owned(),
            std::process::id(),
        );
        state.discoverable = false;
        let path = dir.path().join(STATE_FILE);
        state.write(&path).unwrap();

        let checkout = Checkout {
            root: dir.path().to_owned(),
            primary: true,
            in_git: true,
            shared_state: dir.path().to_owned(),
            worktree_state: dir.path().to_owned(),
        };
        let (found, state) = find_running(&checkout, dir.path()).unwrap();
        let error = use_record(&found, state).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("not a loopback address"), "{text}");
        assert!(text.contains("--address and --token-file"), "{text}");
        assert_eq!(super::super::exit_status(&error).0, 2);

        // Records from before the field are reachable ones.
        let old: DevState = serde_json::from_str(
            r#"{"pid":1,"address":"http://127.0.0.1:1","tokenFile":"t","project":"p","serverSession":"s"}"#,
        )
        .unwrap();
        assert!(old.discoverable);
    }

    #[test]
    fn a_confirmed_record_names_its_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let (address, _) = answer_proof(
            RECORDED_TOKEN,
            "recorded-session",
            dir.path().join("default.project.json"),
            Duration::ZERO,
        );
        let state = recorded(dir.path(), address.clone(), std::process::id());
        let token = state.token_file.clone();
        let (found, found_token) = use_record(&dir.path().join(STATE_FILE), Ok(state)).unwrap();
        assert_eq!(found, address);
        assert_eq!(found_token, Some(token));
    }

    #[test]
    fn verification_outlasts_the_bridges_git_wait() {
        assert!(VERIFY_TIMEOUT > crate::web::studio::GIT_WAIT_LIMIT);
        // An answer slower than the old 5-second limit is still accepted.
        let dir = tempfile::tempdir().unwrap();
        let (address, _) = answer_proof(
            RECORDED_TOKEN,
            "recorded-session",
            dir.path().join("default.project.json"),
            Duration::from_secs(6),
        );
        let state = recorded(dir.path(), address, std::process::id());
        assert_eq!(verify(&state), Ok(()));
    }

    #[test]
    fn verification_never_follows_a_redirect_with_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let (target, target_address) = redirect_target();
        let (address, _) = answer_raw(
            format!(
                "HTTP/1.1 302 Found\r\nLocation: {target_address}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            Duration::ZERO,
        );
        let state = recorded(dir.path(), address, std::process::id());
        let reason = verify(&state).unwrap_err();
        assert!(reason.contains("HTTP 302"), "{reason}");
        assert!(!was_contacted(&target), "the redirect was followed");
    }

    #[test]
    fn bridge_clients_do_not_follow_redirects() {
        let (target, target_address) = redirect_target();
        let (address, _) = answer_raw(
            format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: {target_address}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            Duration::ZERO,
        );
        let client = studio::bridge_client(Duration::from_secs(5)).unwrap();
        let response = client
            .get(format!("{address}/api/studio/sessions"))
            .header("x-rojo-studio-token", "secret")
            .send()
            .unwrap();
        assert_eq!(response.status().as_u16(), 307);
        assert!(!was_contacted(&target), "the redirect was followed");
    }

    #[test]
    fn verification_only_talks_to_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let state = recorded(
            dir.path(),
            "http://192.168.1.2:34872".to_owned(),
            std::process::id(),
        );
        let reason = verify(&state).unwrap_err();
        assert!(reason.contains("loopback"), "{reason}");
    }

    #[test]
    fn a_newer_sourcemap_is_never_replaced_by_an_older_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOURCEMAP_FILE);
        let writer = Arc::new(SourcemapWriter::new(path.clone()));
        let (started, wait_started) = std::sync::mpsc::channel();
        let (release, wait_release) = std::sync::mpsc::channel::<()>();

        // The older update generates first, then stalls before writing.
        let older = {
            let writer = Arc::clone(&writer);
            thread::spawn(move || {
                writer
                    .generate_and_write(|| {
                        started.send(()).unwrap();
                        wait_release.recv().unwrap();
                        Ok(b"old".to_vec())
                    })
                    .unwrap()
            })
        };
        wait_started.recv().unwrap();
        // A newer update must wait for the older one instead of writing
        // first and then being overwritten.
        let newer = {
            let writer = Arc::clone(&writer);
            thread::spawn(move || writer.generate_and_write(|| Ok(b"new".to_vec())).unwrap())
        };
        thread::sleep(Duration::from_millis(200));
        assert!(!newer.is_finished(), "the newer update did not wait");
        release.send(()).unwrap();
        older.join().unwrap();
        newer.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(!writer.is_dirty());
    }

    #[test]
    fn an_automatic_port_taken_before_the_bind_is_skipped() {
        // Something takes the first choice between the probe and the bind.
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken_port = taken.local_addr().unwrap().port();
        let first = WORKTREE_PORT_FIRST + 10;
        let mut tried = Vec::new();
        let (_listener, port, source) = bind_port(
            &request(None, None, false),
            |_| true,
            |port| {
                tried.push(port);
                if port == first {
                    TcpListener::bind(("127.0.0.1", taken_port))
                } else {
                    TcpListener::bind("127.0.0.1:0")
                }
            },
        )
        .unwrap();
        assert_eq!((port, source), (first + 1, PortSource::Worktree));
        assert_eq!(tried, vec![first, first + 1]);
    }

    #[test]
    fn an_explicit_port_taken_before_the_bind_is_an_error() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken_port = taken.local_addr().unwrap().port();
        let mut attempts = 0;
        let error = bind_port(
            &request(Some(taken_port), None, true),
            |_| true,
            |port| {
                attempts += 1;
                TcpListener::bind(("127.0.0.1", port))
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert!(
            error.to_string().contains(&taken_port.to_string()),
            "{error}"
        );
    }

    #[test]
    fn port_zero_is_refused() {
        let error = pick_port(&request(Some(0), None, true), |_| true).unwrap_err();
        assert!(error.to_string().contains("--port 0"), "{error}");
        assert_eq!(super::super::exit_status(&error).0, 2);
        let error = pick_port(&request(None, Some(0), true), |_| true).unwrap_err();
        assert!(error.to_string().contains("servePort 0"), "{error}");
        // --port wins over the project's servePort 0, as it does over any.
        let picked = pick_port(&request(Some(5000), Some(0), true), |_| true).unwrap();
        assert_eq!(picked, (5000, PortSource::Flag));
    }

    /// A port nothing listens on for either loopback address right now.
    fn free_port() -> u16 {
        loop {
            let port = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            if port_is_free(Ipv4Addr::LOCALHOST.into(), port) {
                return port;
            }
        }
    }

    #[test]
    fn the_other_loopback_address_is_held_without_accepting() {
        let port = free_port();
        let (listener, companion) = bind_with_companion(Ipv4Addr::LOCALHOST.into(), port).unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), port);
        let Some(companion) = companion else {
            // No IPv6 on this machine, so `localhost` cannot be ambiguous.
            return;
        };
        // Nothing else can listen there while it is held.
        let taken = TcpListener::bind(("::1", port)).unwrap_err();
        assert_eq!(taken.kind(), std::io::ErrorKind::AddrInUse);
        // A client trying it is refused rather than left waiting, so it moves
        // on to 127.0.0.1.
        let connected = std::net::TcpStream::connect_timeout(
            &SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
            Duration::from_secs(5),
        );
        assert!(connected.is_err());
        drop(companion);
        drop(TcpListener::bind(("::1", port)).unwrap());
    }

    #[test]
    fn a_server_on_the_ipv6_wildcard_also_holds_ipv4_loopback() {
        let port = free_port();
        let Ok((listener, _companion)) = bind_with_companion(Ipv6Addr::UNSPECIFIED.into(), port)
        else {
            // No IPv6 on this machine.
            return;
        };
        // Whether the listener is dual-stack or IPv6-only (Windows' default),
        // no other server can take 127.0.0.1 on the same port.
        let taken = TcpListener::bind(("127.0.0.1", port));
        assert!(taken.is_err(), "127.0.0.1:{port} was free");
        drop(listener);
    }

    #[test]
    fn a_taken_other_loopback_address_is_a_lost_race() {
        let port = free_port();
        let Ok(other) = TcpListener::bind(("::1", port)) else {
            return;
        };
        let error = bind_with_companion(Ipv4Addr::LOCALHOST.into(), port).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        // The listener on 127.0.0.1 was let go again.
        drop(TcpListener::bind(("127.0.0.1", port)).unwrap());

        // An automatic port moves on to the next one; an explicit one fails.
        let first = WORKTREE_PORT_FIRST + 10;
        let mut tried = Vec::new();
        let (_, chosen, _) = bind_port(
            &request(None, None, false),
            |_| true,
            |candidate| {
                tried.push(candidate);
                if candidate == first {
                    bind_with_companion(Ipv4Addr::LOCALHOST.into(), port)
                } else {
                    bind_with_companion(Ipv4Addr::LOCALHOST.into(), free_port())
                }
            },
        )
        .unwrap();
        assert_eq!(chosen, first + 1);
        assert_eq!(tried, vec![first, first + 1]);
        let error = bind_port(
            &request(Some(port), None, true),
            |_| true,
            |candidate| bind_with_companion(Ipv4Addr::LOCALHOST.into(), candidate),
        )
        .unwrap_err();
        assert!(error.to_string().contains(&port.to_string()), "{error}");
        drop(other);
    }

    #[test]
    fn automatic_bind_retries_are_bounded() {
        let mut attempts = 0;
        let error = bind_port(
            &request(None, None, false),
            |_| true,
            |_| -> std::io::Result<TcpListener> {
                attempts += 1;
                Err(std::io::ErrorKind::AddrInUse.into())
            },
        )
        .unwrap_err();
        assert_eq!(attempts, BIND_ATTEMPTS);
        assert!(error.to_string().contains("Could not listen"), "{error}");
    }
}
