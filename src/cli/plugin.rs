use std::{
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use clap::Parser;
use memofs::{InMemoryFs, Vfs, VfsSnapshot};
use roblox_install::RobloxStudio;

use crate::serve_session::ServeSession;

static PLUGIN_BINCODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugin.bincode"));
static PLUGIN_FILE_NAME: &str = "RojoManagedPlugin.rbxm";

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
    },

    /// Removes the plugin if it is installed.
    Uninstall,
}

impl PluginCommand {
    pub fn run(self) -> anyhow::Result<()> {
        self.subcommand.run()
    }
}

impl PluginSubcommand {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            PluginSubcommand::Install { plugins_dir } => {
                let path = match plugins_dir {
                    Some(path) => path,
                    None => RobloxStudio::locate()?.plugins_path().to_path_buf(),
                };
                install_plugin(&path)
            }
            PluginSubcommand::Uninstall => uninstall_plugin(),
        }
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

fn install_plugin(plugins_dir: &Path) -> anyhow::Result<()> {
    let session = initialize_plugin()?;
    let tree = session.tree();
    fs::create_dir_all(plugins_dir)?;
    let destination = plugins_dir.join(PLUGIN_FILE_NAME);
    // Studio may see this directory while installing. Only replace the old
    // plugin after a complete model has been serialized and flushed.
    let mut staged = tempfile::NamedTempFile::new_in(plugins_dir)?;
    {
        let mut writer = BufWriter::new(staged.as_file_mut());
        rbx_binary::to_writer(&mut writer, tree.inner(), &[tree.get_root_id()])?;
        writer.flush()?;
    }
    staged.persist(&destination)?;
    println!("Installed plugin to {}", destination.display());
    Ok(())
}

fn uninstall_plugin() -> anyhow::Result<()> {
    let studio = RobloxStudio::locate()?;

    let plugin_path = studio.plugins_path().join(PLUGIN_FILE_NAME);

    if plugin_path.exists() {
        log::debug!("Removing existing plugin from {}", plugin_path.display());
        fs::remove_file(plugin_path)?;
    } else {
        log::debug!("Plugin not installed at {}", plugin_path.display());
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
    install_plugin(dir.path()).unwrap();
    let bytes = fs::read(destination).unwrap();
    let dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();
    assert_eq!(dom.root().children().len(), 1);
    assert_eq!(
        dom.get_by_ref(dom.root().children()[0]).unwrap().name,
        "Rojo"
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}
