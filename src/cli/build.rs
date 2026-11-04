use std::{
    io::{BufWriter, Write},
    mem::forget,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context};
use clap::{CommandFactory, Parser, ValueEnum};
use memofs::Vfs;
use roblox_install::RobloxStudio;
use tokio::runtime::Runtime;

use crate::serve_session::ServeSession;

use super::resolve_path;

const UNKNOWN_OUTPUT_KIND_ERR: &str = "Could not detect what kind of file to build. \
                                       Expected output file to end in .rbxl, .rbxlx, .rbxm, or .rbxmx.";
const UNKNOWN_PLUGIN_KIND_ERR: &str = "Could not detect what kind of file to build. \
                                       Expected plugin file to end in .rbxm or .rbxmx.";

/// Generates a model or place file from the Rojo project.
#[derive(Debug, Parser)]
pub struct BuildCommand {
    /// Path to the project to build. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Where to output the result.
    ///
    /// Should end in .rbxm, .rbxl, .rbxmx, or .rbxlx.
    #[clap(long, short, conflicts_with = "plugin")]
    pub output: Option<PathBuf>,

    /// Alternative to the output flag that outputs the result in the local plugins folder.
    ///
    /// Should end in .rbxm or .rbxl.
    #[clap(long, short, conflicts_with = "output")]
    pub plugin: Option<PathBuf>,

    /// Compression for binary models and places. Defaults to lz4.
    #[clap(long, value_enum)]
    pub compress: Option<Compression>,

    /// Whether to automatically rebuild when any input files change.
    #[clap(long)]
    pub watch: bool,
}

impl BuildCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let (output_path, output_kind) = match (self.output, self.plugin) {
            (None, None) => {
                BuildCommand::command()
                    .error(
                        clap::ErrorKind::MissingRequiredArgument,
                        "one of the following arguments must be provided: \n    --output <OUTPUT>\n    --plugin <PLUGIN>",
                    )
                    .exit();
            }
            (Some(output), None) => {
                let output_kind =
                    OutputKind::from_output_path(&output).context(UNKNOWN_OUTPUT_KIND_ERR)?;

                (output, output_kind)
            }
            (None, Some(plugin)) => {
                if plugin.is_absolute() {
                    bail!("plugin flag path cannot be absolute.")
                }

                let output_kind =
                    OutputKind::from_plugin_path(&plugin).context(UNKNOWN_PLUGIN_KIND_ERR)?;
                let studio = RobloxStudio::locate()?;

                (studio.plugins_path().join(&plugin), output_kind)
            }
            _ => unreachable!(),
        };

        if self.compress.is_some() && matches!(output_kind, OutputKind::Rbxlx | OutputKind::Rbxmx) {
            bail!("--compress requires a binary output (.rbxl or .rbxm)");
        }
        let project_path = resolve_path(&self.project)?;

        log::trace!("Constructing in-memory filesystem");
        let vfs = if self.watch {
            Vfs::new_default()?
        } else {
            Vfs::new_default_without_watch()
        };

        let session = ServeSession::new(vfs, project_path)?;
        let mut cursor = session.message_queue().cursor();

        write_model(&session, &output_path, output_kind, self.compress)?;

        if self.watch {
            let rt = Runtime::new().context("Failed to start the async runtime for watch mode")?;

            loop {
                let receiver = session.message_queue().subscribe(cursor);
                let (new_cursor, _patch_set) = match rt.block_on(receiver) {
                    Ok(message) => message,
                    // The message queue was dropped, so there is nothing left
                    // to watch. Stop watching gracefully.
                    Err(_) => break,
                };
                cursor = new_cursor;

                // A rebuild can fail because of an in-progress edit (for
                // example, the root's class temporarily isn't a DataModel).
                // Keep watching so that the next change can fix the build;
                // write_model validates before touching the output, so the
                // last good build stays in place.
                if let Err(error) = write_model(&session, &output_path, output_kind, self.compress)
                {
                    log::error!("Failed to rebuild project: {error:?}");
                }
            }
        }

        // Avoid dropping ServeSession: it's potentially VERY expensive to drop
        // and we're about to exit anyways.
        forget(session);

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Compression {
    Lz4,
    Zstd,
    None,
}

impl Compression {
    fn serializer(self) -> rbx_binary::Serializer<'static> {
        rbx_binary::Serializer::new().compression_type(match self {
            Self::Lz4 => rbx_binary::CompressionType::Lz4,
            Self::Zstd => rbx_binary::CompressionType::Zstd,
            Self::None => rbx_binary::CompressionType::None,
        })
    }
}

/// The different kinds of output that Rojo can build to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    /// An XML model file.
    Rbxmx,

    /// An XML place file.
    Rbxlx,

    /// A binary model file.
    Rbxm,

    /// A binary place file.
    Rbxl,
}

impl OutputKind {
    fn from_output_path(output: &Path) -> Option<OutputKind> {
        let extension = output.extension()?.to_str()?;

        match extension {
            "rbxlx" => Some(OutputKind::Rbxlx),
            "rbxmx" => Some(OutputKind::Rbxmx),
            "rbxl" => Some(OutputKind::Rbxl),
            "rbxm" => Some(OutputKind::Rbxm),
            _ => None,
        }
    }

    fn from_plugin_path(output: &Path) -> Option<OutputKind> {
        let extension = output.extension()?.to_str()?;

        match extension {
            "rbxmx" => Some(OutputKind::Rbxmx),
            "rbxm" => Some(OutputKind::Rbxm),
            _ => None,
        }
    }
}

fn xml_encode_config() -> rbx_xml::EncodeOptions<'static> {
    rbx_xml::EncodeOptions::new().property_behavior(rbx_xml::EncodePropertyBehavior::WriteUnknown)
}

#[profiling::function]
fn write_model(
    session: &ServeSession,
    output: &Path,
    output_kind: OutputKind,
    compression: Option<Compression>,
) -> anyhow::Result<()> {
    println!("Building project '{}'", session.project_name());

    let tree = session.tree();
    let root_id = tree.get_root_id();

    let class = tree.get_instance(root_id).unwrap().class_name();
    if matches!(output_kind, OutputKind::Rbxl | OutputKind::Rbxlx) && class != "DataModel" {
        bail!("Cannot build a {} root as a place. Use .rbxm or .rbxmx, or set the root class to DataModel.", class);
    }
    log::trace!("Serializing to a staged output file");
    write_file_atomically(output, |file| {
        match output_kind {
            OutputKind::Rbxm => {
                compression
                    .unwrap_or(Compression::Lz4)
                    .serializer()
                    .serialize(file, tree.inner(), &[root_id])?;
            }
            OutputKind::Rbxl => {
                let root_instance = tree.get_instance(root_id).unwrap();
                let top_level_ids = root_instance.children();

                compression
                    .unwrap_or(Compression::Lz4)
                    .serializer()
                    .serialize(file, tree.inner(), top_level_ids)?;
            }
            OutputKind::Rbxmx => {
                // Model files include the root instance of the tree and all its
                // descendants.

                rbx_xml::to_writer(file, tree.inner(), &[root_id], xml_encode_config())?;
            }
            OutputKind::Rbxlx => {
                // Place files don't contain an entry for the DataModel, but our
                // WeakDom representation does.

                let root_instance = tree.get_instance(root_id).unwrap();
                let top_level_ids = root_instance.children();

                rbx_xml::to_writer(file, tree.inner(), top_level_ids, xml_encode_config())?;
            }
        }
        Ok(())
    })?;

    let filename = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<invalid utf-8>");
    println!("Built project to {}", filename);

    Ok(())
}

/// Replaces `destination` with the bytes produced by `write`, leaving the
/// previous file untouched if writing fails.
///
/// The output is staged in a temporary file next to `destination` and renamed
/// over it once complete. Truncating the destination first would leave a
/// corrupt file behind whenever serialization fails, and Studio may load a
/// plugin or place file while it is half written.
pub(super) fn write_file_atomically(
    destination: &Path,
    write: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // The staged file must be on the same filesystem for the rename to be
    // atomic, so it goes in the destination's directory.
    let parent = match destination.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut staged = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "Could not create a temporary file in {} to write {}",
            parent.display(),
            destination.display()
        )
    })?;

    {
        let mut writer = BufWriter::new(staged.as_file_mut());
        write(&mut writer)?;
        writer.flush()?;
    }

    // Temporary files are created owner-only (0600) on Unix. Keep the
    // permissions of the file being replaced so a rebuild doesn't change who
    // can read it; a new file gets 0644, the usual mode for a file created
    // under the default umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = match std::fs::metadata(destination) {
            Ok(metadata) => metadata.permissions(),
            Err(_) => std::fs::Permissions::from_mode(0o644),
        };
        staged.as_file().set_permissions(permissions)?;
    }

    // Dropping the staged file on error deletes it, so a failed rename leaves
    // only the previous output behind.
    staged.persist(destination).map_err(|error| {
        anyhow!(
            "Could not replace {}: {}. The previous file was left unchanged; \
             if another program such as Roblox Studio has it open, close it and try again.",
            destination.display(),
            error.error
        )
    })?;

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn failed_write_keeps_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("place.rbxl");
        std::fs::write(&destination, b"previous build").unwrap();

        let result = write_file_atomically(&destination, |file| {
            file.write_all(b"partial")?;
            bail!("serialization failed")
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous build");
        // The staged file is cleaned up rather than left next to the output.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn successful_write_replaces_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("place.rbxl");
        std::fs::write(&destination, b"previous build").unwrap();

        write_file_atomically(&destination, |file| Ok(file.write_all(b"new build")?)).unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"new build");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn rebuild_keeps_permissions_of_previous_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("place.rbxl");
        std::fs::write(&destination, b"previous build").unwrap();
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o640)).unwrap();

        write_file_atomically(&destination, |file| Ok(file.write_all(b"new build")?)).unwrap();

        let mode = std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
    }
}
