use std::{
    fs,
    path::{Path, PathBuf},
};

use clap::Parser;
use memofs::{InMemoryFs, Vfs, VfsSnapshot};
use roblox_install::RobloxStudio;

use crate::serve_session::ServeSession;

static PLUGIN_BINCODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugin.bincode"));
pub(super) static PLUGIN_FILE_NAME: &str = "RojoManagedPlugin.rbxm";

/// Install Rojo's plugin.
#[derive(Debug, Parser)]
pub struct PluginCommand {
    #[clap(subcommand)]
    subcommand: PluginSubcommand,
}

/// Manages Rojo's Roblox Studio plugin.
#[derive(Debug, Parser)]
pub enum PluginSubcommand {
    /// Install the plugin in Roblox Studio's plugins folder. If the plugin is
    /// already installed, installing it again will overwrite the current plugin
    /// file.
    Install {
        /// Write to this plugins folder instead of discovering Studio automatically.
        #[clap(long)]
        plugins_dir: Option<PathBuf>,
        /// Install Rojo-<version>.rbxm without replacing RojoManagedPlugin.rbxm.
        #[clap(long)]
        side_by_side: bool,
    },

    /// Removes the plugin if it is installed.
    Uninstall {
        /// Remove from this plugins folder instead of discovering Studio automatically.
        #[clap(long)]
        plugins_dir: Option<PathBuf>,
        /// Remove this version's Rojo-<version>.rbxm instead of RojoManagedPlugin.rbxm.
        #[clap(long, conflicts_with = "all")]
        side_by_side: bool,
        /// Remove RojoManagedPlugin.rbxm and every versioned Rojo-<version>.rbxm in the
        /// plugins folder, including copies that were not installed with --side-by-side.
        #[clap(long)]
        all: bool,
    },
}

impl PluginCommand {
    pub fn run(self) -> anyhow::Result<()> {
        self.subcommand.run()
    }
}

impl PluginSubcommand {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            PluginSubcommand::Install {
                plugins_dir,
                side_by_side,
            } => install_plugin(&resolve_plugins_dir(plugins_dir)?, side_by_side),
            PluginSubcommand::Uninstall {
                plugins_dir,
                side_by_side,
                all,
            } => {
                let plugins_dir = resolve_plugins_dir(plugins_dir)?;
                if all {
                    uninstall_all_plugins(&plugins_dir)
                } else if side_by_side {
                    remove_plugin_file(&plugins_dir.join(side_by_side_file_name()))
                } else {
                    uninstall_plugin(&plugins_dir)
                }
            }
        }
    }
}

fn resolve_plugins_dir(plugins_dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match plugins_dir {
        Some(path) => Ok(path),
        None => Ok(RobloxStudio::locate()?.plugins_path().to_path_buf()),
    }
}

fn initialize_plugin() -> anyhow::Result<ServeSession> {
    let plugin_snapshot: VfsSnapshot = bincode::deserialize(PLUGIN_BINCODE)
        .expect("Rojo's plugin was not properly packed into Rojo's binary");

    let mut in_memory_fs = InMemoryFs::new();
    in_memory_fs.load_snapshot("/plugin", plugin_snapshot)?;

    let vfs = Vfs::new(in_memory_fs);
    Ok(ServeSession::new(vfs, "/plugin")?)
}

pub(super) fn install_plugin(plugins_dir: &Path, side_by_side: bool) -> anyhow::Result<()> {
    let session = initialize_plugin()?;
    let tree = session.tree();
    fs::create_dir_all(plugins_dir)?;
    let destination = plugins_dir.join(if side_by_side {
        side_by_side_file_name()
    } else {
        PLUGIN_FILE_NAME.to_owned()
    });
    // Studio may see this directory while installing, so the old plugin is
    // only replaced once a complete model has been serialized and flushed.
    super::build::write_file_atomically(&destination, |writer| {
        rbx_binary::to_writer(writer, tree.inner(), &[tree.get_root_id()])?;
        Ok(())
    })?;
    println!("Installed plugin to {}", destination.display());
    Ok(())
}

/// The file name `install --side-by-side` uses for this version.
fn side_by_side_file_name() -> String {
    format!("Rojo-{}.rbxm", env!("CARGO_PKG_VERSION"))
}

/// Whether `name` is exactly a file name `install --side-by-side` creates for
/// some version, so other plugins that happen to start with "Rojo-" are kept.
pub(super) fn is_side_by_side_file_name(name: &str) -> bool {
    name.strip_prefix("Rojo-")
        .and_then(|rest| rest.strip_suffix(".rbxm"))
        .is_some_and(is_version)
}

/// Matches `MAJOR.MINOR.PATCH` with an optional `-prerelease` and `+build`
/// metadata, the shape of `CARGO_PKG_VERSION`. Build metadata must be accepted
/// here because install names the file after the full version string.
fn is_version(version: &str) -> bool {
    let (version, build) = match version.split_once('+') {
        Some((version, build)) => (version, Some(build)),
        None => (version, None),
    };
    let (release, prerelease) = match version.split_once('-') {
        Some((release, prerelease)) => (release, Some(prerelease)),
        None => (version, None),
    };
    let identifiers = |text: &str| {
        text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    };
    let numbers: Vec<_> = release.split('.').collect();
    numbers.len() == 3
        && numbers
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && prerelease.is_none_or(identifiers)
        && build.is_none_or(identifiers)
}

fn uninstall_plugin(plugins_dir: &Path) -> anyhow::Result<()> {
    remove_plugin_file(&plugins_dir.join(PLUGIN_FILE_NAME))
}

fn remove_plugin_file(plugin_path: &Path) -> anyhow::Result<()> {
    if plugin_path.exists() {
        log::debug!("Removing existing plugin from {}", plugin_path.display());
        fs::remove_file(plugin_path)?;
        println!("Removed plugin {}", plugin_path.display());
    } else {
        log::debug!("Plugin not installed at {}", plugin_path.display());
    }

    Ok(())
}

fn uninstall_all_plugins(plugins_dir: &Path) -> anyhow::Result<()> {
    uninstall_plugin(plugins_dir)?;
    let entries = match fs::read_dir(plugins_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let is_copy = entry
            .file_name()
            .to_str()
            .is_some_and(is_side_by_side_file_name);
        if is_copy && entry.file_type()?.is_file() {
            remove_plugin_file(&entry.path())?;
        }
    }
    Ok(())
}

#[test]
fn plugin_initialize() {
    let _ = initialize_plugin().unwrap();
}

#[test]
fn installs_and_replaces_a_complete_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join(PLUGIN_FILE_NAME);
    fs::write(&destination, b"old plugin").unwrap();
    install_plugin(dir.path(), false).unwrap();
    let bytes = fs::read(destination).unwrap();
    let dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();
    assert_eq!(dom.root().children().len(), 1);
    assert_eq!(
        dom.get_by_ref(dom.root().children()[0]).unwrap().name,
        "Rojo"
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn new_plugin_is_world_readable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    install_plugin(dir.path(), false).unwrap();
    let mode = fs::metadata(dir.path().join(PLUGIN_FILE_NAME))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o644);
}

#[cfg(unix)]
#[test]
fn reinstalled_plugin_keeps_existing_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join(PLUGIN_FILE_NAME);
    fs::write(&destination, b"old plugin").unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).unwrap();
    install_plugin(dir.path(), false).unwrap();
    let mode = fs::metadata(destination).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn uninstalls_from_an_explicit_plugins_dir() {
    let dir = tempfile::tempdir().unwrap();
    install_plugin(dir.path(), false).unwrap();
    uninstall_plugin(dir.path()).unwrap();
    assert!(!dir.path().join(PLUGIN_FILE_NAME).exists());

    // Uninstalling when nothing is installed is not an error.
    uninstall_plugin(dir.path()).unwrap();
}

#[test]
fn side_by_side_install_preserves_existing_managed_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join(PLUGIN_FILE_NAME);
    fs::write(&original, b"existing plugin").unwrap();
    install_plugin(dir.path(), true).unwrap();
    assert_eq!(fs::read(original).unwrap(), b"existing plugin");
    let bytes = fs::read(
        dir.path()
            .join(format!("Rojo-{}.rbxm", env!("CARGO_PKG_VERSION"))),
    )
    .unwrap();
    assert!(rbx_binary::from_reader(bytes.as_slice()).is_ok());
}

#[test]
fn side_by_side_uninstall_removes_only_this_version() {
    let dir = tempfile::tempdir().unwrap();
    install_plugin(dir.path(), false).unwrap();
    install_plugin(dir.path(), true).unwrap();
    let other_version = dir.path().join("Rojo-7.7.1.rbxm");
    fs::write(&other_version, b"other").unwrap();

    remove_plugin_file(&dir.path().join(side_by_side_file_name())).unwrap();
    assert!(!dir.path().join(side_by_side_file_name()).exists());
    assert!(dir.path().join(PLUGIN_FILE_NAME).exists());
    assert!(other_version.exists());
}

#[test]
fn uninstall_all_removes_only_files_rojo_created() {
    let dir = tempfile::tempdir().unwrap();
    install_plugin(dir.path(), false).unwrap();
    install_plugin(dir.path(), true).unwrap();
    for name in [
        "Rojo-7.7.1.rbxm",
        "Rojo-8.0.0-dev.1.rbxm",
        "Rojo-7.7.1+fork.1.rbxm",
        "Rojo-8.0.0-dev.4+build.7.rbxm",
    ] {
        fs::write(dir.path().join(name), b"copy").unwrap();
    }
    let unrelated = [
        "Rojo-Helper.rbxm",
        "Rojo-7.7.1.rbxmx",
        "Rojo-7.7.rbxm",
        "Rojo-7.7.1-.rbxm",
        "Rojo-7.7.1+.rbxm",
        "Rojo-7.7.1+a+b.rbxm",
        "MyRojo-7.7.1.rbxm",
        "Other.rbxm",
    ];
    for name in unrelated {
        fs::write(dir.path().join(name), b"keep").unwrap();
    }

    uninstall_all_plugins(dir.path()).unwrap();
    let mut remaining: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    remaining.sort();
    let mut expected: Vec<_> = unrelated.iter().map(|name| name.to_string()).collect();
    expected.sort();
    assert_eq!(remaining, expected);
}

#[test]
fn side_by_side_name_is_recognized_with_build_metadata() {
    // Whatever version this binary has, uninstall --all must find what install wrote.
    assert!(is_side_by_side_file_name(&side_by_side_file_name()));
    assert!(is_version("1.2.3+build.5"));
    assert!(is_version("1.2.3-rc.1+sha-abc"));
    assert!(!is_version("1.2.3+"));
    assert!(!is_version("1.2.3+a..b"));
}
