//! Defines Rojo's CLI through clap types.

mod assets;
mod build;
mod doc;
mod doctor;
mod fmt_project;
mod init;
mod lifecycle;
mod mcp;
mod merge;
mod packages;
mod plugin;
mod serve;
mod sourcemap;
mod studio;
mod studio_lock;
mod syncback;
mod test;
mod typecheck;
mod upload;

use std::{
    borrow::Cow,
    env,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::Context;
use clap::Parser;
use thiserror::Error;

pub use self::assets::AssetsCommand;
pub use self::build::BuildCommand;
pub use self::doc::DocCommand;
pub use self::doctor::DoctorCommand;
pub use self::fmt_project::FmtProjectCommand;
pub use self::init::{InitCommand, InitKind};
pub use self::lifecycle::{CheckpointCommand, ReleaseCommand, WorkspaceCommand};
pub use self::mcp::McpCommand;
pub use self::merge::MergeCommand;
pub use self::packages::PackagesCommand;
pub use self::plugin::{PluginCommand, PluginSubcommand};
pub use self::serve::ServeCommand;
pub use self::sourcemap::SourcemapCommand;
pub(crate) use self::studio::backup as studio_backup;
pub use self::studio::{CheckUntrustedModelCommand, StudioCommand};
pub use self::syncback::SyncbackCommand;
pub use self::test::TestCommand;
pub use self::typecheck::TypecheckCommand;
pub use self::upload::UploadCommand;

/// Command line options that Rojo accepts, defined using the clap crate.
#[derive(Debug, Parser)]
#[clap(name = "Rojo", version, about)]
pub struct Options {
    #[clap(flatten)]
    pub global: GlobalOptions,

    /// Subcommand to run in this invocation.
    #[clap(subcommand)]
    pub subcommand: Subcommand,
}

impl Options {
    pub fn run(self) -> anyhow::Result<()> {
        match self.subcommand {
            Subcommand::Init(subcommand) => subcommand.run(),
            Subcommand::Serve(subcommand) => subcommand.run(self.global),
            Subcommand::Build(subcommand) => subcommand.run(),
            Subcommand::Upload(subcommand) => subcommand.run(),
            Subcommand::Sourcemap(subcommand) => subcommand.run(),
            Subcommand::Typecheck(subcommand) => subcommand.run(),
            Subcommand::FmtProject(subcommand) => subcommand.run(),
            Subcommand::Doc(subcommand) => subcommand.run(),
            Subcommand::Doctor(subcommand) => subcommand.run(),
            Subcommand::Plugin(subcommand) => subcommand.run(),
            Subcommand::Syncback(subcommand) => subcommand.run(self.global),
            Subcommand::Checkpoint(subcommand) => subcommand.run(),
            Subcommand::Workspace(subcommand) => subcommand.run(),
            Subcommand::Release(subcommand) => subcommand.run(),
            Subcommand::Merge(subcommand) => subcommand.run(),
            Subcommand::Packages(subcommand) => subcommand.run(),
            Subcommand::Studio(subcommand) => subcommand.run(),
            Subcommand::Test(subcommand) => subcommand.run(),
            Subcommand::Mcp(subcommand) => subcommand.run(),
            Subcommand::Assets(subcommand) => subcommand.run(),
            Subcommand::CheckUntrustedModel(subcommand) => subcommand.run(),
        }
    }
}

#[derive(Debug, Parser)]
pub struct GlobalOptions {
    /// Sets verbosity level. Can be specified multiple times.
    #[clap(long("verbose"), short, global(true), parse(from_occurrences))]
    pub verbosity: u8,

    /// Set color behavior. Valid values are auto, always, and never.
    #[clap(long("color"), global(true), default_value("auto"))]
    pub color: ColorChoice,
}

#[derive(Debug, Clone, Copy)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl FromStr for ColorChoice {
    type Err = ColorChoiceParseError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        match source {
            "auto" => Ok(ColorChoice::Auto),
            "always" => Ok(ColorChoice::Always),
            "never" => Ok(ColorChoice::Never),
            _ => Err(ColorChoiceParseError {
                attempted: source.to_owned(),
            }),
        }
    }
}

impl From<ColorChoice> for termcolor::ColorChoice {
    fn from(value: ColorChoice) -> Self {
        match value {
            ColorChoice::Auto => termcolor::ColorChoice::Auto,
            ColorChoice::Always => termcolor::ColorChoice::Always,
            ColorChoice::Never => termcolor::ColorChoice::Never,
        }
    }
}

impl From<ColorChoice> for env_logger::WriteStyle {
    fn from(value: ColorChoice) -> Self {
        match value {
            ColorChoice::Auto => env_logger::WriteStyle::Auto,
            ColorChoice::Always => env_logger::WriteStyle::Always,
            ColorChoice::Never => env_logger::WriteStyle::Never,
        }
    }
}

#[derive(Debug, Error)]
#[error("Invalid color choice '{attempted}'. Valid values are: auto, always, never")]
pub struct ColorChoiceParseError {
    attempted: String,
}

#[derive(Debug, Parser)]
pub enum Subcommand {
    Init(InitCommand),
    Serve(ServeCommand),
    Build(BuildCommand),
    Upload(UploadCommand),
    Sourcemap(SourcemapCommand),
    Typecheck(TypecheckCommand),
    FmtProject(FmtProjectCommand),
    Doc(DocCommand),
    Doctor(DoctorCommand),
    Plugin(PluginCommand),
    Syncback(SyncbackCommand),
    Checkpoint(CheckpointCommand),
    Workspace(WorkspaceCommand),
    Release(ReleaseCommand),
    Merge(MergeCommand),
    Packages(PackagesCommand),
    Studio(StudioCommand),
    Test(TestCommand),
    Mcp(McpCommand),
    Assets(AssetsCommand),
    /// Internal: checks one untrusted model read from stdin. `rojo serve` and
    /// `rojo studio import` run it as a child process.
    // Must match `model_check::SUBCOMMAND`; a test checks that it does.
    #[clap(name = "check-untrusted-model", hide = true)]
    CheckUntrustedModel(CheckUntrustedModelCommand),
}

/// A command was used incorrectly or could not reach what it needs, as opposed
/// to its work failing. The process exits with status 2 instead of 1.
#[derive(Debug, Error)]
#[error("{0:#}")]
pub struct UsageError(pub anyhow::Error);

/// The command already reported its outcome; exit with this status without
/// logging an error.
#[derive(Debug, Error)]
#[error("exit status {0}")]
pub struct ExitStatus(pub i32);

/// The process exit status for an error returned by [`Options::run`], and
/// whether the error still needs to be logged.
pub fn exit_status(error: &anyhow::Error) -> (i32, bool) {
    if let Some(ExitStatus(code)) = error.downcast_ref::<ExitStatus>() {
        (*code, false)
    } else if error.is::<UsageError>() {
        (2, true)
    } else {
        (1, true)
    }
}

/// A per-user directory for state about the project in `workdir`, for when
/// it is not in a Git repository: `%LOCALAPPDATA%\Rojo\projects\<id>` on
/// Windows, `$XDG_STATE_HOME/rojo/projects/<id>` (or `~/.local/state/...`)
/// elsewhere. It must not be inside the project, because `rojo serve` would
/// sync any JSON or model file placed in a mapped folder into the place.
/// Rojo's per-user state for the repository whose Git common directory is
/// `common_dir`, shared by all of its worktrees. It is outside the repository,
/// because a project can map a folder that contains `.git`, and `rojo serve`
/// would then sync anything stored there into the place.
pub(crate) fn repository_state_dir(common_dir: &Path) -> anyhow::Result<PathBuf> {
    project_state_dir(common_dir)
}

pub(crate) fn project_state_dir(workdir: &Path) -> anyhow::Result<PathBuf> {
    let base = if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Rojo"))
    } else {
        env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .map(|dir| dir.join("rojo"))
    }
    .context("Could not find a per-user directory for Rojo's state (LOCALAPPDATA, XDG_STATE_HOME or HOME)")?;
    let canonical = dunce::canonicalize(workdir).unwrap_or_else(|_| workdir.to_owned());
    let id = blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex();
    Ok(base.join("projects").join(&id[..16]))
}

/// Whether `start` or one of its parents looks like a repository: a `.git`
/// folder with a `HEAD` file, or a `.git` file pointing at one (`gitdir:`),
/// as Git itself requires. This tells that `start` is in a repository even
/// when Git cannot run, while a stray `.git` folder, such as an empty one in a
/// home directory, does not count.
pub(crate) fn has_git_marker(start: &Path) -> bool {
    start.ancestors().any(|dir| {
        let marker = dir.join(".git");
        if marker.is_dir() {
            marker.join("HEAD").is_file()
        } else {
            std::fs::read(&marker).is_ok_and(|contents| contents.starts_with(b"gitdir:"))
        }
    })
}

pub(super) fn resolve_path(path: &Path) -> anyhow::Result<Cow<'_, Path>> {
    if path.is_absolute() {
        Ok(Cow::Borrowed(path))
    } else {
        let current_dir = env::current_dir().context(
            "Could not determine the current working directory. \
             It may have been deleted, or Rojo may not have permission to access it.",
        )?;
        Ok(Cow::Owned(current_dir.join(path)))
    }
}
