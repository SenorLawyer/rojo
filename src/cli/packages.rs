use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Context};
use clap::{ArgEnum, Parser};
use memofs::Vfs;
use serde::Serialize;

use crate::{project::ProjectNode, Project};

/// Inspect package mappings or run an explicitly selected package manager.
#[derive(Debug, Parser)]
pub struct PackagesCommand {
    #[clap(subcommand)]
    pub subcommand: PackagesSubcommand,
}

#[derive(Debug, Parser)]
pub enum PackagesSubcommand {
    Doctor(PackagesDoctorCommand),
    Install(PackagesInstallCommand),
}

impl PackagesCommand {
    pub fn run(self) -> anyhow::Result<()> {
        match self.subcommand {
            PackagesSubcommand::Doctor(command) => command.run(),
            PackagesSubcommand::Install(command) => command.run(),
        }
    }
}

/// Check manifests, lockfiles, package directories and Rojo mappings without installing anything.
#[derive(Debug, Parser)]
pub struct PackagesDoctorCommand {
    /// Project directory or Rojo project file. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,
    /// Emit a versioned JSON report. Findings, including errors, are reported in it with a
    /// failing exit status. A project path that does not exist or is not a project fails before
    /// any report is written.
    #[clap(long)]
    pub json: bool,
}

impl PackagesDoctorCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let report = inspect(&self.project)?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("Package diagnostics for {}", report.directory.display());
            for manager in &report.managers {
                println!(
                    "{}: manifest found, lockfile {}",
                    manager.manager.name(),
                    if manager.lockfile_exists {
                        "present"
                    } else {
                        "missing"
                    },
                );
                for directory in &manager.package_directories {
                    println!(
                        "  {}: {}, {} Rojo mapping(s)",
                        directory.path.display(),
                        if directory.exists {
                            "present"
                        } else {
                            "missing"
                        },
                        directory.mappings.len(),
                    );
                }
            }
            for finding in &report.findings {
                println!(
                    "{} [{}]: {}",
                    finding.severity, finding.code, finding.message
                );
            }
            if report.findings.is_empty() {
                println!("No package mapping problems found.");
            }
        }
        if !report.ok {
            bail!("Package diagnostics found errors");
        }
        Ok(())
    }
}

/// Delegate installation to Wally or pesde. This can download packages and run manager hooks.
#[derive(Debug, Parser)]
pub struct PackagesInstallCommand {
    /// Project directory or Rojo project file. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,
    /// Select the manager explicitly, including when both manifests exist.
    #[clap(long, arg_enum)]
    pub manager: PackageManager,
    /// Native executable to use instead of resolving the manager through PATH.
    #[clap(long)]
    pub executable: Option<PathBuf>,
    /// Require pesde's lockfile to be current. Wally 0.3 does not support this mode.
    #[clap(long)]
    pub locked: bool,
    /// Additional native install arguments, following --.
    #[clap(last = true)]
    pub arguments: Vec<OsString>,
}

impl PackagesInstallCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let directory = project_directory(&self.project)?;
        if !directory.join(self.manager.manifest()).is_file() {
            bail!(
                "{} was not found in {}",
                self.manager.manifest(),
                directory.display()
            );
        }
        let executable = self
            .executable
            .unwrap_or_else(|| self.manager.name().into());
        let mut command = install_command(
            &directory,
            self.manager,
            &executable,
            self.locked,
            &self.arguments,
        )?;
        let status = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("Could not start {}. Install it or pass --executable with its native binary path", executable.display()))?;
        if !status.success() {
            bail!("{} install failed with {status}", self.manager.name());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, ArgEnum, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    Wally,
    Pesde,
}

impl PackageManager {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Wally => "wally",
            Self::Pesde => "pesde",
        }
    }

    fn manifest(self) -> &'static str {
        match self {
            Self::Wally => "wally.toml",
            Self::Pesde => "pesde.toml",
        }
    }

    fn lockfile(self) -> &'static str {
        match self {
            Self::Wally => "wally.lock",
            Self::Pesde => "pesde.lock",
        }
    }
}

fn install_command(
    directory: &Path,
    manager: PackageManager,
    executable: &Path,
    locked: bool,
    arguments: &[OsString],
) -> anyhow::Result<Command> {
    if manager == PackageManager::Wally && locked {
        bail!("Wally 0.3 does not provide a frozen/locked install mode. Commit wally.lock and verify it remains unchanged in CI; Rojo cannot promise a frozen Wally install.");
    }
    #[cfg(windows)]
    if executable
        .extension()
        .is_some_and(|extension| !extension.eq_ignore_ascii_case("exe"))
    {
        bail!("--executable must identify a native .exe, not a shell or batch script");
    }
    // Resolve explicit relative binary paths before changing the child directory.
    let executable = if executable.is_relative() && executable.components().count() > 1 {
        std::env::current_dir()?.join(executable)
    } else {
        executable.to_path_buf()
    };
    let mut command = Command::new(executable);
    command.current_dir(directory).arg("install");
    if locked {
        command.arg("--locked");
    }
    command.args(arguments);
    Ok(command)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DoctorReport {
    schema_version: u32,
    directory: PathBuf,
    project_file: Option<PathBuf>,
    pub(super) managers: Vec<ManagerReport>,
    pub(super) findings: Vec<Finding>,
    ok: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ManagerReport {
    pub(super) manager: PackageManager,
    manifest: PathBuf,
    lockfile: PathBuf,
    lockfile_exists: bool,
    dependency_counts: BTreeMap<String, usize>,
    roblox_sync_config_generator: Option<String>,
    package_directories: Vec<PackageDirectory>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PackageDirectory {
    path: PathBuf,
    exists: bool,
    server_only: bool,
    mappings: Vec<Mapping>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Mapping {
    filesystem_path: PathBuf,
    instance_path: String,
    server_only: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct Finding {
    pub(super) severity: &'static str,
    pub(super) code: &'static str,
    pub(super) message: String,
}

impl DoctorReport {
    fn finding(&mut self, severity: &'static str, code: &'static str, message: String) {
        self.findings.push(Finding {
            severity,
            code,
            message,
        });
        self.ok &= severity != "error";
    }
}

fn project_directory(path: &Path) -> anyhow::Result<PathBuf> {
    let path = super::resolve_path(path)?;
    let path = dunce::canonicalize(path.as_ref())
        .with_context(|| format!("Could not open project path {}", path.display()))?;
    if path.is_dir() {
        Ok(path)
    } else if Project::is_project_file(&path) {
        Ok(path
            .parent()
            .context("Project file has no parent")?
            .to_path_buf())
    } else {
        bail!("Expected a project directory or a .project.json/.project.jsonc file");
    }
}

pub(super) fn inspect(path: &Path) -> anyhow::Result<DoctorReport> {
    let directory = project_directory(path)?;
    let project_path = if Project::is_project_file(path) {
        dunce::canonicalize(path)?
    } else {
        directory.clone()
    };
    let mut report = DoctorReport {
        schema_version: 1,
        directory: directory.clone(),
        project_file: None,
        managers: Vec::new(),
        findings: Vec::new(),
        ok: true,
    };
    let mut mappings = Vec::new();
    // Doctor does not evaluate ignore globs, so it cannot tell whether one
    // keeps a package folder out of a broader mapping.
    let mut has_glob_ignores = false;
    match Project::load_fuzzy(&Vfs::new_default_without_watch(), &project_path) {
        Ok(Some(project)) => {
            report.project_file = Some(project.file_location.clone());
            has_glob_ignores = !project.glob_ignore_paths.is_empty();
            // Only a place project's children are services. In a model project
            // a child named ServerStorage is an ordinary instance.
            let is_place = project.tree.class_name.as_deref() == Some("DataModel");
            let root_path = if is_place {
                "game"
            } else {
                project.name.as_deref().unwrap_or("model")
            };
            collect_mappings(
                &project.tree,
                project.folder_location(),
                root_path,
                is_place,
                false,
                &mut mappings,
            );
        }
        Ok(None) => report.finding(
            "warning",
            "project-missing",
            "No default Rojo project found. Pass a project file to inspect a named configuration."
                .into(),
        ),
        Err(error) => report.finding("error", "project-invalid", error.to_string()),
    }

    for manager in [PackageManager::Wally, PackageManager::Pesde] {
        let manifest = directory.join(manager.manifest());
        if !manifest.is_file() {
            continue;
        }
        let value: toml::Value = match fs_err::read_to_string(&manifest)
            .map_err(anyhow::Error::from)
            .and_then(|contents| toml::from_str(&contents).map_err(Into::into))
        {
            Ok(value) => value,
            Err(error) => {
                report.finding(
                    "error",
                    "manifest-invalid",
                    format!("{}: {error}", manifest.display()),
                );
                continue;
            }
        };
        let sections: &[&str] = match manager {
            PackageManager::Wally => &["dependencies", "server-dependencies", "dev-dependencies"],
            PackageManager::Pesde => &["dependencies", "dev_dependencies", "peer_dependencies"],
        };
        let dependency_counts: BTreeMap<_, _> = sections
            .iter()
            .map(|section| {
                (
                    (*section).to_owned(),
                    value
                        .get(*section)
                        .and_then(toml::Value::as_table)
                        .map_or(0, |table| table.len()),
                )
            })
            .collect();
        let lockfile = directory.join(manager.lockfile());
        let lockfile_exists = lockfile.is_file();
        if !lockfile_exists {
            report.finding(
                "warning",
                "lockfile-missing",
                format!(
                    "{} is missing; resolve dependencies with {} and commit its lockfile.",
                    manager.lockfile(),
                    manager.name()
                ),
            );
        }
        let generator = value
            .get("scripts")
            .and_then(|scripts| scripts.get("roblox_sync_config_generator"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned);
        let target_environment = value
            .get("target")
            .and_then(|target| target.get("environment"))
            .and_then(toml::Value::as_str);
        let roblox_target = matches!(target_environment, Some("roblox" | "roblox_server"));
        if manager == PackageManager::Pesde && roblox_target && generator.is_none() {
            report.finding("error", "pesde-generator-missing", "Roblox pesde projects need scripts.roblox_sync_config_generator. Configure pesde/scripts_rojo as described in https://docs.pesde.dev/guides/roblox/.".into());
        }
        if manager == PackageManager::Pesde {
            if let Some(generator) = &generator {
                if !directory.join(generator).is_file() {
                    report.finding("warning", "pesde-generator-unavailable", format!("Configured generator {generator} does not exist. Install the scripts package and check its alias; pesde init normally uses .pesde/scripts/roblox_sync_config_generator.luau."));
                }
            }
        }
        // Each folder lists the manifest sections that install into it. Wally
        // installs dev-dependencies into DevPackages, which Rojo ignores, and
        // pesde installs everything into the folder for its target.
        const PESDE_SECTIONS: &[&str] = &["dependencies", "dev_dependencies", "peer_dependencies"];
        // pesde installs into `<environment>_packages`. Lune and Luau targets
        // run outside Roblox, so their folders are not expected in a Rojo tree.
        let pesde_folder;
        let paths: Vec<(&str, bool, &[&str])> = match manager {
            PackageManager::Wally => vec![
                ("Packages", false, &["dependencies"]),
                ("ServerPackages", true, &["server-dependencies"]),
            ],
            PackageManager::Pesde if target_environment == Some("roblox_server") => vec![
                ("roblox_packages", false, &[]),
                ("roblox_server_packages", true, PESDE_SECTIONS),
            ],
            PackageManager::Pesde if target_environment.is_none() || roblox_target => vec![
                ("roblox_packages", false, PESDE_SECTIONS),
                ("roblox_server_packages", true, &[]),
            ],
            PackageManager::Pesde => {
                pesde_folder = format!("{}_packages", target_environment.unwrap_or_default());
                vec![(pesde_folder.as_str(), false, PESDE_SECTIONS)]
            }
        };
        let synced_by_rojo =
            manager == PackageManager::Wally || target_environment.is_none() || roblox_target;
        let mut package_directories = Vec::new();
        for (name, server_only, installed_sections) in paths {
            let has_dependencies = installed_sections.iter().any(|section| {
                dependency_counts
                    .get(*section)
                    .is_some_and(|count| *count > 0)
            });
            let package_path = directory.join(name);
            let exists = package_path.is_dir();
            let normalized_package_path = normalized(&package_path);
            let matched: Vec<_> = mappings
                .iter()
                .filter(|mapping| {
                    normalized_package_path.starts_with(&mapping.filesystem_path)
                        || mapping
                            .filesystem_path
                            .starts_with(&normalized_package_path)
                })
                .cloned()
                .collect();
            let exposing: Vec<_> = matched
                .iter()
                .filter(|mapping| !mapping.server_only)
                .collect();
            if server_only && exists && !exposing.is_empty() {
                // An ancestor mapping such as `$path: "."` may leave the folder
                // out through globIgnorePaths, which doctor does not apply.
                let possibly_ignored = has_glob_ignores
                    && exposing.iter().all(|mapping| {
                        mapping.filesystem_path != normalized_package_path
                            && normalized_package_path.starts_with(&mapping.filesystem_path)
                    });
                if possibly_ignored {
                    report.finding("warning", "server-packages-possibly-exposed", format!("{} is inside a mapping outside ServerScriptService/ServerStorage. Doctor does not apply globIgnorePaths; check that they exclude it, or map it only under a server-only service.", name));
                } else {
                    report.finding("error", "server-packages-exposed", format!("{} is mapped outside ServerScriptService/ServerStorage. Move its mapping to a server-only service.", name));
                }
            }
            if exists && matched.is_empty() && synced_by_rojo {
                report.finding(
                    "warning",
                    "packages-unmapped",
                    format!("{} exists but no Rojo $path includes it.", name),
                );
            }
            if !exists && has_dependencies {
                report.finding(
                    "warning",
                    "packages-not-installed",
                    format!(
                        "{name} does not exist. Run {} install, then check its Rojo mapping.",
                        manager.name()
                    ),
                );
            }
            package_directories.push(PackageDirectory {
                path: package_path,
                exists,
                server_only,
                mappings: matched,
            });
        }
        report.managers.push(ManagerReport {
            manager,
            manifest,
            lockfile,
            lockfile_exists,
            dependency_counts,
            roblox_sync_config_generator: generator,
            package_directories,
        });
    }
    if report.managers.is_empty()
        && !report
            .findings
            .iter()
            .any(|finding| finding.code == "manifest-invalid")
    {
        report.finding(
            "warning",
            "manifest-missing",
            "No wally.toml or pesde.toml found in the selected project directory.".into(),
        );
    }
    Ok(report)
}

fn normalized(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    dunce::canonicalize(&output).unwrap_or(output)
}

fn collect_mappings(
    node: &ProjectNode,
    base: &Path,
    instance_path: &str,
    is_data_model: bool,
    inherited_server: bool,
    mappings: &mut Vec<Mapping>,
) {
    let server_only = inherited_server
        || node
            .class_name
            .as_ref()
            .is_some_and(|class| matches!(class.as_str(), "ServerScriptService" | "ServerStorage"));
    if let Some(path) = &node.path {
        mappings.push(Mapping {
            filesystem_path: normalized(&base.join(path.path())),
            instance_path: instance_path.into(),
            server_only,
        });
    }
    for (name, child) in &node.children {
        let child_server = server_only
            || is_data_model
                && matches!(
                    child.class_name.as_deref().unwrap_or(name),
                    "ServerScriptService" | "ServerStorage"
                );
        collect_mappings(
            child,
            base,
            &format!("{instance_path}.{name}"),
            false,
            child_server,
            mappings,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn doctor_finds_lockfiles_and_server_package_exposure() {
        let root = tempdir().unwrap();
        fs_err::write(
            root.path().join("wally.toml"),
            "[dependencies]\nPromise = 'evaera/promise@4.0.0'\n",
        )
        .unwrap();
        fs_err::write(root.path().join("wally.lock"), "lockfile fixture").unwrap();
        fs_err::create_dir(root.path().join("Packages")).unwrap();
        fs_err::create_dir(root.path().join("ServerPackages")).unwrap();
        fs_err::write(root.path().join("default.project.json"), r#"{"name":"test","tree":{"$className":"DataModel","ReplicatedStorage":{"Packages":{"$path":"Packages"},"Leaked":{"$path":"ServerPackages"}}}}"#).unwrap();
        let report = inspect(root.path()).unwrap();
        assert!(!report.ok);
        assert!(report.managers[0].lockfile_exists);
        assert!(report
            .findings
            .iter()
            .any(|finding| finding.code == "server-packages-exposed"));
        assert_eq!(report.managers[0].dependency_counts["dependencies"], 1);
    }

    #[test]
    fn doctor_warns_when_glob_ignores_may_hide_server_packages_from_an_ancestor_mapping() {
        let root = tempdir().unwrap();
        fs_err::write(
            root.path().join("wally.toml"),
            "[server-dependencies]
Lib = 'a/lib@1.0.0'
",
        )
        .unwrap();
        fs_err::write(root.path().join("wally.lock"), "lockfile fixture").unwrap();
        fs_err::create_dir(root.path().join("ServerPackages")).unwrap();
        let project = |extra: &str| {
            format!(
                r#"{{"name":"test",{extra}"tree":{{"$className":"DataModel","ReplicatedStorage":{{"Root":{{"$path":"."}}}},"ServerStorage":{{"ServerPackages":{{"$path":"ServerPackages"}}}}}}}}"#
            )
        };
        let codes = |report: &DoctorReport| -> Vec<(&'static str, &'static str)> {
            report
                .findings
                .iter()
                .map(|finding| (finding.severity, finding.code))
                .collect()
        };

        fs_err::write(
            root.path().join("default.project.json"),
            project(r#""globIgnorePaths":["ServerPackages/**"],"#),
        )
        .unwrap();
        let report = inspect(root.path()).unwrap();
        assert!(report.ok);
        assert!(codes(&report).contains(&("warning", "server-packages-possibly-exposed")));
        assert!(!codes(&report)
            .iter()
            .any(|(_, code)| *code == "server-packages-exposed"));

        // Without ignore globs the ancestor mapping certainly includes it.
        fs_err::write(root.path().join("default.project.json"), project("")).unwrap();
        let report = inspect(root.path()).unwrap();
        assert!(!report.ok);
        assert!(codes(&report).contains(&("error", "server-packages-exposed")));

        // Ignore globs cannot hide a folder that is mapped directly.
        fs_err::write(
            root.path().join("default.project.json"),
            r#"{"name":"test","globIgnorePaths":["ServerPackages/**"],"tree":{"$className":"DataModel","ReplicatedStorage":{"Leaked":{"$path":"ServerPackages"}}}}"#,
        )
        .unwrap();
        let report = inspect(root.path()).unwrap();
        assert!(!report.ok);
        assert!(codes(&report).contains(&("error", "server-packages-exposed")));
    }

    #[test]
    fn doctor_checks_pesde_generator_and_inferred_server_service() {
        let root = tempdir().unwrap();
        fs_err::write(
            root.path().join("pesde.toml"),
            "[target]\nenvironment = 'roblox'\n",
        )
        .unwrap();
        fs_err::create_dir(root.path().join("roblox_server_packages")).unwrap();
        fs_err::write(root.path().join("default.project.json"), r#"{"name":"test","tree":{"$className":"DataModel","ServerScriptService":{"Packages":{"$path":"roblox_server_packages"}}}}"#).unwrap();
        let report = inspect(root.path()).unwrap();
        assert!(report
            .findings
            .iter()
            .any(|finding| finding.code == "pesde-generator-missing"));
        assert!(!report
            .findings
            .iter()
            .any(|finding| finding.code == "server-packages-exposed"));
    }

    #[test]
    fn native_install_preserves_arguments_and_honest_lock_semantics() {
        let arguments = vec![OsString::from("--prod"), OsString::from("space ; literal")];
        let command = install_command(
            Path::new("."),
            PackageManager::Pesde,
            Path::new("pesde"),
            true,
            &arguments,
        )
        .unwrap();
        assert_eq!(command.get_program(), "pesde");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["install", "--locked", "--prod", "space ; literal"]
        );
        assert!(install_command(
            Path::new("."),
            PackageManager::Wally,
            Path::new("wally"),
            true,
            &[]
        )
        .is_err());
    }

    #[test]
    fn invalid_manifest_still_produces_a_structured_report() {
        let root = tempdir().unwrap();
        fs_err::write(root.path().join("wally.toml"), "[broken").unwrap();
        let report = inspect(root.path()).unwrap();
        assert!(!report.ok);
        assert!(report
            .findings
            .iter()
            .any(|finding| finding.code == "manifest-invalid"));
        assert!(serde_json::to_value(report).unwrap()["schemaVersion"] == 1);
    }

    #[test]
    fn generator_path_diagnostic_is_reported_once() {
        let root = tempdir().unwrap();
        fs_err::write(root.path().join("pesde.toml"), "[target]\nenvironment = 'roblox'\n[scripts]\nroblox_sync_config_generator = '.pesde/wrong-alias/generator.luau'\n").unwrap();
        let report = inspect(root.path()).unwrap();
        assert_eq!(
            report
                .findings
                .iter()
                .filter(|finding| finding.code == "pesde-generator-unavailable")
                .count(),
            1
        );
    }

    #[test]
    fn doctor_checks_both_managers_and_ancestor_mappings() {
        let root = tempdir().unwrap();
        fs_err::write(root.path().join("wally.toml"), "[dependencies]\n").unwrap();
        fs_err::write(root.path().join("pesde.toml"), "[dependencies]\n").unwrap();
        fs_err::create_dir(root.path().join("Packages")).unwrap();
        fs_err::create_dir(root.path().join("roblox_packages")).unwrap();
        fs_err::write(root.path().join("default.project.json"), r#"{"name":"test","tree":{"$className":"DataModel","ReplicatedStorage":{"Dependencies":{"$path":"."}}}}"#).unwrap();
        let report = inspect(root.path()).unwrap();
        assert_eq!(report.managers.len(), 2);
        for manager in &report.managers {
            assert_eq!(manager.package_directories[0].mappings.len(), 1);
        }
        assert!(!report
            .findings
            .iter()
            .any(|finding| finding.code == "packages-unmapped"));
    }

    #[test]
    fn doctor_checks_server_package_subdirectory_mappings_for_both_managers() {
        for (manifest, packages) in [
            ("wally.toml", "ServerPackages"),
            ("pesde.toml", "roblox_server_packages"),
        ] {
            let root = tempdir().unwrap();
            fs_err::write(root.path().join(manifest), "[dependencies]\n").unwrap();
            fs_err::create_dir_all(root.path().join(packages).join("PrivateLib")).unwrap();
            for (service, should_be_exposed) in [
                ("ReplicatedStorage", true),
                ("ServerStorage", false),
                ("ServerScriptService", false),
            ] {
                let mut tree = serde_json::json!({"$className":"DataModel"});
                tree[service] = serde_json::json!({
                    "PrivateLib": {"$path":format!("{packages}/PrivateLib")}
                });
                fs_err::write(
                    root.path().join("default.project.json"),
                    serde_json::to_vec(
                        &serde_json::json!({"name":"subdirectory-test","tree":tree}),
                    )
                    .unwrap(),
                )
                .unwrap();

                let report = inspect(root.path()).unwrap();
                assert_eq!(
                    report
                        .findings
                        .iter()
                        .any(|finding| finding.code == "server-packages-exposed"),
                    should_be_exposed,
                    "{manifest} under {service}",
                );
                assert_eq!(report.ok, !should_be_exposed, "{manifest} under {service}");
                assert_eq!(report.managers[0].package_directories[1].mappings.len(), 1);
            }
        }
    }

    fn not_installed(report: &DoctorReport) -> Vec<&str> {
        report
            .findings
            .iter()
            .filter(|finding| finding.code == "packages-not-installed")
            .map(|finding| finding.message.as_str())
            .collect()
    }

    #[test]
    fn pesde_server_target_checks_server_packages() {
        let root = tempdir().unwrap();
        fs_err::write(
            root.path().join("pesde.toml"),
            "[target]
environment = 'roblox_server'
[dependencies]
lib = { name = 'a/b', version = '1' }
",
        )
        .unwrap();
        fs_err::write(
            root.path().join("default.project.json"),
            r#"{"name":"test","tree":{"$className":"DataModel"}}"#,
        )
        .unwrap();
        let report = inspect(root.path()).unwrap();
        let missing = not_installed(&report);
        assert_eq!(missing.len(), 1);
        assert!(
            missing[0].starts_with("roblox_server_packages "),
            "{missing:?}"
        );

        fs_err::create_dir(root.path().join("roblox_server_packages")).unwrap();
        let report = inspect(root.path()).unwrap();
        assert!(not_installed(&report).is_empty());
    }

    #[test]
    fn wally_dev_dependencies_do_not_require_packages() {
        let root = tempdir().unwrap();
        fs_err::write(
            root.path().join("wally.toml"),
            "[dev-dependencies]
TestEZ = 'roblox/testez@0.4.1'
",
        )
        .unwrap();
        fs_err::write(
            root.path().join("default.project.json"),
            r#"{"name":"test","tree":{"$className":"DataModel"}}"#,
        )
        .unwrap();
        let report = inspect(root.path()).unwrap();
        assert!(not_installed(&report).is_empty());

        fs_err::write(
            root.path().join("wally.toml"),
            "[server-dependencies]
Lib = 'a/b@1.0.0'
",
        )
        .unwrap();
        let report = inspect(root.path()).unwrap();
        let missing = not_installed(&report);
        assert_eq!(missing.len(), 1);
        assert!(missing[0].starts_with("ServerPackages "), "{missing:?}");
    }

    #[test]
    fn explicit_service_class_overrides_a_server_only_name() {
        for (manifest, packages) in [
            ("wally.toml", "ServerPackages"),
            ("pesde.toml", "roblox_server_packages"),
        ] {
            let root = tempdir().unwrap();
            fs_err::write(root.path().join(manifest), "[dependencies]\n").unwrap();
            fs_err::create_dir(root.path().join(packages)).unwrap();
            fs_err::write(
                root.path().join("default.project.json"),
                serde_json::to_vec(&serde_json::json!({
                    "name": "override", "tree": {"$className": "DataModel", "ServerStorage": {
                        "$className": "ReplicatedStorage", "Private": {"$path": packages}
                    }}
                }))
                .unwrap(),
            )
            .unwrap();
            let report = inspect(root.path()).unwrap();
            assert!(
                report
                    .findings
                    .iter()
                    .any(|finding| finding.code == "server-packages-exposed"),
                "{manifest}"
            );
        }
    }

    #[test]
    fn pesde_lune_and_luau_targets_use_their_own_package_folders() {
        for environment in ["lune", "luau"] {
            let root = tempdir().unwrap();
            fs_err::write(
                root.path().join("pesde.toml"),
                format!(
                    "[target]
environment = '{environment}'
[dependencies]
lib = {{ name = 'a/b', version = '1' }}
"
                ),
            )
            .unwrap();
            let report = inspect(root.path()).unwrap();
            let folder = format!("{environment}_packages");
            let missing = not_installed(&report);
            assert_eq!(missing.len(), 1, "{missing:?}");
            assert!(missing[0].starts_with(&format!("{folder} ")), "{missing:?}");

            // An installed folder satisfies the check and does not need a Rojo mapping.
            fs_err::create_dir(root.path().join(&folder)).unwrap();
            let report = inspect(root.path()).unwrap();
            assert!(not_installed(&report).is_empty());
            assert!(!report
                .findings
                .iter()
                .any(|finding| finding.code == "packages-unmapped"));
            assert_eq!(report.managers[0].package_directories.len(), 1);
        }
    }

    #[test]
    fn model_projects_do_not_infer_services_from_child_names() {
        let root = tempdir().unwrap();
        fs_err::write(
            root.path().join("wally.toml"),
            "[dependencies]
",
        )
        .unwrap();
        fs_err::create_dir(root.path().join("ServerPackages")).unwrap();
        fs_err::write(
            root.path().join("default.project.json"),
            r#"{"name":"Library","tree":{"$className":"Folder","ServerStorage":{"Private":{"$path":"ServerPackages"}}}}"#,
        )
        .unwrap();
        let report = inspect(root.path()).unwrap();
        // In a model, "ServerStorage" is just a Folder child, so wherever the model is
        // inserted these packages are not protected by a server-only service.
        assert!(report
            .findings
            .iter()
            .any(|finding| finding.code == "server-packages-exposed"));
        let mapping = &report.managers[0].package_directories[1].mappings[0];
        assert_eq!(mapping.instance_path, "Library.ServerStorage.Private");
    }
}
