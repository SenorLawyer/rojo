//! `rojo doctor`: one read-only report of the checks users otherwise piece
//! together from `rojo serve` failures, `rojo packages doctor`, the plugins
//! folder, connection errors in Studio and Git's version.

use std::{
    fs,
    io::Read,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Parser;
use memofs::Vfs;
use rbx_dom_weak::{types::Variant, Ustr};
use roblox_install::RobloxStudio;
use serde::{de::IgnoredAny, Deserialize, Serialize};

use crate::{
    serve_session::ServeSession,
    web::{
        interface::PROTOCOL_VERSION,
        model_check,
        studio::{IMPORT_CHECK_LIMIT, MAX_MODEL_BYTES},
        util::deserialize_msgpack,
    },
    Project,
};

use super::{lifecycle, packages, plugin, resolve_path, serve, ExitStatus};

const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the server check waits. Doctor only asks for `/api/rojo`, which a
/// healthy server answers at once, so a slow answer is itself worth reporting
/// and must not make doctor hang.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_millis(1500);
/// `/api/rojo` answers with well under a kilobyte. Whatever listens on the
/// port may not be Rojo, so doctor reads no more than this of its answer.
const MAX_INFO_BYTES: usize = 1024 * 1024;

/// The first Git release with `rev-parse --path-format`, which the lifecycle
/// commands need (see `lifecycle::git_common_dir`).
const MIN_GIT: (u32, u32) = (2, 31);

/// Check the project, packages, Studio plugin, a running `rojo serve` and Git,
/// and print one report. Changes nothing. Exits with 1 when a check fails.
#[derive(Debug, Parser)]
pub struct DoctorCommand {
    /// Project directory or project file, as for `rojo serve`. Defaults to the
    /// current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Address of the `rojo serve` to check. Defaults to the project's
    /// `serveAddress`, or `127.0.0.1`.
    #[clap(long)]
    pub address: Option<IpAddr>,

    /// Port of the `rojo serve` to check. Defaults to the project's
    /// `servePort`, or `34872`.
    #[clap(long)]
    pub port: Option<u16>,

    /// Check this plugins folder instead of discovering Studio's.
    #[clap(long)]
    pub plugins_dir: Option<PathBuf>,

    /// Emit a versioned JSON report instead of text.
    #[clap(long)]
    pub json: bool,
}

impl DoctorCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let project_path = resolve_path(&self.project)?.into_owned();
        let report = build_report(&Inputs {
            project_path,
            address: self.address,
            port: self.port,
            plugins_dir: self.plugins_dir,
        });
        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", render_text(&report));
        }
        if report.ok {
            Ok(())
        } else {
            // The report already says what failed, so exit without logging
            // a second error.
            Err(ExitStatus(1).into())
        }
    }
}

struct Inputs {
    project_path: PathBuf,
    address: Option<IpAddr>,
    port: Option<u16>,
    plugins_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Info,
    Warning,
    Error,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Info => "info",
            Status::Warning => "warning",
            Status::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Category {
    Project,
    ServePlaceIds,
    Packages,
    Plugin,
    Server,
    Git,
}

impl Category {
    const ALL: [Category; 6] = [
        Category::Project,
        Category::ServePlaceIds,
        Category::Packages,
        Category::Plugin,
        Category::Server,
        Category::Git,
    ];

    fn title(self) -> &'static str {
        match self {
            Category::Project => "Project",
            Category::ServePlaceIds => "servePlaceIds",
            Category::Packages => "Packages",
            Category::Plugin => "Studio plugin",
            Category::Server => "rojo serve",
            Category::Git => "Git",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    category: Category,
    /// A stable identifier for scripts, such as `plugin-outdated`.
    code: String,
    status: Status,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

fn check(
    category: Category,
    code: impl Into<String>,
    status: Status,
    message: impl Into<String>,
    hint: Option<&str>,
) -> Check {
    Check {
        category,
        code: code.into(),
        status,
        message: message.into(),
        hint: hint.map(str::to_owned),
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Counts {
    ok: usize,
    info: usize,
    warning: usize,
    error: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    schema_version: u32,
    rojo_version: &'static str,
    project_file: Option<PathBuf>,
    checks: Vec<Check>,
    counts: Counts,
    /// False when any check is an error.
    ok: bool,
}

impl Report {
    fn new(checks: Vec<Check>, project_file: Option<PathBuf>) -> Self {
        let mut counts = Counts::default();
        for check in &checks {
            match check.status {
                Status::Ok => counts.ok += 1,
                Status::Info => counts.info += 1,
                Status::Warning => counts.warning += 1,
                Status::Error => counts.error += 1,
            }
        }
        Report {
            schema_version: 1,
            rojo_version: CLI_VERSION,
            project_file,
            ok: counts.error == 0,
            counts,
            checks,
        }
    }
}

fn build_report(inputs: &Inputs) -> Report {
    let mut checks = Vec::new();
    let project = check_project(&inputs.project_path, &mut checks);
    let project = project.as_ref();

    match project {
        Some(project) => checks.extend(check_place_ids(project)),
        None => checks.push(check(
            Category::ServePlaceIds,
            "not-checked",
            Status::Info,
            "Not checked, because the project could not be loaded.",
            None,
        )),
    }

    // Packages live next to the project file, and `packages doctor` refuses
    // paths that do not exist; the project check has reported those already.
    let package_path = project
        .map(|project| project.file_location.clone())
        .unwrap_or_else(|| inputs.project_path.clone());
    checks.extend(check_packages(&package_path));

    let plugins_dir = match &inputs.plugins_dir {
        Some(dir) => Ok(dir.clone()),
        None => RobloxStudio::locate()
            .map(|studio| studio.plugins_path().to_path_buf())
            .map_err(|err| err.to_string()),
    };
    checks.extend(check_plugin(plugins_dir, inputs.plugins_dir.is_some()));

    let address = server_address(inputs.address, inputs.port, project);
    checks.extend(check_server(
        address,
        fetch_server_info(address),
        project.and_then(|project| project.name.as_deref()),
    ));

    let git_dir = project
        .map(|project| project.folder_location().to_path_buf())
        .unwrap_or_else(|| existing_ancestor(&inputs.project_path));
    checks.extend(check_git(&git_dir, &GitRunner));

    Report::new(checks, project.map(|project| project.file_location.clone()))
}

/// Parses the project file with the loader `rojo serve` uses, then builds the
/// first snapshot as serve does, so a broken `$path` fails here the same way
/// it would there. The parsed project is returned even when the snapshot
/// fails: its settings (servePort, servePlaceIds, the name) are still what a
/// fixed project would be served with, so the other checks keep using them.
fn check_project(path: &Path, checks: &mut Vec<Check>) -> Option<Project> {
    let vfs = Vfs::new_default_without_watch();
    // ServeSession canonicalizes too; doing it first keeps reported paths in
    // one form. A path that does not exist is left alone and fails below.
    let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let path = canonical.as_path();
    let project = match Project::load_initial_project(&vfs, path) {
        Ok(project) => project,
        Err(err) => {
            let err = anyhow::Error::from(err);
            checks.push(check(
                Category::Project,
                "project-invalid",
                Status::Error,
                // Some errors, such as a missing path, do not name the path.
                format!("Could not load the project at {}: {err:#}", path.display()),
                Some(
                    "Run doctor in the project's folder, pass its .project.json path, or create one with `rojo init`. `rojo serve` fails the same way.",
                ),
            ));
            return None;
        }
    };
    let name = project.name.as_deref().unwrap_or("unnamed");
    match ServeSession::new(vfs, path) {
        Ok(session) => {
            let class_name = {
                let tree = session.tree();
                tree.get_instance(tree.get_root_id())
                    .map(|root| root.class_name().to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            };
            checks.push(check(
                Category::Project,
                "project-loaded",
                Status::Ok,
                format!(
                    "{} loads; its root is a {class_name} named {name}.",
                    project.file_location.display(),
                ),
                None,
            ));
        }
        Err(err) => {
            let err = anyhow::Error::from(err);
            checks.push(check(
                Category::Project,
                "snapshot-failed",
                Status::Error,
                format!(
                    "{} parses, but its instances cannot be built: {err:#}",
                    project.file_location.display()
                ),
                Some("Fix or create the files its $path entries name. `rojo serve` fails the same way."),
            ));
        }
    }
    Some(project)
}

/// Mirrors how the plugin uses `servePlaceIds`: when present, Studio refuses
/// to sync any place whose ID is not listed, so an empty list blocks every
/// place. `blockedPlaceIds` is checked after it and wins.
fn check_place_ids(project: &Project) -> Vec<Check> {
    let category = Category::ServePlaceIds;
    let mut checks = Vec::new();
    match &project.serve_place_ids {
        None => checks.push(check(
            category,
            "absent",
            Status::Warning,
            "servePlaceIds is not set, so any place open in Studio can connect and be synced.",
            Some("Add \"servePlaceIds\": [<your place ID>] to the project file to stop syncing into the wrong place."),
        )),
        Some(ids) if ids.is_empty() => checks.push(check(
            category,
            "empty",
            Status::Error,
            "servePlaceIds is an empty list, so the plugin refuses to sync every place.",
            Some("List the place IDs that may sync, include 0 to allow unpublished places, or remove the field."),
        )),
        Some(ids) => {
            let mut sorted: Vec<_> = ids.iter().copied().collect();
            sorted.sort_unstable();
            let list = sorted
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let unpublished = if ids.contains(&0) {
                " Unpublished places (ID 0) are allowed."
            } else {
                ""
            };
            checks.push(check(
                category,
                "present",
                Status::Ok,
                format!("Only these places can sync: {list}.{unpublished}"),
                None,
            ));
        }
    }
    if let (Some(allowed), Some(blocked)) = (&project.serve_place_ids, &project.blocked_place_ids) {
        let mut both: Vec<_> = allowed.intersection(blocked).copied().collect();
        if !both.is_empty() {
            both.sort_unstable();
            let list = both
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            checks.push(check(
                category,
                "blocked-and-allowed",
                Status::Warning,
                format!("{list} is in both servePlaceIds and blockedPlaceIds, so it cannot sync."),
                Some("Remove it from one of the two lists."),
            ));
        }
    }
    checks
}

/// Folds in `rojo packages doctor`'s findings. Its project findings are left
/// out because the project check covers them, and a project without a
/// package manager is fine here, so that finding is only information.
fn check_packages(path: &Path) -> Vec<Check> {
    let category = Category::Packages;
    let Ok(report) = packages::inspect(path) else {
        return vec![check(
            category,
            "not-checked",
            Status::Info,
            "Not checked, because the project path could not be opened.",
            None,
        )];
    };
    let mut checks = Vec::new();
    for finding in &report.findings {
        let status = match (finding.code, finding.severity) {
            ("project-missing" | "project-invalid", _) => continue,
            ("manifest-missing", _) => Status::Info,
            (_, "error") => Status::Error,
            (_, "warning") => Status::Warning,
            _ => Status::Info,
        };
        let hint = (status >= Status::Warning)
            .then_some("Run `rojo packages doctor` for the full package report.");
        checks.push(check(
            category,
            finding.code,
            status,
            finding.message.clone(),
            hint,
        ));
    }
    if !report.managers.is_empty()
        && !checks
            .iter()
            .any(|check: &Check| check.status >= Status::Warning)
    {
        let names: Vec<_> = report
            .managers
            .iter()
            .map(|manager| manager.manager.name())
            .collect();
        checks.push(check(
            category,
            "packages-ok",
            Status::Ok,
            format!(
                "{} packages and their Rojo mappings look right.",
                names.join(" and ")
            ),
            None,
        ));
    }
    checks
}

/// `explicit` is whether the folder came from `--plugins-dir`, in which case
/// every repair command has to name it too, or it would act on Studio's own
/// folder instead.
fn check_plugin(plugins_dir: Result<PathBuf, String>, explicit: bool) -> Vec<Check> {
    let category = Category::Plugin;
    let plugins_dir = match plugins_dir {
        Ok(dir) => dir,
        Err(err) => {
            return vec![check(
                category,
                "studio-not-found",
                Status::Warning,
                format!("Could not find Roblox Studio's plugins folder: {err}"),
                Some("Pass --plugins-dir with the folder Studio loads plugins from."),
            )]
        }
    };
    let flag = if explicit {
        format!(" --plugins-dir \"{}\"", plugins_dir.display())
    } else {
        String::new()
    };
    let install = format!("`rojo plugin install{flag}`");
    let mut checks = Vec::new();
    let managed = plugins_dir.join(plugin::PLUGIN_FILE_NAME);
    match read_plugin(&managed) {
        Ok(Some(bytes)) => match plugin_version(bytes) {
            Ok(version) if version == CLI_VERSION => checks.push(check(
                category,
                "plugin-current",
                Status::Ok,
                format!(
                    "{} is version {version}, the same as this CLI.",
                    managed.display()
                ),
                None,
            )),
            Ok(version) => checks.push(check(
                category,
                "plugin-version-mismatch",
                Status::Warning,
                format!(
                    "{} is version {version}, but this CLI is {CLI_VERSION}.",
                    managed.display()
                ),
                Some(&format!(
                    "Run {install} with this CLI, then restart Studio."
                )),
            )),
            Err(reason) => checks.push(check(
                category,
                "plugin-unreadable",
                Status::Warning,
                format!(
                    "{} is not a Rojo plugin model doctor can read: {reason}",
                    managed.display()
                ),
                Some(&format!(
                    "Run {install} to replace it, then restart Studio."
                )),
            )),
        },
        Ok(None) => checks.push(check(
            category,
            "plugin-missing",
            Status::Warning,
            format!("{} is not installed.", managed.display()),
            Some(&format!(
                "Run {install}, unless you installed Rojo's plugin another way."
            )),
        )),
        Err(reason) => checks.push(check(
            category,
            "plugin-unreadable",
            Status::Warning,
            format!("Could not read {}: {reason}", managed.display()),
            Some(&format!(
                "Run {install} to replace it, then restart Studio."
            )),
        )),
    }

    let mut copies: Vec<String> = fs::read_dir(&plugins_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| plugin::is_side_by_side_file_name(name))
        .collect();
    copies.sort();
    if !copies.is_empty() {
        checks.push(check(
            category,
            "side-by-side-copies",
            Status::Warning,
            format!(
                "Side-by-side copies in {}: {}. Studio loads each one as a separate Rojo plugin.",
                plugins_dir.display(),
                copies.join(", ")
            ),
            Some(&format!(
                "Remove the ones you no longer test with `rojo plugin uninstall --side-by-side{flag}` (this version) or `rojo plugin uninstall --all{flag}`."
            )),
        ));
    }
    checks
}

/// Reads at most the size limit for untrusted models, so a huge file in the
/// plugins folder is reported instead of read into memory. `None` when the
/// file does not exist.
fn read_plugin(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_MODEL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    if bytes.len() > MAX_MODEL_BYTES {
        return Err(format!(
            "it is larger than {} MiB",
            MAX_MODEL_BYTES / 1024 / 1024
        ));
    }
    Ok(Some(bytes))
}

/// The plugin's version, from the `Version` StringValue that the plugin's
/// Config reads at runtime (see plugin.project.json).
///
/// The file is untrusted: anything can be in the plugins folder, and
/// `rbx_binary` sizes allocations from counts the file declares, so a
/// corrupted model could abort doctor in a way no error handling catches.
/// It goes through the same check as `rojo studio import` instead: structural
/// checks, then decoding in a child process with a memory cap and a time
/// limit, which re-encodes it. Only that re-encoded model, written by Rojo's
/// own encoder from a tree that passed the checks, is decoded here.
fn plugin_version(bytes: Vec<u8>) -> Result<String, String> {
    let encoded = model_check::check_untrusted_model(bytes, true, true, IMPORT_CHECK_LIMIT)
        .map_err(|err| format!("{err:#}"))?;
    let dom = rbx_binary::from_reader(encoded.as_slice()).map_err(|err| err.to_string())?;
    let not_rojo = || "it has no Rojo/Version value".to_owned();
    let root = dom
        .root()
        .children()
        .iter()
        .filter_map(|id| dom.get_by_ref(*id))
        .find(|instance| instance.name == "Rojo")
        .ok_or_else(not_rojo)?;
    let version = root
        .children()
        .iter()
        .filter_map(|id| dom.get_by_ref(*id))
        .find(|instance| instance.name == "Version")
        .ok_or_else(not_rojo)?;
    match version.properties.get(&Ustr::from("Value")) {
        Some(Variant::String(value)) => Ok(value.trim().to_owned()),
        Some(Variant::BinaryString(value)) => {
            Ok(String::from_utf8_lossy(value.as_ref()).trim().to_owned())
        }
        _ => Err(not_rojo()),
    }
}

/// The address `rojo serve` would listen on for these options, made
/// connectable: a server bound to every interface is reached over loopback.
fn server_address(
    address: Option<IpAddr>,
    port: Option<u16>,
    project: Option<&Project>,
) -> SocketAddr {
    let ip = address
        .or_else(|| project.and_then(|project| project.serve_address))
        .unwrap_or(serve::DEFAULT_BIND_ADDRESS.into());
    let ip = match ip {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let port = port
        .or_else(|| project.and_then(|project| project.serve_port))
        .unwrap_or(serve::DEFAULT_PORT);
    SocketAddr::new(ip, port)
}

/// What the server check found at the address.
enum Probe {
    /// Nothing accepted the connection.
    NotRunning,
    /// Something accepted the connection but did not answer in time, such as
    /// a server that is stuck.
    NoAnswer,
    /// Something answered, but not with Rojo's server info.
    NotRojo(String),
    Rojo(ServerInfo),
}

/// The parts of `/api/rojo` doctor reads. This is deliberately looser than
/// [`crate::web::interface::ServerInfoResponse`] so that older servers still parse: upstream Rojo 7
/// answers with JSON and protocol 4, and doctor should report that as a
/// version mismatch rather than as "not Rojo".
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerInfo {
    server_version: String,
    protocol_version: u64,
    #[serde(default)]
    project_name: Option<String>,
    /// Only whether it is present matters. Its contents include the plugin's
    /// pairing token, so they are never kept or printed.
    #[serde(default)]
    studio_bridge: Option<IgnoredAny>,
}

/// Decodes `/api/rojo` as this fork's MessagePack, or as the JSON that
/// released upstream Rojo sends. The content type picks which to try first.
fn parse_server_info(content_type: Option<&str>, bytes: &[u8]) -> Option<ServerInfo> {
    let json = || serde_json::from_slice::<ServerInfo>(bytes).ok();
    let msgpack = || deserialize_msgpack::<ServerInfo>(bytes).ok();
    if content_type.is_some_and(|value| value.contains("json")) {
        json().or_else(msgpack)
    } else {
        msgpack().or_else(json)
    }
}

fn fetch_server_info(address: SocketAddr) -> Probe {
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .no_proxy()
        .build()
    {
        Ok(client) => client,
        Err(err) => return Probe::NotRojo(format!("could not create an HTTP client: {err}")),
    };
    let response = match client.get(format!("http://{address}/api/rojo")).send() {
        Ok(response) => response,
        Err(err) if err.is_connect() => return Probe::NotRunning,
        Err(err) if err.is_timeout() => return Probe::NoAnswer,
        Err(err) => return Probe::NotRojo(err.to_string()),
    };
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if !status.is_success() {
        return Probe::NotRojo(format!("answered {status}"));
    }
    let mut bytes = Vec::new();
    if let Err(err) = response
        .take(MAX_INFO_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
    {
        return match err.kind() {
            std::io::ErrorKind::TimedOut => Probe::NoAnswer,
            _ => Probe::NotRojo(err.to_string()),
        };
    }
    if bytes.len() > MAX_INFO_BYTES {
        return Probe::NotRojo(format!(
            "answered with more than {} MiB",
            MAX_INFO_BYTES / 1024 / 1024
        ));
    }
    match parse_server_info(content_type.as_deref(), &bytes) {
        Some(info) => Probe::Rojo(info),
        None => Probe::NotRojo(format!("answered {status} without Rojo's server info")),
    }
}

fn check_server(address: SocketAddr, probe: Probe, project_name: Option<&str>) -> Vec<Check> {
    let category = Category::Server;
    let info = match probe {
        Probe::NotRunning => {
            return vec![check(
                category,
                "not-running",
                Status::Info,
                format!("No rojo serve is listening on {address}."),
                Some("Start one with `rojo serve` to sync, or pass --address/--port to check another."),
            )]
        }
        Probe::NoAnswer => {
            return vec![check(
                category,
                "no-answer",
                Status::Warning,
                format!(
                    "Something on {address} accepted the connection but did not answer within {} ms.",
                    REQUEST_TIMEOUT.as_millis()
                ),
                Some("If it is a rojo serve, its terminal may show why it is stuck; restart it."),
            )]
        }
        Probe::NotRojo(reason) => {
            return vec![check(
                category,
                "not-rojo",
                Status::Warning,
                format!("Something on {address} is not a Rojo server ({reason})."),
                Some("Stop that program or serve on another port with `rojo serve --port`, and set the same port in the plugin."),
            )]
        }
        Probe::Rojo(info) => info,
    };
    let mut checks = Vec::new();
    if info.server_version == CLI_VERSION {
        checks.push(check(
            category,
            "server-current",
            Status::Ok,
            format!(
                "rojo serve {} is running on {address}, the same version as this CLI.",
                info.server_version
            ),
            None,
        ));
    } else {
        checks.push(check(
            category,
            "server-version-mismatch",
            Status::Warning,
            format!(
                "rojo serve on {address} is version {}, but this CLI is {CLI_VERSION}.",
                info.server_version
            ),
            Some("Run the same Rojo for serve and `rojo plugin install`, for example the one in the project's rokit.toml."),
        ));
    }
    if info.protocol_version != PROTOCOL_VERSION {
        checks.push(check(
            category,
            "protocol-mismatch",
            Status::Error,
            format!(
                "The server speaks protocol {}, but the plugin this CLI installs speaks {PROTOCOL_VERSION}, so they cannot connect.",
                info.protocol_version
            ),
            Some("Restart rojo serve with this CLI, or install the plugin from the Rojo that runs the server."),
        ));
    }
    // The API names the project but not its folder, and nothing else in it
    // identifies a checkout: the session ID is new on every start, and the
    // bridge's identity digest covers that session too. So a matching name
    // is only reported as a matching name.
    match (project_name, info.project_name.as_deref()) {
        (Some(name), Some(served)) if served == name => checks.push(check(
            category,
            "same-project-name",
            Status::Info,
            format!(
                "The server serves a project named {served}, like this one. The API does not say which folder it serves, so doctor cannot confirm it is this checkout."
            ),
            Some("With several checkouts or worktrees, check the folder in the terminal that runs rojo serve."),
        )),
        (Some(name), Some(served)) => checks.push(check(
            category,
            "other-project",
            Status::Warning,
            format!("The server on {address} serves {served}, not this project ({name})."),
            Some("Stop it, or serve this project on another port with `rojo serve --port`."),
        )),
        (Some(_), None) => checks.push(check(
            category,
            "project-unknown",
            Status::Info,
            "The server did not say which project it serves.",
            None,
        )),
        (None, _) => {}
    }
    // Listing sessions and their sync health needs the operator token, which
    // doctor does not take. The API has no separate "bridge enabled" field:
    // the bridge only answers with its pairing details, and it withholds
    // those from requests it does not trust (not from loopback, with an
    // Origin, or from another user's process), so their absence does not
    // prove the bridge is off. The details carry the plugin's token and are
    // never printed.
    checks.push(if info.studio_bridge.is_some() {
        check(
            category,
            "studio-bridge-enabled",
            Status::Info,
            "The Studio control bridge is enabled.",
            Some("Run `rojo studio sessions --token-file <file>` to see connected Studios and their sync health."),
        )
    } else {
        check(
            category,
            "studio-bridge-not-advertised",
            Status::Info,
            "The server did not advertise a Studio control bridge to doctor. It may be off, or hidden from this address or user.",
            Some("To use `rojo studio` and `rojo mcp`, serve with --studio-token-file on a loopback address and run doctor as the same user."),
        )
    });
    checks
}

/// Runs Git for the Git check, so tests can stand in for it.
trait Git {
    fn version(&self, directory: &Path) -> anyhow::Result<String>;
    fn common_dir(&self, directory: &Path) -> anyhow::Result<PathBuf>;
}

struct GitRunner;

impl Git for GitRunner {
    fn version(&self, directory: &Path) -> anyhow::Result<String> {
        lifecycle::git(directory, ["--version"])
    }

    fn common_dir(&self, directory: &Path) -> anyhow::Result<PathBuf> {
        lifecycle::git_common_dir(directory)
    }
}

/// Git is only needed by checkpoints, workspaces, releases and Studio locks
/// and backups, not to serve or build, so its problems are warnings.
fn check_git(directory: &Path, git: &dyn Git) -> Vec<Check> {
    let category = Category::Git;
    let output = match git.version(directory) {
        Ok(output) => output,
        Err(err) => {
            return vec![check(
                category,
                "git-missing",
                Status::Warning,
                format!("Git could not run: {err:#}"),
                Some("Install Git 2.31 or newer for checkpoints, workspaces and releases."),
            )]
        }
    };
    let mut checks = Vec::new();
    let version = parse_git_version(&output);
    match version {
        Some(version) if version >= MIN_GIT => checks.push(check(
            category,
            "git-version",
            Status::Ok,
            output.clone(),
            None,
        )),
        _ => checks.push(check(
            category,
            "git-too-old",
            Status::Warning,
            format!(
                "{output} is older than Git {}.{}, which checkpoints, workspaces and releases need.",
                MIN_GIT.0, MIN_GIT.1
            ),
            Some("Update Git."),
        )),
    }
    match git.common_dir(directory) {
        Ok(common_dir) => checks.push(check(
            category,
            "repository",
            Status::Ok,
            format!(
                "The project is in a Git repository ({}).",
                common_dir.display()
            ),
            None,
        )),
        Err(err) if super::has_git_marker(directory) => checks.push(check(
            category,
            "repository-unreadable",
            Status::Warning,
            format!(
                "The project looks like it is in a Git repository, but Git cannot read it: {err:#}"
            ),
            Some("Run `git status` in the project to see what Git reports."),
        )),
        Err(_) => checks.push(check(
            category,
            "not-a-repository",
            Status::Info,
            "The project is not in a Git repository.",
            Some("Run `git init` to use checkpoints, workspaces and releases."),
        )),
    }
    checks
}

/// Parses `git version 2.43.0.windows.1` into `(2, 43)`.
fn parse_git_version(output: &str) -> Option<(u32, u32)> {
    let version = output.trim().strip_prefix("git version ")?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    Some((major, minor))
}

/// Git cannot run in a folder that does not exist, such as a mistyped
/// project path, so check the nearest one that does.
fn existing_ancestor(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|dir| dir.is_dir())
        .unwrap_or(path)
        .to_path_buf()
}

fn render_text(report: &Report) -> String {
    let mut out = format!("Rojo doctor {}\n", report.rojo_version);
    for category in Category::ALL {
        let checks: Vec<_> = report
            .checks
            .iter()
            .filter(|check| check.category == category)
            .collect();
        if checks.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{}\n", category.title()));
        for check in checks {
            out.push_str(&format!(
                "  {:<8} {}\n",
                check.status.label(),
                check.message
            ));
            if let Some(hint) = &check.hint {
                out.push_str(&format!("           fix: {hint}\n"));
            }
        }
    }
    let counts = &report.counts;
    out.push_str(&format!(
        "\n{} ok, {} info, {} warning(s), {} error(s)\n",
        counts.ok, counts.info, counts.warning, counts.error
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::interface::ServerInfoResponse;
    use std::net::TcpListener;
    use tempfile::tempdir;

    fn codes(checks: &[Check]) -> Vec<(&str, Status)> {
        checks
            .iter()
            .map(|check| (check.code.as_str(), check.status))
            .collect()
    }

    fn write_project(dir: &Path, project: serde_json::Value) {
        fs::write(
            dir.join("default.project.json"),
            serde_json::to_vec(&project).unwrap(),
        )
        .unwrap();
    }

    fn load(dir: &Path) -> Project {
        Project::load_initial_project(&Vfs::new_default_without_watch(), dir).unwrap()
    }

    #[test]
    fn report_counts_statuses_and_fails_only_on_errors() {
        let report = Report::new(
            vec![
                check(Category::Project, "a", Status::Ok, "", None),
                check(Category::Git, "b", Status::Warning, "", None),
                check(Category::Server, "c", Status::Info, "", None),
            ],
            None,
        );
        assert!(report.ok);
        assert_eq!(
            (report.counts.ok, report.counts.info, report.counts.warning),
            (1, 1, 1)
        );

        let report = Report::new(
            vec![check(Category::Plugin, "d", Status::Error, "", None)],
            None,
        );
        assert!(!report.ok);
        assert_eq!(report.counts.error, 1);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["schemaVersion"], 1);
        assert_eq!(json["ok"], false);
        assert_eq!(json["checks"][0]["status"], "error");
        assert_eq!(json["checks"][0]["category"], "plugin");
    }

    #[test]
    fn text_groups_checks_and_shows_hints() {
        let report = Report::new(
            vec![
                check(Category::Git, "g", Status::Ok, "git version 2.45.0", None),
                check(
                    Category::Project,
                    "p",
                    Status::Error,
                    "broken",
                    Some("fix it"),
                ),
            ],
            None,
        );
        let text = render_text(&report);
        // Categories print in a fixed order, not the order checks ran.
        assert!(text.find("Project").unwrap() < text.find("Git").unwrap());
        assert!(text.contains("  error    broken\n           fix: fix it\n"));
        assert!(text.ends_with("1 ok, 0 info, 0 warning(s), 1 error(s)\n"));
    }

    #[test]
    fn project_check_reports_the_root_class_or_the_load_error() {
        let dir = tempdir().unwrap();
        write_project(
            dir.path(),
            serde_json::json!({"name": "game", "tree": {"$className": "DataModel"}}),
        );
        let mut checks = Vec::new();
        let session = check_project(dir.path(), &mut checks);
        assert!(session.is_some());
        assert_eq!(codes(&checks), [("project-loaded", Status::Ok)]);
        assert!(checks[0].message.contains("DataModel"), "{:?}", checks[0]);

        // A missing $path fails here the same way `rojo serve` fails, but the
        // parsed settings are still returned for the other checks.
        write_project(
            dir.path(),
            serde_json::json!({"name": "game", "servePort": 4000, "tree": {"$path": "missing"}}),
        );
        let mut checks = Vec::new();
        let project = check_project(dir.path(), &mut checks).unwrap();
        assert_eq!(codes(&checks), [("snapshot-failed", Status::Error)]);
        assert_eq!(project.serve_port, Some(4000));
        assert_eq!(project.name.as_deref(), Some("game"));

        let empty = tempdir().unwrap();
        let mut checks = Vec::new();
        assert!(check_project(empty.path(), &mut checks).is_none());
        assert_eq!(checks[0].status, Status::Error);
    }

    #[test]
    fn place_ids_are_classified_like_the_plugin_uses_them() {
        let dir = tempdir().unwrap();
        let tree = serde_json::json!({"$className": "DataModel"});

        write_project(dir.path(), serde_json::json!({"name": "a", "tree": tree}));
        assert_eq!(
            codes(&check_place_ids(&load(dir.path()))),
            [("absent", Status::Warning)]
        );

        write_project(
            dir.path(),
            serde_json::json!({"name": "a", "tree": tree, "servePlaceIds": []}),
        );
        assert_eq!(
            codes(&check_place_ids(&load(dir.path()))),
            [("empty", Status::Error)]
        );

        write_project(
            dir.path(),
            serde_json::json!({"name": "a", "tree": tree, "servePlaceIds": [20, 0, 10]}),
        );
        let checks = check_place_ids(&load(dir.path()));
        assert_eq!(codes(&checks), [("present", Status::Ok)]);
        assert!(checks[0].message.contains("0, 10, 20"), "{:?}", checks[0]);
        assert!(checks[0].message.contains("Unpublished"));

        write_project(
            dir.path(),
            serde_json::json!({"name": "a", "tree": tree, "servePlaceIds": [1, 2], "blockedPlaceIds": [2]}),
        );
        assert_eq!(
            codes(&check_place_ids(&load(dir.path()))),
            [
                ("present", Status::Ok),
                ("blocked-and-allowed", Status::Warning)
            ]
        );

        // IDs that are not numbers stop the project from loading, as they
        // stop `rojo serve`, so the project check reports them.
        write_project(
            dir.path(),
            serde_json::json!({"name": "a", "tree": tree, "servePlaceIds": ["one"]}),
        );
        let mut checks = Vec::new();
        assert!(check_project(dir.path(), &mut checks).is_none());
        assert_eq!(codes(&checks), [("project-invalid", Status::Error)]);
    }

    #[test]
    fn package_findings_are_folded_in() {
        let dir = tempdir().unwrap();
        write_project(
            dir.path(),
            serde_json::json!({"name": "a", "tree": {"$className": "DataModel"}}),
        );
        // No manager is not a problem for doctor.
        assert_eq!(
            codes(&check_packages(dir.path())),
            [("manifest-missing", Status::Info)]
        );

        fs::write(dir.path().join("wally.toml"), "[dependencies]\n").unwrap();
        fs::write(dir.path().join("wally.lock"), "").unwrap();
        assert_eq!(
            codes(&check_packages(dir.path())),
            [("packages-ok", Status::Ok)]
        );

        fs::create_dir(dir.path().join("ServerPackages")).unwrap();
        write_project(
            dir.path(),
            serde_json::json!({"name": "a", "tree": {"$className": "DataModel",
                "ReplicatedStorage": {"Leaked": {"$path": "ServerPackages"}}}}),
        );
        let checks = check_packages(dir.path());
        assert!(codes(&checks).contains(&("server-packages-exposed", Status::Error)));
        assert!(!codes(&checks)
            .iter()
            .any(|(code, _)| *code == "packages-ok"));

        assert_eq!(
            codes(&check_packages(&dir.path().join("missing"))),
            [("not-checked", Status::Info)]
        );
    }

    #[test]
    fn plugin_check_reads_the_installed_version_and_finds_copies() {
        let dir = tempdir().unwrap();
        assert_eq!(
            codes(&check_plugin(Ok(dir.path().to_owned()), false)),
            [("plugin-missing", Status::Warning)]
        );

        plugin::install_plugin(dir.path(), false).unwrap();
        let bytes = fs::read(dir.path().join(plugin::PLUGIN_FILE_NAME)).unwrap();
        // The bundled plugin's Version.txt is released together with the CLI,
        // and the plugin passes the untrusted-model checks.
        assert_eq!(plugin_version(bytes.clone()).as_deref(), Ok(CLI_VERSION));
        assert_eq!(
            codes(&check_plugin(Ok(dir.path().to_owned()), false)),
            [("plugin-current", Status::Ok)]
        );

        // An older plugin, as Studio would load one left by an earlier install.
        let mut dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();
        let rojo = dom.root().children()[0];
        let version = *dom
            .get_by_ref(rojo)
            .unwrap()
            .children()
            .iter()
            .find(|id| dom.get_by_ref(**id).unwrap().name == "Version")
            .unwrap();
        dom.get_by_ref_mut(version)
            .unwrap()
            .properties
            .insert(Ustr::from("Value"), Variant::String("7.7.1".to_owned()));
        let mut old = Vec::new();
        rbx_binary::to_writer(&mut old, &dom, &[rojo]).unwrap();
        fs::write(dir.path().join(plugin::PLUGIN_FILE_NAME), &old).unwrap();
        let checks = check_plugin(Ok(dir.path().to_owned()), false);
        assert_eq!(
            codes(&checks),
            [("plugin-version-mismatch", Status::Warning)]
        );
        assert!(checks[0].message.contains("7.7.1"));

        fs::write(dir.path().join(plugin::PLUGIN_FILE_NAME), b"not a model").unwrap();
        fs::write(dir.path().join("Rojo-7.7.1.rbxm"), b"copy").unwrap();
        fs::write(dir.path().join("Rojo-Helper.rbxm"), b"unrelated").unwrap();
        let checks = check_plugin(Ok(dir.path().to_owned()), false);
        assert_eq!(
            codes(&checks),
            [
                ("plugin-unreadable", Status::Warning),
                ("side-by-side-copies", Status::Warning)
            ]
        );
        assert!(checks[1].message.contains("Rojo-7.7.1.rbxm"));
        assert!(!checks[1].message.contains("Helper"));

        assert_eq!(
            codes(&check_plugin(Err("no Studio".into()), false)),
            [("studio-not-found", Status::Warning)]
        );
    }

    #[test]
    fn corrupted_or_oversized_plugins_are_unreadable_not_fatal() {
        let dir = tempdir().unwrap();
        plugin::install_plugin(dir.path(), false).unwrap();
        let path = dir.path().join(plugin::PLUGIN_FILE_NAME);
        let mut bytes = fs::read(&path).unwrap();
        // The header's instance count, which rbx_binary reserves room for
        // before reading anything else. Unchecked, this asks for gigabytes.
        bytes[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        let checks = check_plugin(Ok(dir.path().to_owned()), false);
        assert_eq!(codes(&checks), [("plugin-unreadable", Status::Warning)]);
        assert!(
            checks[0].message.contains("too many instances"),
            "{:?}",
            checks[0]
        );

        // A truncated model declares more than it holds.
        let mut truncated = fs::read(dir.path().join(plugin::PLUGIN_FILE_NAME)).unwrap();
        truncated.truncate(truncated.len() / 2);
        fs::write(&path, &truncated).unwrap();
        assert_eq!(
            codes(&check_plugin(Ok(dir.path().to_owned()), false)),
            [("plugin-unreadable", Status::Warning)]
        );

        // Files over the untrusted-model size limit are not read whole.
        fs::write(&path, vec![0u8; MAX_MODEL_BYTES + 1]).unwrap();
        let checks = check_plugin(Ok(dir.path().to_owned()), false);
        assert_eq!(codes(&checks), [("plugin-unreadable", Status::Warning)]);
        assert!(checks[0].message.contains("larger than"), "{:?}", checks[0]);
    }

    #[test]
    fn repair_hints_name_an_explicit_plugins_dir() {
        let dir = tempdir().unwrap();
        let flag = format!("--plugins-dir \"{}\"", dir.path().display());
        let hints = |checks: Vec<Check>| -> Vec<String> {
            checks.into_iter().filter_map(|check| check.hint).collect()
        };

        // Missing, then unreadable, each with side-by-side copies.
        fs::write(dir.path().join("Rojo-7.7.1.rbxm"), b"copy").unwrap();
        let missing = hints(check_plugin(Ok(dir.path().to_owned()), true));
        fs::write(dir.path().join(plugin::PLUGIN_FILE_NAME), b"broken").unwrap();
        let unreadable = hints(check_plugin(Ok(dir.path().to_owned()), true));
        for hint in missing.iter().chain(&unreadable) {
            assert!(hint.contains(&flag), "{hint}");
        }
        assert_eq!(missing.len(), 2);
        assert_eq!(unreadable.len(), 2);
        // Both uninstall commands in the side-by-side hint name it.
        assert_eq!(unreadable[1].matches(&flag).count(), 2, "{}", unreadable[1]);

        // An outdated plugin's hint too.
        fs::remove_file(dir.path().join("Rojo-7.7.1.rbxm")).unwrap();
        let mut dom = rbx_dom_weak::WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Folder"));
        let rojo = dom.insert(
            dom.root_ref(),
            rbx_dom_weak::InstanceBuilder::new("Folder").with_name("Rojo"),
        );
        dom.insert(
            rojo,
            rbx_dom_weak::InstanceBuilder::new("StringValue")
                .with_name("Version")
                .with_property("Value", Variant::String("7.7.1".to_owned())),
        );
        let mut old = Vec::new();
        rbx_binary::to_writer(&mut old, &dom, &[rojo]).unwrap();
        fs::write(dir.path().join(plugin::PLUGIN_FILE_NAME), &old).unwrap();
        let checks = check_plugin(Ok(dir.path().to_owned()), true);
        assert_eq!(
            codes(&checks),
            [("plugin-version-mismatch", Status::Warning)]
        );
        assert!(hints(checks)[0].contains(&flag));

        // Discovered folders keep the plain commands.
        for hint in hints(check_plugin(Ok(dir.path().to_owned()), false)) {
            assert!(!hint.contains("--plugins-dir"), "{hint}");
        }
    }

    #[test]
    fn server_address_follows_serve_precedence() {
        let dir = tempdir().unwrap();
        write_project(
            dir.path(),
            serde_json::json!({"name": "a", "tree": {"$className": "DataModel"},
                "servePort": 4000, "serveAddress": "0.0.0.0"}),
        );
        let project = load(dir.path());
        assert_eq!(
            server_address(None, None, None),
            "127.0.0.1:34872".parse().unwrap()
        );
        // A server bound to every interface is reached over loopback.
        assert_eq!(
            server_address(None, None, Some(&project)),
            "127.0.0.1:4000".parse().unwrap()
        );
        assert_eq!(
            server_address(Some("::1".parse().unwrap()), Some(5000), Some(&project)),
            "[::1]:5000".parse().unwrap()
        );
    }

    /// This fork's real `/api/rojo` body, encoded as the server encodes it.
    fn msgpack_info(server_version: &str, protocol_version: u64, bridge: bool) -> Vec<u8> {
        crate::web::util::serialize_msgpack(ServerInfoResponse {
            studio_bridge: bridge.then(|| serde_json::json!({"pluginToken": "secret"})),
            session_id: crate::SessionId::new(),
            server_version: server_version.to_owned(),
            protocol_version,
            project_name: "game".to_owned(),
            expected_place_ids: None,
            unexpected_place_ids: None,
            game_id: None,
            place_id: None,
            root_instance_id: rbx_dom_weak::types::Ref::new(),
        })
        .unwrap()
    }

    fn info(server_version: &str, protocol_version: u64, bridge: bool) -> Probe {
        let bytes = msgpack_info(server_version, protocol_version, bridge);
        Probe::Rojo(parse_server_info(Some("application/msgpack"), &bytes).unwrap())
    }

    /// The shape upstream Rojo 7.x answers `/api/rojo` with: JSON, protocol 4.
    const UPSTREAM_V7_INFO: &str = r#"{"sessionId":"5f0e2c4e-8a35-4f0b-9f4e-0f2b8b1d9a77","serverVersion":"7.6.1","protocolVersion":4,"projectName":"game","expectedPlaceIds":null,"unexpectedPlaceIds":null,"gameId":null,"placeId":null,"rootInstanceId":"0123456789abcdef0123456789abcdef"}"#;

    #[test]
    fn server_info_parses_from_msgpack_and_upstream_json() {
        let info = parse_server_info(
            Some("application/msgpack"),
            &msgpack_info(CLI_VERSION, PROTOCOL_VERSION, true),
        )
        .unwrap();
        assert_eq!(info.protocol_version, PROTOCOL_VERSION);
        assert!(info.studio_bridge.is_some());

        for content_type in [Some("application/json"), None] {
            let info = parse_server_info(content_type, UPSTREAM_V7_INFO.as_bytes()).unwrap();
            assert_eq!(
                (info.server_version.as_str(), info.protocol_version),
                ("7.6.1", 4)
            );
            assert!(info.studio_bridge.is_none());
        }
        // A mislabeled body is still tried in the other format.
        assert!(parse_server_info(
            Some("application/json"),
            &msgpack_info(CLI_VERSION, PROTOCOL_VERSION, false)
        )
        .is_some());
        assert!(parse_server_info(Some("application/json"), b"{\"hello\":1}").is_none());
        assert!(parse_server_info(None, b"hello").is_none());
    }

    #[test]
    fn server_results_are_classified() {
        let address: SocketAddr = "127.0.0.1:34872".parse().unwrap();
        assert_eq!(
            codes(&check_server(address, Probe::NotRunning, None)),
            [("not-running", Status::Info)]
        );
        assert_eq!(
            codes(&check_server(address, Probe::NotRojo("404".into()), None)),
            [("not-rojo", Status::Warning)]
        );
        assert_eq!(
            codes(&check_server(
                address,
                info(CLI_VERSION, PROTOCOL_VERSION, false),
                Some("game")
            )),
            [
                ("server-current", Status::Ok),
                ("same-project-name", Status::Info),
                ("studio-bridge-not-advertised", Status::Info)
            ]
        );
        // A matching name is not claimed to be the same checkout, and missing
        // pairing details are not claimed to mean the bridge is off.
        let checks = check_server(
            address,
            info(CLI_VERSION, PROTOCOL_VERSION, false),
            Some("game"),
        );
        let messages: String = checks.iter().map(|check| check.message.as_str()).collect();
        assert!(messages.contains("cannot confirm"), "{messages}");
        assert!(messages.contains("may be off"), "{messages}");
        assert!(!messages.contains("not enabled"), "{messages}");
        // Without a project to compare, no name check is made.
        assert_eq!(
            codes(&check_server(
                address,
                info(CLI_VERSION, PROTOCOL_VERSION, true),
                None
            )),
            [
                ("server-current", Status::Ok),
                ("studio-bridge-enabled", Status::Info)
            ]
        );
        let checks = check_server(address, info("7.0.0", 4, true), Some("other"));
        assert_eq!(
            codes(&checks),
            [
                ("server-version-mismatch", Status::Warning),
                ("protocol-mismatch", Status::Error),
                ("other-project", Status::Warning),
                ("studio-bridge-enabled", Status::Info)
            ]
        );
        // The pairing details include the plugin's token, which must not leak
        // into the report.
        let json = serde_json::to_string(&checks).unwrap();
        assert!(!json.contains("secret"));
    }

    #[test]
    fn probing_a_closed_port_reports_not_running_quickly() {
        // Bind and drop to find a port nothing listens on.
        let address = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let start = std::time::Instant::now();
        assert!(matches!(fetch_server_info(address), Probe::NotRunning));
        assert!(start.elapsed() < REQUEST_TIMEOUT + Duration::from_secs(1));
    }

    #[test]
    fn probing_a_server_that_never_answers_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let start = std::time::Instant::now();
        // The listener accepts into its backlog but never replies.
        let probe = fetch_server_info(address);
        assert!(matches!(probe, Probe::NoAnswer));
        assert_eq!(
            codes(&check_server(address, probe, None)),
            [("no-answer", Status::Warning)]
        );
        assert!(start.elapsed() < REQUEST_TIMEOUT + Duration::from_secs(2));
        drop(listener);
    }

    /// Answers one request with `body`, then probes it.
    fn probe_one_response(content_type: &str, body: &[u8]) -> (SocketAddr, Probe) {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 1024];
            let _ = stream.read(&mut buffer);
            stream.write_all(&response).unwrap();
        });
        let probe = fetch_server_info(address);
        server.join().unwrap();
        (address, probe)
    }

    /// Serves one request on a fresh port with `body`, without probing it.
    fn serve_one_response(body: &'static [u8]) -> (SocketAddr, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 1024];
            let _ = stream.read(&mut buffer);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
        });
        (address, server)
    }

    #[test]
    fn oversized_server_answers_are_not_read_whole() {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 1024];
            let _ = stream.read(&mut buffer);
            // Chunked, so the client cannot reject it by its length alone.
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
            );
            let chunk = vec![b' '; 64 * 1024];
            // 64 MiB in all; writes fail once doctor stops reading and closes.
            for _ in 0..1024 {
                let head = format!("{:x}\r\n", chunk.len());
                if stream.write_all(head.as_bytes()).is_err()
                    || stream.write_all(&chunk).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    return;
                }
            }
        });
        let probe = fetch_server_info(address);
        match &probe {
            Probe::NotRojo(reason) => assert!(reason.contains("more than 1 MiB"), "{reason}"),
            _ => panic!("expected not-rojo"),
        }
        assert_eq!(
            codes(&check_server(address, probe, None)),
            [("not-rojo", Status::Warning)]
        );
        server.join().unwrap();
    }

    #[test]
    fn server_check_uses_the_project_settings_when_its_snapshot_fails() {
        let (address, server) = serve_one_response(UPSTREAM_V7_INFO.as_bytes());
        let dir = tempdir().unwrap();
        write_project(
            dir.path(),
            serde_json::json!({"name": "game", "servePort": address.port(),
                "tree": {"$path": "missing"}}),
        );
        let plugins = tempdir().unwrap();
        let report = build_report(&Inputs {
            project_path: dir.path().to_owned(),
            address: None,
            port: None,
            plugins_dir: Some(plugins.path().to_owned()),
        });
        server.join().unwrap();
        let project: Vec<_> = report
            .checks
            .iter()
            .filter(|check| check.category == Category::Project)
            .collect();
        assert_eq!(
            codes(&project.into_iter().cloned().collect::<Vec<_>>()),
            [("snapshot-failed", Status::Error)]
        );
        let server: Vec<_> = report
            .checks
            .iter()
            .filter(|check| check.category == Category::Server)
            .cloned()
            .collect();
        // It probed the project's servePort and compared the project's name.
        let server_codes = codes(&server);
        assert!(
            server_codes.contains(&("protocol-mismatch", Status::Error)),
            "{server_codes:?}"
        );
        assert!(
            server_codes.contains(&("same-project-name", Status::Info)),
            "{server_codes:?}"
        );
        assert!(server[0].message.contains(&address.to_string()));
        // servePlaceIds still came from the parsed project.
        assert!(report
            .checks
            .iter()
            .any(|check| check.category == Category::ServePlaceIds && check.code == "absent"));
        assert_eq!(
            report.project_file,
            Some(dir.path().join("default.project.json"))
        );
    }

    #[test]
    fn probing_a_server_that_is_not_rojo_says_so() {
        let (_, probe) = probe_one_response("text/plain", b"hello");
        assert!(matches!(probe, Probe::NotRojo(_)));
    }

    #[test]
    fn an_upstream_json_server_is_a_protocol_mismatch_not_a_stranger() {
        let (address, probe) = probe_one_response("application/json", UPSTREAM_V7_INFO.as_bytes());
        let checks = check_server(address, probe, Some("game"));
        assert_eq!(
            codes(&checks),
            [
                ("server-version-mismatch", Status::Warning),
                ("protocol-mismatch", Status::Error),
                ("same-project-name", Status::Info),
                ("studio-bridge-not-advertised", Status::Info)
            ]
        );
        assert!(!Report::new(checks, None).ok);

        // The same through this fork's MessagePack.
        let (address, probe) =
            probe_one_response("application/msgpack", &msgpack_info("7.6.1", 4, false));
        assert!(codes(&check_server(address, probe, None))
            .contains(&("protocol-mismatch", Status::Error)));
    }

    struct FakeGit {
        version: Option<&'static str>,
        repository: bool,
    }

    impl Git for FakeGit {
        fn version(&self, _: &Path) -> anyhow::Result<String> {
            self.version
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("Git is required for this command"))
        }

        fn common_dir(&self, directory: &Path) -> anyhow::Result<PathBuf> {
            if self.repository {
                Ok(directory.join(".git"))
            } else {
                anyhow::bail!("not a git repository")
            }
        }
    }

    #[test]
    fn git_checks_version_and_repository() {
        let dir = tempdir().unwrap();
        let run = |version, repository| {
            check_git(
                dir.path(),
                &FakeGit {
                    version,
                    repository,
                },
            )
        };
        assert_eq!(codes(&run(None, true)), [("git-missing", Status::Warning)]);
        assert_eq!(
            codes(&run(Some("git version 2.43.0.windows.1"), true)),
            [("git-version", Status::Ok), ("repository", Status::Ok)]
        );
        assert_eq!(
            codes(&run(Some("git version 2.30.9"), false)),
            [
                ("git-too-old", Status::Warning),
                ("not-a-repository", Status::Info)
            ]
        );

        // A .git folder Git cannot read is worth a warning, unlike no repository.
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git").join("HEAD"), "ref: refs/heads/main").unwrap();
        assert_eq!(
            codes(&run(Some("git version 2.31.0"), false)),
            [
                ("git-version", Status::Ok),
                ("repository-unreadable", Status::Warning)
            ]
        );
    }

    #[test]
    fn git_versions_parse() {
        assert_eq!(parse_git_version("git version 2.31.0"), Some((2, 31)));
        assert_eq!(
            parse_git_version("git version 2.45.2.windows.1\n"),
            Some((2, 45))
        );
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39))
        );
        assert_eq!(parse_git_version("not git"), None);
    }

    #[test]
    fn doctor_does_not_change_the_project() {
        let dir = tempdir().unwrap();
        write_project(
            dir.path(),
            serde_json::json!({"name": "a", "tree": {"$className": "DataModel",
                "ReplicatedStorage": {"$path": "src"}}}),
        );
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("a.luau"), "return 1").unwrap();
        let plugins = tempdir().unwrap();
        let listing = |path: &Path| -> Vec<(PathBuf, Vec<u8>)> {
            let mut entries: Vec<_> = walkdir::WalkDir::new(path)
                .into_iter()
                .map(|entry| entry.unwrap())
                .map(|entry| {
                    let bytes = if entry.file_type().is_file() {
                        fs::read(entry.path()).unwrap()
                    } else {
                        Vec::new()
                    };
                    (entry.path().to_owned(), bytes)
                })
                .collect();
            entries.sort();
            entries
        };
        let before = (listing(dir.path()), listing(plugins.path()));
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let report = build_report(&Inputs {
            project_path: dir.path().to_owned(),
            address: Some(closed.ip()),
            port: Some(closed.port()),
            plugins_dir: Some(plugins.path().to_owned()),
        });
        assert_eq!((listing(dir.path()), listing(plugins.path())), before);
        assert!(report.project_file.is_some());
        let categories: Vec<_> = report.checks.iter().map(|check| check.category).collect();
        for category in Category::ALL {
            assert!(categories.contains(&category), "{category:?} missing");
        }
        // Without servePlaceIds this is a warning, not an error.
        assert!(report.ok, "{report:#?}");
    }
}
