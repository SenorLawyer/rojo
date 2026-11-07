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
        let (port, port_source) = pick_port(
            &PortRequest {
                flag: self.port,
                project: session.project_port(),
                prefer_default: checkout.primary,
                slot: worktree_slot(&checkout.root),
            },
            |port| port_is_free(ip, port),
        )?;

        let token_file = match self.studio_token_file {
            Some(path) => resolve_path(&path)?.into_owned(),
            None => checkout.shared_state.join(TOKEN_FILE),
        };
        ensure_token(&token_file)?;

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
            if let Err(err) = writer.update(&session) {
                log::warn!("Could not write {}: {err:#}", writer.path.display());
            }
            spawn_sourcemap_watcher(Arc::clone(&session), Arc::clone(&writer));
            Some(writer)
        };

        let studio_address = studio_address(ip, port);
        let state_path = checkout.worktree_state.join(STATE_FILE);
        let state = studio_address.as_ref().map(|address| DevState {
            pid: std::process::id(),
            address: address.clone(),
            token_file: token_file.clone(),
            project: session.root_project().file_location.clone(),
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
        let result = server.start_until(
            (ip, port).into(),
            allowed_hosts,
            || {
                if let Some(state) = &state {
                    if let Err(err) = state.write(&state_path) {
                        log::warn!(
                            "Could not record this server in {}, so `rojo studio` needs --address: {err:#}",
                            state_path.display()
                        );
                    }
                }
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
        if state.is_some() {
            DevState::remove_own(&state_path);
        }
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
    project: PathBuf,
}

impl DevState {
    fn write(&self, path: &Path) -> anyhow::Result<()> {
        let parent = path.parent().context("State file has no folder")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("Could not create {}", parent.display()))?;
        write_atomically(path, &serde_json::to_vec_pretty(self)?)
    }

    fn read(path: &Path) -> Option<DevState> {
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }

    /// Removes the state file unless another `rojo dev` has replaced it
    /// since, which a second one in the same worktree does.
    fn remove_own(path: &Path) {
        if DevState::read(path).is_some_and(|state| state.pid == std::process::id()) {
            let _ = fs::remove_file(path);
        }
    }
}

/// The Studio bridge address and token a command should use: the flags when
/// given, then what a `rojo dev` running in the worktree around `start`
/// recorded, then the default address and the token `rojo dev` creates.
/// Nothing here fails: when discovery does not work, the command falls back
/// to the defaults and reports a connection or token error as before.
pub(super) fn bridge_target(
    address: Option<&str>,
    token_file: Option<&Path>,
    start: &Path,
) -> (String, Option<PathBuf>) {
    if let (Some(address), Some(token_file)) = (address, token_file) {
        return (address.to_owned(), Some(token_file.to_owned()));
    }
    // `rojo mcp --project` may name the project file rather than its folder.
    let start = match start.parent() {
        Some(parent) if start.is_file() => parent,
        _ => start,
    };
    let checkout = Checkout::discover(start).ok();
    let running = checkout
        .as_ref()
        .and_then(|checkout| find_running(checkout, start));
    let address = address
        .map(str::to_owned)
        .or_else(|| running.as_ref().map(|state| state.address.clone()))
        .unwrap_or_else(|| DEFAULT_STUDIO_ADDRESS.to_owned());
    let token_file = token_file
        .map(Path::to_owned)
        .or_else(|| running.map(|state| state.token_file))
        .or_else(|| {
            let default = checkout?.shared_state.join(TOKEN_FILE);
            default.is_file().then_some(default)
        });
    (address, token_file)
}

/// The state a running `rojo dev` recorded for this worktree. Outside Git it
/// is keyed by the project folder, so the folders above `start` are tried
/// too, for commands run in a subfolder of the project.
fn find_running(checkout: &Checkout, start: &Path) -> Option<DevState> {
    if checkout.in_git {
        return DevState::read(&checkout.worktree_state.join(STATE_FILE));
    }
    let start = dunce::canonicalize(start).unwrap_or_else(|_| start.to_owned());
    start
        .ancestors()
        .find_map(|dir| DevState::read(&super::project_state_dir(dir).ok()?.join(STATE_FILE)))
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

/// Picks the port to serve on. A port from `--port` or the project's
/// `servePort` is used as given, and an error when taken, since the user or
/// the project asked for exactly that port and the plugin is set up for it.
/// Otherwise the default port (when preferred), then the worktree's slot and
/// the ports after it, wrapping around the range.
fn pick_port(
    request: &PortRequest,
    is_free: impl Fn(u16) -> bool,
) -> anyhow::Result<(u16, PortSource)> {
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

/// Whether nothing listens on `port` for `ip`. The wildcard address is tried
/// too: on Windows, binding 127.0.0.1 can succeed while another server holds
/// 0.0.0.0 on the same port, and the plugin would then reach that one.
fn port_is_free(ip: IpAddr, port: u16) -> bool {
    let wildcard: IpAddr = match ip {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    TcpListener::bind(SocketAddr::new(ip, port)).is_ok()
        && (ip == wildcard || TcpListener::bind(SocketAddr::new(wildcard, port)).is_ok())
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

fn ensure_token(path: &Path) -> anyhow::Result<()> {
    if path.is_file() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Could not create {}", parent.display()))?;
    }
    studio::create_token(path)
        .with_context(|| format!("Could not create the Studio token {}", path.display()))
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
}

impl SourcemapWriter {
    fn new(path: PathBuf) -> Self {
        SourcemapWriter {
            path,
            last: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Vec<u8>>> {
        self.last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Regenerates the sourcemap and writes it when it differs from the last
    /// one. Returns whether the file was written.
    fn update(&self, session: &ServeSession) -> anyhow::Result<bool> {
        let mut bytes = Vec::new();
        sourcemap::write_sourcemap_json(session, &mut bytes, false, &HashMap::new())?;
        self.write_if_changed(bytes)
    }

    fn write_if_changed(&self, bytes: Vec<u8>) -> anyhow::Result<bool> {
        let mut last = self.lock();
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

fn spawn_sourcemap_watcher(session: Arc<ServeSession>, writer: Arc<SourcemapWriter>) {
    thread::Builder::new()
        .name("rojo dev sourcemap".to_owned())
        .spawn(move || {
            let mut cursor = session.message_queue().cursor();
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
fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
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
    use std::collections::HashSet;

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
        };
        state.write(&path).unwrap();
        assert_eq!(DevState::read(&path), Some(state.clone()));
        DevState::remove_own(&path);
        assert!(path.exists(), "another process's state was removed");
        state.pid = std::process::id();
        state.write(&path).unwrap();
        DevState::remove_own(&path);
        assert!(!path.exists());
    }

    #[test]
    fn explicit_flags_skip_discovery() {
        let (address, token) = bridge_target(
            Some("http://127.0.0.1:1"),
            Some(Path::new("token")),
            Path::new("/does/not/exist"),
        );
        assert_eq!(address, "http://127.0.0.1:1");
        assert_eq!(token, Some(PathBuf::from("token")));
    }
}
