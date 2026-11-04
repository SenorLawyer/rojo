//! Process adapter for the separately installed, pinned rbx-merge engine.

use std::{
    env, fs,
    io::{self, Cursor},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use anyhow::{bail, Context};
use clap::Parser;
use memofs::IoResultExt;
use serde::{Deserialize, Serialize};

const ENGINE_REPOSITORY: &str = "https://github.com/kennethloeffler/rbx-merge";
const ENGINE_REVISION: &str = "d6375bde1bdc1f08437324ebfbd6bfcc655e90d0";
const ENGINE_VERSION: &str = "rbx-merge 0.2.0";
const ENGINE_RECEIPT: &str = "rojo-engine.json";
/// Name prefix of the engine's staged output, next to the destination.
const STAGED_PREFIX: &str = ".rojo-merge-";

/// Semantic diff and three-way merge for Roblox place and model files.
#[derive(Debug, Parser)]
pub struct MergeCommand {
    /// Use a trusted external rbx-merge executable instead of the pinned installation.
    #[clap(long, global = true)]
    pub engine: Option<PathBuf>,

    #[clap(subcommand)]
    pub command: MergeSubcommand,
}

#[derive(Debug, Parser)]
pub enum MergeSubcommand {
    /// Build the pinned engine into a private tool directory. Requires Cargo and Git.
    InstallEngine {
        /// Installation root. Defaults to Rojo's per-user cache for this exact revision.
        #[clap(long)]
        root: Option<PathBuf>,
    },
    /// Merge three files, replacing the output only after a clean, validated result.
    Run(Box<MergeFiles>),
    /// Re-run a stashed conflict after editing its conflicts.txt resolutions.
    Resolve {
        #[clap(long)]
        stash_dir: PathBuf,
        #[clap(long)]
        out: PathBuf,
    },
    /// Print the engine's deterministic instance and property representation.
    Textconv {
        path: PathBuf,
        #[clap(long)]
        all_properties: bool,
    },
    /// Print semantic representations of both sides with file headers.
    Diff {
        old: PathBuf,
        new: PathBuf,
        #[clap(long)]
        all_properties: bool,
    },
    /// Activate the engine's Git drivers for this clone and its linked worktrees.
    Install {
        /// Also add binary safe defaults to the clone's committed .gitattributes.
        #[clap(long)]
        write_gitattributes: bool,
    },
    /// Check this clone's engine and Git driver configuration.
    Doctor,
    /// Remove the engine's local Git driver configuration. Leaves committed defaults.
    Uninstall,
}

#[derive(Debug, Parser)]
pub struct MergeFiles {
    #[clap(long)]
    pub base: PathBuf,
    #[clap(long)]
    pub ours: PathBuf,
    #[clap(long)]
    pub theirs: PathBuf,
    #[clap(long)]
    pub out: PathBuf,
    /// Original Roblox path, used when Git's input and output temporary files lack extensions.
    #[clap(long)]
    pub path: Option<PathBuf>,
    /// Explicitly take this side for every unresolved conflict: base, ours, or theirs.
    #[clap(long, possible_values = ["base", "ours", "theirs"])]
    pub take: Option<String>,
    /// Edited per-conflict resolutions from a previous conflict report.
    #[clap(long)]
    pub resolutions: Option<PathBuf>,
    /// Write a new conflict report here. Existing files are never overwritten.
    #[clap(long)]
    pub conflicts_out: Option<PathBuf>,
    /// Save conflict inputs and report in a new directory for a later resolve.
    #[clap(long)]
    pub stash_dir: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct EngineReceipt {
    revision: String,
    version: String,
    blake3: String,
}

impl MergeCommand {
    pub fn run(self) -> anyhow::Result<()> {
        if let MergeSubcommand::InstallEngine { root } = &self.command {
            return install_engine(root.as_deref());
        }
        let engine = select_engine(self.engine.as_deref())?;
        match self.command {
            MergeSubcommand::Run(options) => merge_files(&engine, *options),
            MergeSubcommand::Resolve { stash_dir, out } => {
                let mut child = process(&engine);
                child.arg("resolve").arg("--stash-dir").arg(&stash_dir);
                publish_output(&mut child, &out, &out)
            }
            MergeSubcommand::Textconv {
                path,
                all_properties,
            } => {
                let mut child = process(&engine);
                child.arg("textconv").arg(path);
                if all_properties {
                    child.arg("--all-properties");
                }
                require_success(child.status().context("Could not run rbx-merge textconv")?)
            }
            MergeSubcommand::Diff {
                old,
                new,
                all_properties,
            } => {
                let mut child = process(&engine);
                child.arg("diff").arg(old).arg(new);
                if all_properties {
                    child.arg("--all-properties");
                }
                require_success(child.status().context("Could not run rbx-merge diff")?)
            }
            MergeSubcommand::Install {
                write_gitattributes,
            } => {
                let mut child = process(&engine);
                // Git resolves the executable through sh on Windows too. Use
                // forward slashes, then let the engine quote that single path.
                let driver_path = engine
                    .to_str()
                    .context("Git drivers require a UTF-8 engine path")?
                    .replace('\\', "/");
                child.arg("install").arg("--driver-path").arg(driver_path);
                if write_gitattributes {
                    child.arg("--write-gitattributes");
                }
                require_success(
                    child
                        .status()
                        .context("Could not install local merge drivers")?,
                )
            }
            MergeSubcommand::Doctor => require_success(process(&engine).arg("doctor").status()?),
            MergeSubcommand::Uninstall => {
                require_success(process(&engine).arg("uninstall").status()?)
            }
            MergeSubcommand::InstallEngine { .. } => unreachable!(),
        }
    }
}

fn merge_files(engine: &Path, options: MergeFiles) -> anyhow::Result<()> {
    for input in [&options.base, &options.ours, &options.theirs] {
        fs::File::open(input)
            .with_context(|| format!("Could not read merge input {}", input.display()))?;
    }
    if let Some(report) = &options.conflicts_out {
        ensure_new_artifact(report)?;
        if paths_overlap(report, &options.out)? {
            bail!("Conflict report must not be the Roblox output file");
        }
    }
    if let Some(stash) = &options.stash_dir {
        ensure_new_artifact(stash)?;
        if paths_overlap(stash, &options.out)? {
            bail!("Conflict stash must not be the Roblox output file");
        }
    }
    let hint = options.path.as_deref().unwrap_or(&options.out);
    let mut child = process(engine);
    child
        .arg("merge")
        .arg("--base")
        .arg(&options.base)
        .arg("--ours")
        .arg(&options.ours)
        .arg("--theirs")
        .arg(&options.theirs)
        .arg("--path")
        .arg(hint);
    if let Some(take) = &options.take {
        child.arg("--take").arg(take);
    }
    if let Some(resolutions) = &options.resolutions {
        child.arg("--resolutions").arg(resolutions);
    }
    if let Some(report) = &options.conflicts_out {
        child.arg("--conflicts-out").arg(report);
    }
    if let Some(stash) = &options.stash_dir {
        child.arg("--stash-dir").arg(stash);
    }
    publish_output(&mut child, &options.out, hint)
}

fn paths_overlap(left: &Path, right: &Path) -> anyhow::Result<bool> {
    let left = absolute_target(left)?;
    let right = absolute_target(right)?;
    Ok(left.starts_with(&right) || right.starts_with(&left))
}

fn ensure_new_artifact(path: &Path) -> anyhow::Result<()> {
    if fs::symlink_metadata(path).with_not_found()?.is_some() {
        bail!("Conflict artifact {} already exists; choose a new path to preserve previous resolutions", path.display());
    }
    Ok(())
}

fn absolute_target(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match dunce::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for name in suffix.iter().rev() {
                    canonical.push(name);
                }
                return Ok(canonical);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                suffix.push(
                    ancestor
                        .file_name()
                        .context("Path has no surviving ancestor")?
                        .to_owned(),
                );
                ancestor = ancestor.parent().context("Path has no parent")?;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn output_digest(path: &Path) -> io::Result<Option<blake3::Hash>> {
    let Some(mut file) = fs::File::open(path).with_not_found()? else {
        return Ok(None);
    };
    let mut hash = blake3::Hasher::new();
    hash.update_reader(&mut file)?;
    Ok(Some(hash.finalize()))
}

fn publish_output(child: &mut Command, out: &Path, hint: &Path) -> anyhow::Result<()> {
    if fs::symlink_metadata(out)
        .with_not_found()?
        .is_some_and(|meta| meta.file_type().is_symlink())
    {
        bail!(
            "Refusing to replace a symbolic-link output: {}",
            out.display()
        );
    }
    let destination = absolute_target(out)?;
    // Check the shape of the destination before reading it: on Unix, reading a
    // directory such as `/` fails with an unrelated "Is a directory" error.
    let parent = destination
        .parent()
        .with_context(|| format!("Output {} has no parent directory", out.display()))?;
    if destination.is_dir() {
        bail!("Output {} is a directory", out.display());
    }
    let original = output_digest(&destination)?;
    // A recognizable name lets users find and delete leftovers from a merge
    // that was killed before Rojo could clean up.
    let staged = tempfile::Builder::new()
        .prefix(STAGED_PREFIX)
        .suffix(".tmp")
        .tempfile_in(parent)?;
    child.arg("--out").arg(staged.path());
    let status = child
        .status()
        .context("Could not run rbx-merge; output was preserved")?;
    if !status.success() {
        bail!("rbx-merge exited with {}; output {} was preserved. Review the reported conflicts before resolving", status, out.display());
    }
    let bytes = fs::read(staged.path())?;
    validate_output(&bytes, hint)
        .context("Engine output was invalid; destination was preserved")?;
    match_permissions(staged.as_file(), &destination)?;
    staged.as_file().sync_all()?;
    if output_digest(&destination)? != original {
        bail!(
            "Output {} changed during the merge; preserving the newer file",
            out.display()
        );
    }
    staged
        .persist(&destination)
        .with_context(|| format!("Could not publish merged output {}", out.display()))?;
    Ok(())
}

/// Temporary files are created owner-only (0600) on Unix. Keep the mode of the
/// file being replaced, or use 0644 for a new file as the default umask would.
fn match_permissions(staged: &fs::File, destination: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = match fs::metadata(destination) {
            Ok(metadata) => metadata.permissions(),
            Err(_) => fs::Permissions::from_mode(0o644),
        };
        staged.set_permissions(permissions)?;
    }
    #[cfg(not(unix))]
    let _ = (staged, destination);
    Ok(())
}

fn validate_output(bytes: &[u8], hint: &Path) -> anyhow::Result<()> {
    // Studio and Windows tools often write upper-case extensions such as `.RBXL`.
    let extension = hint
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("rbxm" | "rbxl") => {
            rbx_binary::from_reader(Cursor::new(bytes))?;
        }
        Some("rbxmx" | "rbxlx") => {
            let options = rbx_xml::DecodeOptions::new()
                .property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown);
            rbx_xml::from_reader(Cursor::new(bytes), options)?;
        }
        _ => bail!("Use a .rbxm, .rbxl, .rbxmx, or .rbxlx output path or --path hint"),
    }
    Ok(())
}

/// Engine commands share Rojo's console, so Ctrl+C stops the engine together
/// with Rojo instead of leaving it running and holding the staged output.
fn process(executable: impl AsRef<std::ffi::OsStr>) -> Command {
    Command::new(executable)
}

/// For the short `--version` probe only. When Rojo runs without a console (for
/// example from a GUI Git client) this keeps a console window from flashing up,
/// and the probe finishes too quickly for Ctrl+C to matter.
fn quiet_process(executable: impl AsRef<std::ffi::OsStr>) -> Command {
    let command = Command::new(executable);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let mut command = command;
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    command
}

fn require_success(status: ExitStatus) -> anyhow::Result<()> {
    if !status.success() {
        bail!("rbx-merge exited with {}", status);
    }
    Ok(())
}

fn default_engine_root() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    let cache = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let cache = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
    Ok(cache
        .context("Could not find the user cache; supply an explicit --engine path")?
        .join("Rojo")
        .join("rbx-merge")
        .join(ENGINE_REVISION))
}

fn engine_binary(root: &Path) -> PathBuf {
    root.join("bin")
        .join(format!("rbx-merge{}", env::consts::EXE_SUFFIX))
}

fn verify_version(engine: &Path) -> anyhow::Result<()> {
    let output = quiet_process(engine)
        .arg("--version")
        .output()
        .with_context(|| {
            format!(
                "Could not run {}; install the pinned engine with `rojo merge install-engine`",
                engine.display()
            )
        })?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != ENGINE_VERSION
    {
        bail!("Expected {ENGINE_VERSION} at {}; use `rojo merge install-engine` or explicitly select a compatible --engine", engine.display());
    }
    Ok(())
}

fn select_engine(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    let override_path = explicit
        .map(PathBuf::from)
        .or_else(|| env::var_os("ROJO_RBX_MERGE").map(PathBuf::from));
    if let Some(path) = override_path {
        let engine = dunce::canonicalize(&path)
            .with_context(|| format!("Could not find merge engine {}", path.display()))?;
        verify_version(&engine)?;
        return Ok(engine);
    }
    let root = default_engine_root()?;
    let engine = engine_binary(&root);
    let receipt: EngineReceipt = serde_json::from_slice(
        &fs::read(root.join(ENGINE_RECEIPT))
            .context("Pinned merge engine is not installed; run `rojo merge install-engine`")?,
    )?;
    let digest = output_digest(&engine)?.context("Pinned engine executable is missing")?;
    if receipt.revision != ENGINE_REVISION
        || receipt.version != ENGINE_VERSION
        || receipt.blake3 != digest.to_hex().as_str()
    {
        bail!("Pinned merge engine does not match its installation receipt; reinstall with `rojo merge install-engine`");
    }
    verify_version(&engine)?;
    Ok(engine)
}

fn install_engine(root: Option<&Path>) -> anyhow::Result<()> {
    let root = match root {
        Some(root) => std::path::absolute(root)?,
        None => default_engine_root()?,
    };
    fs::create_dir_all(&root)?;
    // Not `process`: this build can take minutes, and a child without its own
    // console must still receive Ctrl+C from the terminal that started it.
    let status = Command::new("cargo")
        .arg("install")
        .arg("--locked")
        .arg("--git")
        .arg(ENGINE_REPOSITORY)
        .arg("--rev")
        .arg(ENGINE_REVISION)
        .arg("--root")
        .arg(&root)
        .arg("--force")
        .arg("rbx_merge_cli")
        .status()
        .context("Could not run Cargo to install the pinned merge engine")?;
    if !status.success() {
        bail!("Pinned merge engine installation failed with {status}");
    }
    let engine = engine_binary(&root);
    verify_version(&engine)?;
    let receipt = EngineReceipt {
        revision: ENGINE_REVISION.into(),
        version: ENGINE_VERSION.into(),
        blake3: output_digest(&engine)?
            .context("Cargo did not install the merge executable")?
            .to_hex()
            .to_string(),
    };
    fs::write(
        root.join(ENGINE_RECEIPT),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    println!(
        "Installed pinned engine {ENGINE_REVISION} at {}",
        engine.display()
    );
    println!("Upstream backend is MIT; CLI is Apache-2.0. Source and licenses: {ENGINE_REPOSITORY}/tree/{ENGINE_REVISION}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_artifacts_preserve_existing_reports() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("conflicts.txt");
        fs::write(&report, "resolution = ours\n").unwrap();
        assert!(ensure_new_artifact(&report).is_err());
        assert_eq!(fs::read_to_string(report).unwrap(), "resolution = ours\n");
    }

    #[test]
    fn output_and_conflict_artifacts_must_not_contain_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.rbxm");
        assert!(paths_overlap(&out, &out.join("stash")).unwrap());
        assert!(paths_overlap(&out, dir.path()).unwrap());
        assert!(!paths_overlap(&out, &dir.path().join("new/stash")).unwrap());
    }

    #[test]
    fn engine_override_is_a_path_not_a_shell_command() {
        let options =
            MergeCommand::try_parse_from(["merge", "--engine", "a path; echo injected", "doctor"])
                .unwrap();
        assert_eq!(options.engine, Some(PathBuf::from("a path; echo injected")));
    }

    #[test]
    fn global_git_install_is_not_exposed() {
        assert!(MergeCommand::try_parse_from(["merge", "install", "--global"]).is_err());
    }

    #[test]
    fn output_at_a_filesystem_root_is_an_error() {
        let root = std::path::absolute("/").unwrap();
        let mut child = Command::new("rbx-merge-must-not-run");
        let error = publish_output(&mut child, &root, Path::new("merged.rbxm")).unwrap_err();
        assert!(error.to_string().contains("no parent"), "{error:#}");
    }

    #[test]
    fn output_that_is_a_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("rbx-merge-must-not-run");
        let error = publish_output(&mut child, dir.path(), Path::new("merged.rbxm")).unwrap_err();
        assert!(error.to_string().contains("is a directory"), "{error:#}");
    }

    #[test]
    fn validates_xml_and_rejects_empty_success_output() {
        assert!(validate_output(b"", Path::new("merged.rbxmx")).is_err());
        assert!(validate_output(
            b"<roblox version=\"4\"></roblox>",
            Path::new("merged.rbxmx")
        )
        .is_ok());
    }

    #[test]
    fn output_extensions_are_matched_without_regard_to_case() {
        let xml = b"<roblox version=\"4\"></roblox>";
        assert!(validate_output(xml, Path::new("MERGED.RBXLX")).is_ok());
        assert!(validate_output(xml, Path::new("merged.RbxMx")).is_ok());
        // Upper-case binary extensions select the binary decoder, which rejects XML.
        let error = validate_output(xml, Path::new("merged.RBXL")).unwrap_err();
        assert!(!error.to_string().contains("Use a .rbxm"), "{error:#}");
        assert!(validate_output(xml, Path::new("merged.txt")).is_err());
    }

    /// Stands in for the engine: copies a valid model to the path after `--out`.
    #[cfg(unix)]
    fn fake_engine(dir: &Path, model: &Path) -> Command {
        let script = dir.join("engine.sh");
        fs::write(
            &script,
            "#!/bin/sh
while [ \"$1\" != --out ]; do shift; done
cp \"$MODEL\" \"$2\"
",
        )
        .unwrap();
        let mut child = Command::new("sh");
        child.arg(script).env("MODEL", model);
        child
    }

    #[cfg(unix)]
    #[test]
    fn merged_output_gets_normal_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model.rbxmx");
        fs::write(&model, "<roblox version=\"4\"></roblox>").unwrap();

        let new_output = dir.path().join("new.rbxmx");
        publish_output(
            &mut fake_engine(dir.path(), &model),
            &new_output,
            &new_output,
        )
        .unwrap();
        let mode = fs::metadata(&new_output).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);

        let existing = dir.path().join("existing.rbxmx");
        fs::write(&existing, "old").unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o640)).unwrap();
        publish_output(&mut fake_engine(dir.path(), &model), &existing, &existing).unwrap();
        let mode = fs::metadata(&existing).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[test]
    fn staged_output_has_a_recognizable_name_and_is_removed_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("merged.rbxm");
        // Not an engine: whatever runs, it cannot produce a valid model.
        let mut child = Command::new(env::current_exe().unwrap());
        child
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let error = publish_output(&mut child, &out, &out).unwrap_err();
        assert!(error.to_string().contains("preserved"), "{error:#}");
        let staged = child
            .get_args()
            .skip_while(|arg| *arg != "--out")
            .nth(1)
            .map(PathBuf::from)
            .unwrap();
        let name = staged.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with(STAGED_PREFIX) && name.ends_with(".tmp"),
            "{name}"
        );
        assert!(!staged.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
