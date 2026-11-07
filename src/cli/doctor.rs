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
///
/// The connect timeout is longer than a healthy server needs because only a
/// refused connection shows that nothing listens, and Windows retries a
/// refused loopback connection for about two seconds before reporting it.
/// A shorter timeout would turn every "no server" into "inconclusive" there.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(3000);
/// Covers connecting and the answer: 1.5 seconds once connected.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(4500);
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
        .filter(|name| plugins_dir.join(name).is_file())
        .collect();
    copies.sort();
    if !copies.is_empty() {
        let paths: Vec<String> = copies
            .iter()
            .map(|name| format!("\"{}\"", plugins_dir.join(name).display()))
            .collect();
        checks.push(check(
            category,
            "side-by-side-copies",
            Status::Warning,
            format!(
                "Side-by-side copies in {}: {}. Studio loads each one as a separate Rojo plugin.",
                plugins_dir.display(),
                copies.join(", ")
            ),
            // `uninstall --side-by-side` only removes this CLI's version, and
            // `uninstall --all` also removes the managed plugin, so name the
            // files, and give --all only together with the reinstall.
            Some(&format!(
                "Delete the copies you no longer test: {}. Or remove every copy and reinstall the managed plugin with `rojo plugin uninstall --all{flag}` followed by {install}.",
                paths.join(", ")
            )),
        ));
    }
    checks
}

/// Reads at most the size limit for untrusted models, so a huge file in the
/// plugins folder is reported instead of read into memory. `None` when the
/// file does not exist.
///
/// Only regular files are opened. Opening or reading a named pipe or FIFO
/// can block until something writes to it, which may be never, and a
/// directory cannot be a plugin. The opened handle is checked again in case
/// the path changed in between.
fn read_plugin(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let not_a_file = || "it is not a regular file".to_owned();
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(not_a_file()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    }
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(not_a_file());
    }
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
    /// The connection was refused, so nothing listens there.
    NotRunning,
    /// The connection failed in a way that does not show whether anything
    /// listens, such as a connect timeout against a full backlog.
    Inconclusive(String),
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
    /// Absent or null when the bridge is off or hidden from this request.
    #[serde(default)]
    studio_bridge: Option<BridgeField>,
}

/// `studioBridge` as the server sends it, or anything else. The fields exist
/// only so decoding checks their shape, hence `dead_code`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
#[allow(dead_code)]
enum BridgeField {
    Advertised(BridgeAdvert),
    Unrecognized(IgnoredAny),
}

/// The pairing details `StudioBridge::pairing` (src/web/studio.rs) sends:
/// `{"protocolVersion": 1, "pluginToken": "...", "identity": {...}}`. Only
/// their shape is checked. The token is never kept, so it cannot be printed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct BridgeAdvert {
    protocol_version: u64,
    plugin_token: Shape<StringShape>,
    identity: Shape<MapShape>,
}

/// A value of the kind `K` names, checked and then discarded.
#[derive(Debug)]
struct Shape<K>(std::marker::PhantomData<K>);

#[derive(Debug)]
struct StringShape;
#[derive(Debug)]
struct MapShape;

impl<'de> Deserialize<'de> for Shape<StringShape> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = ();
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string")
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
                Ok(())
            }
        }
        deserializer.deserialize_str(Visitor)?;
        Ok(Shape(std::marker::PhantomData))
    }
}

impl<'de> Deserialize<'de> for Shape<MapShape> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ();
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(())
            }
        }
        deserializer.deserialize_map(Visitor)?;
        Ok(Shape(std::marker::PhantomData))
    }
}

/// The deepest nesting accepted in a `/api/rojo` answer. Real answers nest
/// three or four levels. The decoders recurse once per level, even through
/// values doctor ignores, so a small body nested thousands of levels deep
/// would overflow the main thread's stack, which no error handling catches.
const MAX_INFO_DEPTH: usize = 32;

/// Decodes `/api/rojo` as this fork's MessagePack, or as the JSON that
/// released upstream Rojo sends. The content type picks which to try first.
/// Each format is only decoded after an iterative scan shows it is nested at
/// most [`MAX_INFO_DEPTH`] levels deep.
fn parse_server_info(content_type: Option<&str>, bytes: &[u8]) -> Result<ServerInfo, String> {
    let json = || -> Result<ServerInfo, String> {
        json_depth(bytes, MAX_INFO_DEPTH)?;
        serde_json::from_slice(bytes).map_err(|err| err.to_string())
    };
    let msgpack = || -> Result<ServerInfo, String> {
        msgpack_depth(bytes, MAX_INFO_DEPTH)?;
        deserialize_msgpack(bytes).map_err(|err| err.to_string())
    };
    let (first, second): (&dyn Fn() -> _, &dyn Fn() -> _) =
        if content_type.is_some_and(|value| value.contains("json")) {
            (&json, &msgpack)
        } else {
            (&msgpack, &json)
        };
    // The first format's reason is the one worth reporting.
    first().or_else(|reason| second().map_err(|_| reason))
}

/// Checks that a JSON document nests at most `limit` arrays and objects,
/// without recursing. Strings are skipped, so brackets in them do not count.
fn json_depth(bytes: &[u8], limit: usize) -> Result<(), String> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > limit {
                    return Err(format!("it is nested more than {limit} levels deep"));
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// Checks that the first MessagePack value in `bytes` nests at most `limit`
/// arrays and maps, without recursing: an explicit stack holds how many
/// values each open container still has. Every value takes at least one
/// byte, so the walk ends within `bytes.len()` steps.
fn msgpack_depth(bytes: &[u8], limit: usize) -> Result<(), String> {
    let truncated = || "it is not complete MessagePack".to_owned();
    let mut position = 0usize;
    // Values left in each open container; the bottom entry is the top-level
    // value itself.
    let mut remaining: Vec<u64> = vec![1];
    let read_uint = |position: &mut usize, size: usize| -> Result<u64, String> {
        let end = position.checked_add(size).ok_or_else(truncated)?;
        let slice = bytes.get(*position..end).ok_or_else(truncated)?;
        *position = end;
        Ok(slice
            .iter()
            .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)))
    };
    while let Some(left) = remaining.last_mut() {
        if *left == 0 {
            remaining.pop();
            continue;
        }
        *left -= 1;
        let marker = *bytes.get(position).ok_or_else(truncated)?;
        position += 1;
        // A container's item count, or the number of bytes to skip.
        let (items, skip): (u64, u64) = match marker {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => (0, 0),
            0x80..=0x8f => (u64::from(marker & 0x0f) * 2, 0),
            0x90..=0x9f => (u64::from(marker & 0x0f), 0),
            0xa0..=0xbf => (0, u64::from(marker & 0x1f)),
            0xc4 | 0xd9 => (0, read_uint(&mut position, 1)?),
            0xc5 | 0xda => (0, read_uint(&mut position, 2)?),
            0xc6 | 0xdb => (0, read_uint(&mut position, 4)?),
            0xc7 => (0, read_uint(&mut position, 1)? + 1),
            0xc8 => (0, read_uint(&mut position, 2)? + 1),
            0xc9 => (0, read_uint(&mut position, 4)? + 1),
            0xca => (0, 4),
            0xcb => (0, 8),
            0xcc | 0xd0 => (0, 1),
            0xcd | 0xd1 => (0, 2),
            0xce | 0xd2 => (0, 4),
            0xcf | 0xd3 => (0, 8),
            0xd4 => (0, 2),
            0xd5 => (0, 3),
            0xd6 => (0, 5),
            0xd7 => (0, 9),
            0xd8 => (0, 17),
            0xdc => (read_uint(&mut position, 2)?, 0),
            0xdd => (read_uint(&mut position, 4)?, 0),
            0xde => (read_uint(&mut position, 2)? * 2, 0),
            0xdf => (read_uint(&mut position, 4)? * 2, 0),
            0xc1 => return Err("it is not MessagePack".to_owned()),
        };
        position = usize::try_from(skip)
            .ok()
            .and_then(|skip| position.checked_add(skip))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(truncated)?;
        if items > 0 {
            // The bottom entry is not a container.
            if remaining.len() > limit {
                return Err(format!("it is nested more than {limit} levels deep"));
            }
            remaining.push(items);
        }
    }
    Ok(())
}

/// Sorts a failed request into what it shows about the port. Only a refused
/// connection shows that nothing listens; a connect timeout (which reqwest
/// also counts as a connect error) can be a full backlog or a firewall.
fn classify_send_error(timeout: bool, connect: bool, refused: bool, reason: String) -> Probe {
    match (timeout, connect, refused) {
        (true, true, _) => Probe::Inconclusive(format!("connecting timed out ({reason})")),
        (true, false, _) => Probe::NoAnswer,
        (false, true, true) => Probe::NotRunning,
        (false, true, false) => Probe::Inconclusive(reason),
        (false, false, _) => Probe::NotRojo(reason),
    }
}

/// Whether an io `ConnectionRefused` error is anywhere in `err`'s sources.
fn connection_refused(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(err);
    while let Some(err) = source {
        if err
            .downcast_ref::<std::io::Error>()
            .is_some_and(|err| err.kind() == std::io::ErrorKind::ConnectionRefused)
        {
            return true;
        }
        source = err.source();
    }
    false
}

fn fetch_server_info(address: SocketAddr) -> Probe {
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        // A redirect means something other than rojo serve answers on this
        // port; following it would report whatever answers elsewhere.
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
    {
        Ok(client) => client,
        Err(err) => return Probe::NotRojo(format!("could not create an HTTP client: {err}")),
    };
    let response = match client.get(format!("http://{address}/api/rojo")).send() {
        Ok(response) => response,
        Err(err) => {
            let refused = connection_refused(&err);
            return classify_send_error(
                err.is_timeout(),
                err.is_connect(),
                refused,
                err.to_string(),
            );
        }
    };
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if status.is_redirection() {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("nowhere");
        return Probe::NotRojo(format!("answered {status}, redirecting to {location}"));
    }
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
        Ok(info) => Probe::Rojo(info),
        Err(reason) => Probe::NotRojo(format!(
            "answered {status} without Rojo's server info: {reason}"
        )),
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
        Probe::Inconclusive(reason) => {
            return vec![check(
                category,
                "probe-inconclusive",
                Status::Warning,
                format!(
                    "Could not tell whether a rojo serve is on {address}: {reason}."
                ),
                Some("Check that nothing else holds the port and that a firewall allows local connections, then run doctor again."),
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
    checks.push(match info.studio_bridge {
        Some(BridgeField::Advertised(_)) => check(
            category,
            "studio-bridge-enabled",
            Status::Info,
            "The Studio control bridge is enabled.",
            Some("Run `rojo studio sessions --token-file <file>` to see connected Studios and their sync health."),
        ),
        Some(BridgeField::Unrecognized(_)) => check(
            category,
            "studio-bridge-unrecognized",
            Status::Info,
            "The server sent Studio bridge details doctor does not recognize, so it cannot tell whether the bridge is enabled.",
            Some("Run doctor from the same Rojo version as rojo serve."),
        ),
        None => check(
            category,
            "studio-bridge-not-advertised",
            Status::Info,
            "The server did not advertise a Studio control bridge to doctor. It may be off, or hidden from this address or user.",
            Some("To use `rojo studio` and `rojo mcp`, serve with --studio-token-file on a loopback address and run doctor as the same user."),
        ),
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

        // Something that is not a regular file is not opened. A directory
        // stands in for a named pipe or FIFO, whose read could block forever.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fs::create_dir(dir.path().join("Rojo-7.7.1.rbxm")).unwrap();
        let checks = check_plugin(Ok(dir.path().to_owned()), false);
        // The directory named like a side-by-side copy is not listed either.
        assert_eq!(codes(&checks), [("plugin-unreadable", Status::Warning)]);
        assert!(
            checks[0].message.contains("not a regular file"),
            "{:?}",
            checks[0]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_named_like_the_plugin_does_not_block() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(plugin::PLUGIN_FILE_NAME);
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let checks = check_plugin(Ok(dir.path().to_owned()), false);
        assert_eq!(codes(&checks), [("plugin-unreadable", Status::Warning)]);
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
        // The side-by-side hint names the stray file, and offers --all only
        // together with the reinstall, both naming the folder.
        let copy = format!("\"{}\"", dir.path().join("Rojo-7.7.1.rbxm").display());
        assert!(unreadable[1].contains(&copy), "{}", unreadable[1]);
        assert!(
            !unreadable[1].contains("--side-by-side"),
            "{}",
            unreadable[1]
        );
        assert!(
            unreadable[1].contains(&format!(
                "`rojo plugin uninstall --all {flag}` followed by `rojo plugin install {flag}`"
            )),
            "{}",
            unreadable[1]
        );

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
        // The shape `StudioBridge::pairing` sends.
        let pairing = serde_json::json!({
            "protocolVersion": 1,
            "pluginToken": "secret",
            "identity": {"serverSession": "session", "digest": "abc"},
        });
        msgpack_info_with_bridge(server_version, protocol_version, bridge.then_some(pairing))
    }

    fn msgpack_info_with_bridge(
        server_version: &str,
        protocol_version: u64,
        bridge: Option<serde_json::Value>,
    ) -> Vec<u8> {
        crate::web::util::serialize_msgpack(ServerInfoResponse {
            studio_bridge: bridge,
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
        assert!(matches!(
            info.studio_bridge,
            Some(BridgeField::Advertised(_))
        ));

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
        .is_ok());
        assert!(parse_server_info(Some("application/json"), b"{\"hello\":1}").is_err());
        assert!(parse_server_info(None, b"hello").is_err());
    }

    #[test]
    fn only_the_real_pairing_shape_counts_as_an_enabled_bridge() {
        let address: SocketAddr = "127.0.0.1:34872".parse().unwrap();
        let bridge_code = |bridge: Option<serde_json::Value>| {
            let bytes = msgpack_info_with_bridge(CLI_VERSION, PROTOCOL_VERSION, bridge);
            let info = parse_server_info(Some("application/msgpack"), &bytes).unwrap();
            let checks = check_server(address, Probe::Rojo(info), None);
            checks.last().unwrap().code.clone()
        };
        let pairing = serde_json::json!({
            "protocolVersion": 1, "pluginToken": "t", "identity": {"digest": "d"}
        });
        assert_eq!(bridge_code(Some(pairing)), "studio-bridge-enabled");
        assert_eq!(bridge_code(None), "studio-bridge-not-advertised");
        for odd in [
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::json!("on"),
            serde_json::json!({}),
            serde_json::json!({"protocolVersion": 1, "identity": {}}),
            serde_json::json!({"protocolVersion": 1, "pluginToken": false, "identity": {}}),
            serde_json::json!({"protocolVersion": 1, "pluginToken": "t", "identity": "x"}),
        ] {
            assert_eq!(
                bridge_code(Some(odd.clone())),
                "studio-bridge-unrecognized",
                "{odd}"
            );
        }
        // The same through upstream-style JSON.
        let json = r#"{"serverVersion":"8.1.0","protocolVersion":5,"studioBridge":false}"#;
        let info = parse_server_info(Some("application/json"), json.as_bytes()).unwrap();
        assert!(matches!(
            info.studio_bridge,
            Some(BridgeField::Unrecognized(_))
        ));
    }

    /// A MessagePack `/api/rojo` answer with valid fields whose `studioBridge`
    /// is `depth` nested single-element arrays, which a recursive decoder
    /// walks even though doctor ignores the value.
    fn deep_msgpack_info(depth: usize) -> Vec<u8> {
        let mut bytes = vec![0x83];
        for (key, value) in [
            ("serverVersion", &b"\xa58.1.0"[..]),
            ("protocolVersion", &[0x05]),
        ] {
            bytes.push(0xa0 | key.len() as u8);
            bytes.extend_from_slice(key.as_bytes());
            bytes.extend_from_slice(value);
        }
        bytes.push(0xa0 | "studioBridge".len() as u8);
        bytes.extend_from_slice(b"studioBridge");
        bytes.extend(std::iter::repeat_n(0x91, depth));
        bytes.push(0xc0);
        bytes
    }

    #[test]
    fn deeply_nested_answers_are_rejected_before_decoding() {
        // Within the limit the same shape still decodes.
        let shallow = deep_msgpack_info(MAX_INFO_DEPTH - 2);
        assert!(parse_server_info(Some("application/msgpack"), &shallow).is_ok());

        // Decoding this one on a test thread's stack would overflow it.
        let deep = deep_msgpack_info(100_000);
        let reason = parse_server_info(Some("application/msgpack"), &deep)
            .err()
            .unwrap();
        assert!(reason.contains("nested more than"), "{reason}");
        assert!(parse_server_info(None, &deep).is_err());

        let deep_json = format!(
            "{{\"serverVersion\":\"8.1.0\",\"protocolVersion\":5,\"studioBridge\":{}null{}}}",
            "[".repeat(100_000),
            "]".repeat(100_000)
        );
        let reason = parse_server_info(Some("application/json"), deep_json.as_bytes())
            .err()
            .unwrap();
        assert!(reason.contains("nested more than"), "{reason}");
    }

    #[test]
    fn depth_scans_count_only_real_nesting() {
        // Brackets inside strings, including after escaped quotes, do not count.
        let quoted = format!(r#"{{"a":"{}\"{}"}}"#, "[".repeat(100), "{".repeat(100));
        assert!(json_depth(quoted.as_bytes(), 2).is_ok());
        assert!(json_depth(b"[[[]]]", 3).is_ok());
        assert!(json_depth(b"[[[[]]]]", 3).is_err());

        // Scalars of every size, strings, binary and extensions are skipped.
        let flat = [
            0x9b, 0x01, 0xe0, 0xc0, 0xcc, 0xff, 0xcd, 0, 1, 0xa3, b'a', b'b', b'c', 0xc4, 2, 0, 0,
            0xd6, 1, 0, 0, 0, 0, 0xcb, 0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0x90,
        ];
        assert_eq!(msgpack_depth(&flat, 1), Ok(()));
        assert!(msgpack_depth(&[0x91, 0x91, 0xc0], 2).is_ok());
        assert!(msgpack_depth(&[0x91, 0x91, 0x91, 0xc0], 2).is_err());
        // Maps hold two values per entry.
        assert!(msgpack_depth(&[0x81, 0xc0, 0x91, 0xc0], 2).is_ok());
        assert!(msgpack_depth(&[0x81, 0xc0], 2).is_err());
        // Truncated or invalid input, and huge declared counts, fail cleanly.
        assert!(msgpack_depth(&[0xdd, 0xff, 0xff, 0xff, 0xff], 4).is_err());
        assert!(msgpack_depth(&[0xdb, 0xff, 0xff, 0xff, 0xff], 4).is_err());
        assert!(msgpack_depth(&[0xc1], 4).is_err());
        assert!(msgpack_depth(&[], 4).is_err());
        // The fork's real answer passes.
        assert!(msgpack_depth(&msgpack_info(CLI_VERSION, PROTOCOL_VERSION, true), 4).is_ok());
    }

    #[test]
    fn only_a_refused_connection_means_nothing_is_running() {
        let reason = || "error".to_owned();
        assert!(matches!(
            classify_send_error(false, true, true, reason()),
            Probe::NotRunning
        ));
        // A connect timeout is also a connect error; it must not claim absence.
        assert!(matches!(
            classify_send_error(true, true, false, reason()),
            Probe::Inconclusive(_)
        ));
        assert!(matches!(
            classify_send_error(false, true, false, reason()),
            Probe::Inconclusive(_)
        ));
        assert!(matches!(
            classify_send_error(true, false, false, reason()),
            Probe::NoAnswer
        ));
        assert!(matches!(
            classify_send_error(false, false, false, reason()),
            Probe::NotRojo(_)
        ));
        let address: SocketAddr = "127.0.0.1:34872".parse().unwrap();
        assert_eq!(
            codes(&check_server(
                address,
                Probe::Inconclusive("timed out".into()),
                None
            )),
            [("probe-inconclusive", Status::Warning)]
        );
    }

    #[test]
    fn redirects_are_not_followed() {
        use std::io::{Read, Write};
        // Port B answers with real server info; port A redirects to it.
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_address = target.local_addr().unwrap();
        let body = msgpack_info(CLI_VERSION, PROTOCOL_VERSION, false);
        let target_thread = std::thread::spawn(move || {
            target.set_nonblocking(true).unwrap();
            // Answer only if doctor wrongly follows the redirect.
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(3) {
                if let Ok((mut stream, _)) = target.accept() {
                    stream.set_nonblocking(false).unwrap();
                    let mut buffer = [0; 1024];
                    let _ = stream.read(&mut buffer);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/msgpack\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        });
        let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = redirect.local_addr().unwrap();
        let redirect_thread = std::thread::spawn(move || {
            let (mut stream, _) = redirect.accept().unwrap();
            let mut buffer = [0; 1024];
            let _ = stream.read(&mut buffer);
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/api/rojo\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let probe = fetch_server_info(address);
        redirect_thread.join().unwrap();
        match &probe {
            Probe::NotRojo(reason) => assert!(reason.contains("302"), "{reason}"),
            _ => panic!("expected not-rojo"),
        }
        assert_eq!(
            codes(&check_server(address, probe, None)),
            [("not-rojo", Status::Warning)]
        );
        assert!(
            !target_thread.join().unwrap(),
            "doctor followed the redirect"
        );
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
        // Doctor canonicalizes the project path, so the temp directory's
        // canonical form is what it reports (8.3 names on Windows runners,
        // /private/var on macOS).
        assert_eq!(
            report.project_file,
            Some(
                dunce::canonicalize(dir.path())
                    .unwrap()
                    .join("default.project.json")
            )
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
