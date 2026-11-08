use std::{
    io::{BufWriter, Write},
    mem::forget,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{anyhow, bail, Context};
use clap::{CommandFactory, Parser, ValueEnum};
use memofs::Vfs;
use roblox_install::RobloxStudio;
use tokio::runtime::Runtime;

use crate::serve_session::ServeSession;

use super::{build_into, resolve_path};

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

    /// Apply the project onto this existing place (.rbxl or .rbxlx) instead
    /// of building a new one.
    ///
    /// Instances, properties and services the project does not define keep
    /// what the base place has, such as the map, terrain and lighting. The
    /// output cannot be the base place itself. See docs/build-into.md.
    #[clap(long, value_name = "BASE", requires = "output")]
    pub into: Option<PathBuf>,
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
        if let Some(base) = &self.into {
            if !matches!(output_kind, OutputKind::Rbxl | OutputKind::Rbxlx) {
                bail!("--into builds a place, so the output must end in .rbxl or .rbxlx");
            }
            // Studio can save the base while it is being built into, and
            // nothing stops it from doing so between Rojo's last look at the
            // file and the replace, so that save would be lost.
            if is_same_path(base, &output_path) {
                bail!("{}", same_file_error(base, &output_path));
            }
        }
        let into = self.into.as_deref();
        let project_path = resolve_path(&self.project)?;

        log::trace!("Constructing in-memory filesystem");
        let vfs = if self.watch {
            Vfs::new_default()?
        } else {
            Vfs::new_default_without_watch()
        };

        let session = ServeSession::new(vfs, project_path)?;
        let mut cursor = session.message_queue().cursor();

        // Taken before the first build reads the base, so a base saved while
        // that build runs still triggers a rebuild.
        let mut base_state = into.map(BaseState::read);

        let initial_build = write_model(&session, &output_path, output_kind, self.compress, into);

        if self.watch {
            // The first build can fail for the same transient reasons as a
            // rebuild, so watch mode reports it and waits for the next change
            // instead of exiting.
            if let Err(error) = &initial_build {
                log::error!("Failed to build project: {error:?}");
            }

            let rt = Runtime::new().context("Failed to start the async runtime for watch mode")?;

            loop {
                // A rebuild always reads the whole tree, so missing trimmed
                // history only means rebuilding from the current state now.
                let mut changed = match session.message_queue().subscribe(cursor) {
                    Ok(receiver) => match &into {
                        // The base place is outside the project, so the
                        // project's watcher never reports it. Its file is
                        // polled instead, waking up between project changes.
                        Some(_) => {
                            match rt.block_on(async {
                                tokio::time::timeout(BASE_POLL_INTERVAL, receiver).await
                            }) {
                                Ok(Ok((new_cursor, _patch_set))) => {
                                    cursor = new_cursor;
                                    true
                                }
                                Ok(Err(_)) => break,
                                Err(_elapsed) => false,
                            }
                        }
                        None => match rt.block_on(receiver) {
                            Ok((new_cursor, _patch_set)) => {
                                cursor = new_cursor;
                                true
                            }
                            // The message queue was dropped, so there is
                            // nothing left to watch. Stop watching gracefully.
                            Err(_) => break,
                        },
                    },
                    Err(_) => {
                        cursor = session.message_queue().cursor();
                        true
                    }
                };

                // A base that was missing, unreadable or mid-save failed the
                // last build; saving it again changes its state and retries.
                if let Some(base) = into {
                    let current = BaseState::read(base);
                    if base_state.as_ref() != Some(&current) {
                        base_state = Some(current);
                        changed = true;
                    }
                }
                if !changed {
                    continue;
                }

                // A link created since startup can make the output the base
                // after all. That will not change back on its own, so watching
                // stops instead of failing every rebuild.
                if let Some(base) = into {
                    if is_same_path(base, &output_path) {
                        bail!("{}", same_file_error(base, &output_path));
                    }
                }

                // A rebuild can fail because of an in-progress edit (for
                // example, the root's class temporarily isn't a DataModel).
                // Keep watching so that the next change can fix the build;
                // write_model validates before touching the output, so the
                // last good build stays in place.
                if let Err(error) =
                    write_model(&session, &output_path, output_kind, self.compress, into)
                {
                    log::error!("Failed to rebuild project: {error:?}");
                }
            }
        }

        // Avoid dropping ServeSession: it's potentially VERY expensive to drop
        // and we're about to exit anyways.
        forget(session);

        if self.watch {
            Ok(())
        } else {
            initial_build
        }
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
    into: Option<&Path>,
) -> anyhow::Result<()> {
    println!("Building project '{}'", session.project_name());

    let project_tree = session.tree();
    let root_id = project_tree.get_root_id();

    let class = project_tree.get_instance(root_id).unwrap().class_name();
    if matches!(output_kind, OutputKind::Rbxl | OutputKind::Rbxlx) && class != "DataModel" {
        bail!("Cannot build a {} root as a place. Use .rbxm or .rbxmx, or set the root class to DataModel.", class);
    }

    // Resolved once: the base check and the write both use this path, so a
    // link repointed in between cannot send the write somewhere unchecked.
    let destination = resolve_output(output)
        .with_context(|| format!("Could not resolve symlink {}", output.display()))?;

    // The base is read on every build so a watched build picks up a newly
    // saved base place, and the merge never mutates the session's own tree.
    let merged;
    let tree = match into {
        Some(base) => {
            // Checked here as well as at startup, so that a link created
            // while watching cannot make a rebuild replace the base.
            if is_same_path(base, &destination) {
                bail!("{}", same_file_error(base, output));
            }
            let base_dom = build_into::read_base_place(base)?;
            merged = build_into::merge_into_place(&project_tree, base_dom)?;
            &merged
        }
        None => &*project_tree,
    };
    let root_id = tree.get_root_id();

    if matches!(output_kind, OutputKind::Rbxmx | OutputKind::Rbxlx) {
        ensure_xml_can_store(tree.inner(), into.is_some())?;
    }

    log::trace!("Serializing to a staged output file");
    write_resolved_file_atomically(&destination, |file| {
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

fn same_file_error(base: &Path, output: &Path) -> String {
    format!(
        "--into cannot write over its own base place ({} is {}). Studio could save \
         the base while it is being replaced, and that save would be lost. Write to a \
         different --output, such as {}, and open that instead.",
        output.display(),
        base.display(),
        suggested_output(base).display()
    )
}

/// `place.rbxl` becomes `place.built.rbxl`, next to the base.
fn suggested_output(base: &Path) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "place".to_owned());
    let extension = base
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rbxl".to_owned());
    base.with_file_name(format!("{stem}.built.{extension}"))
}

/// Fails, instead of letting rbx_xml panic, when an instance has a `Content`
/// value XML files cannot store yet, such as one that refers to an object.
///
/// Builds abort on panic, so a panic would end `--watch` too. Such values
/// come from a binary place used as the `--into` base; project files cannot
/// produce them.
fn ensure_xml_can_store(dom: &rbx_dom_weak::WeakDom, has_base: bool) -> anyhow::Result<()> {
    use rbx_dom_weak::types::{ContentType, Variant};

    for instance in dom.descendants() {
        for (name, value) in &instance.properties {
            let Variant::Content(content) = value else {
                continue;
            };
            if matches!(content.value(), ContentType::None | ContentType::Uri(_)) {
                continue;
            }
            let mut path = Vec::new();
            let mut current = instance;
            while current.referent() != dom.root_ref() {
                path.push(current.name.as_str());
                current = dom.get_by_ref(current.parent()).unwrap();
            }
            path.reverse();
            bail!(
                "{}{} has {} set to an object rather than a URI, which .rbxlx and .rbxmx \
                 files cannot store. Write a .rbxl output instead.",
                if has_base { "The base place's " } else { "" },
                path.join("/"),
                name
            );
        }
    }
    Ok(())
}

/// How often `--watch --into` checks the base place for changes.
const BASE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// What `--watch --into` compares to notice that the base place changed.
#[derive(Debug, PartialEq, Eq)]
struct BaseState(Option<(Option<SystemTime>, u64)>);

impl BaseState {
    fn read(path: &Path) -> Self {
        Self(
            std::fs::metadata(path)
                .ok()
                .map(|metadata| (metadata.modified().ok(), metadata.len())),
        )
    }
}

/// Whether `left` and `right` name the same file, following links. Neither
/// needs to exist: a missing file is compared by its canonical directory and
/// its name, ignoring case on Windows, whose file systems do too.
fn is_same_path(left: &Path, right: &Path) -> bool {
    match (comparable_path(left), comparable_path(right)) {
        (Some(left), Some(right)) => {
            if cfg!(windows) {
                left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
            } else {
                left == right
            }
        }
        _ => false,
    }
}

fn comparable_path(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = dunce::canonicalize(path) {
        return Some(canonical);
    }
    let absolute = std::path::absolute(path).ok()?;
    let name = absolute.file_name()?;
    let parent = absolute.parent()?;
    match dunce::canonicalize(parent) {
        Ok(parent) => Some(parent.join(name)),
        Err(_) => Some(absolute),
    }
}

/// Replaces `destination` with the bytes produced by `write`, leaving the
/// previous file untouched if writing fails.
///
/// The output is staged in a temporary file next to `destination` and renamed
/// over it once complete. Truncating the destination first would leave a
/// corrupt file behind whenever serialization fails, and Studio may load a
/// plugin or place file while it is half written.
///
/// If `destination` is a symlink, the file it points to is replaced instead,
/// so the link keeps working. Hard links to the previous file are not
/// updated, since the new contents are a different file.
pub(super) fn write_file_atomically(
    destination: &Path,
    write: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // Renaming over a symlink would replace the link itself with a plain
    // file, and whatever it pointed to would silently stop being updated.
    let destination = resolve_output(destination)
        .with_context(|| format!("Could not resolve symlink {}", destination.display()))?;
    write_resolved_file_atomically(&destination, write)
}

/// [`write_file_atomically`] to a destination [`resolve_output`] already
/// resolved, which is used exactly as given: links are not followed again.
fn write_resolved_file_atomically(
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

    // Without this, a crash shortly after the rename can leave an empty or
    // partial file in place of the output on filesystems that reorder the
    // rename before the data reaches the disk.
    staged.as_file().sync_all().with_context(|| {
        format!(
            "Could not flush the staged output for {}",
            destination.display()
        )
    })?;

    // Dropping the staged file on error deletes it, so a failed rename leaves
    // only the previous output behind. On Windows, another program reading
    // the output at that moment makes the rename fail briefly, and a watched
    // build would then keep the old output until the next change, so it is
    // tried a few times.
    let mut attempt = 0;
    loop {
        match staged.persist(destination) {
            Ok(_) => break,
            Err(error) if attempt < 5 => {
                attempt += 1;
                staged = error.file;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                return Err(anyhow!(
                    "Could not replace {}: {}. The previous file was left unchanged; \
                     if another program such as Roblox Studio has it open, close it and try again.",
                    destination.display(),
                    error.error
                ))
            }
        }
    }

    // Persist the rename itself. This is best-effort: some filesystems do not
    // support syncing directories, and the output is already complete.
    #[cfg(unix)]
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }

    Ok(())
}

/// The file a write to `path` ends up in: `path` followed through any chain
/// of symlinks, in its folder with that folder's links resolved too, so a
/// link to the folder repointed later does not move the write.
fn resolve_output(path: &Path) -> std::io::Result<PathBuf> {
    let resolved = resolve_symlinks(path)?;
    let (Some(parent), Some(name)) = (resolved.parent(), resolved.file_name()) else {
        return Ok(resolved);
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    Ok(match dunce::canonicalize(parent) {
        Ok(parent) => parent.join(name),
        Err(_) => resolved,
    })
}

/// Follows `path` through any chain of symlinks to the path that would be
/// opened for writing. Unlike `fs::canonicalize`, the final target does not
/// need to exist, and other components of the path are left as they are.
fn resolve_symlinks(path: &Path) -> std::io::Result<PathBuf> {
    // Matches the usual limit on symlink hops, so a loop is reported rather
    // than followed forever.
    const MAX_HOPS: usize = 40;

    let mut path = path.to_path_buf();
    for _ in 0..MAX_HOPS {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = std::fs::read_link(&path)?;
                path = match path.parent() {
                    Some(parent) if target.is_relative() => parent.join(target),
                    _ => target,
                };
            }
            _ => return Ok(path),
        }
    }

    Err(std::io::Error::other(format!(
        "too many levels of symbolic links at {}",
        path.display()
    )))
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
    fn suggested_output_sits_next_to_the_base() {
        assert_eq!(
            suggested_output(Path::new("maps/place.rbxl")),
            Path::new("maps/place.built.rbxl")
        );
        assert_eq!(
            suggested_output(Path::new("place.rbxlx")),
            Path::new("place.built.rbxlx")
        );
    }

    #[test]
    fn same_path_does_not_need_the_files_to_exist() {
        let dir = tempfile::tempdir().unwrap();
        let place = dir.path().join("place.rbxl");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert!(is_same_path(&place, &place));
        assert!(is_same_path(
            &place,
            &dir.path().join("sub").join("..").join("place.rbxl")
        ));
        assert!(!is_same_path(&place, &dir.path().join("other.rbxl")));
        #[cfg(windows)]
        assert!(is_same_path(&place, &dir.path().join("PLACE.rbxl")));
    }

    /// rbx_xml panics on object-backed `Content`, and builds abort on panic,
    /// so the build has to catch them first.
    #[test]
    fn xml_output_refuses_object_content() {
        use rbx_dom_weak::{
            types::{Content, Variant},
            InstanceBuilder, WeakDom,
        };

        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
        let sound = dom.insert(
            workspace,
            InstanceBuilder::new("Sound")
                .with_name("Music")
                .with_property("AudioContent", Content::from_uri("rbxassetid://1")),
        );
        assert!(ensure_xml_can_store(&dom, true).is_ok());

        dom.get_by_ref_mut(sound).unwrap().properties.insert(
            "AudioContent".into(),
            Variant::Content(Content::from_referent(workspace)),
        );
        let error = ensure_xml_can_store(&dom, true).unwrap_err().to_string();
        assert!(error.contains("Workspace/Music"), "{error}");
        assert!(error.contains("AudioContent"), "{error}");
        assert!(error.contains(".rbxl output"), "{error}");
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

    /// Creates a file symlink, or returns false where that needs privileges
    /// the test does not have (Windows without Developer Mode).
    fn symlink_file(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).unwrap();
            true
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }

    /// The writer publishes to the path it is given and does not follow it
    /// again, so what was checked is what is written.
    #[test]
    fn a_resolved_destination_is_not_followed_again() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.rbxl");
        let link = dir.path().join("link.rbxl");
        std::fs::write(&real, b"previous build").unwrap();
        if !symlink_file(Path::new("real.rbxl"), &link) {
            eprintln!("skipping: cannot create symlinks here");
            return;
        }
        assert_eq!(
            resolve_output(&link).unwrap(),
            dunce::canonicalize(&real).unwrap()
        );

        // Given the link itself as the resolved path, it replaces the link.
        write_resolved_file_atomically(&link, |file| Ok(file.write_all(b"new build")?)).unwrap();
        assert!(!std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"previous build");
    }

    /// A link repointed at the base after the output was resolved and
    /// checked does not make the build write over the base.
    #[cfg(unix)]
    #[test]
    fn a_link_repointed_after_resolving_does_not_redirect_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.rbxl");
        let output = dir.path().join("out.rbxl");
        let link = dir.path().join("link.rbxl");
        std::fs::write(&base, b"base").unwrap();
        std::fs::write(&output, b"previous build").unwrap();
        std::os::unix::fs::symlink(&output, &link).unwrap();

        let destination = resolve_output(&link).unwrap();
        assert!(!is_same_path(&base, &destination));
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&base, &link).unwrap();

        write_resolved_file_atomically(&destination, |file| Ok(file.write_all(b"merged")?))
            .unwrap();
        assert_eq!(std::fs::read(&base).unwrap(), b"base");
        assert_eq!(std::fs::read(&output).unwrap(), b"merged");
    }

    #[test]
    fn write_through_symlink_replaces_link_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.rbxl");
        let link = dir.path().join("link.rbxl");
        std::fs::write(&real, b"previous build").unwrap();
        if !symlink_file(Path::new("real.rbxl"), &link) {
            eprintln!("skipping: cannot create symlinks here");
            return;
        }

        write_file_atomically(&link, |file| Ok(file.write_all(b"new build")?)).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"new build");
        assert_eq!(std::fs::read(&link).unwrap(), b"new build");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn write_through_dangling_symlink_creates_link_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.rbxl");
        let link = dir.path().join("link.rbxl");
        if !symlink_file(&real, &link) {
            eprintln!("skipping: cannot create symlinks here");
            return;
        }

        write_file_atomically(&link, |file| Ok(file.write_all(b"new build")?)).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"new build");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rbxl");
        let b = dir.path().join("b.rbxl");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();

        assert!(write_file_atomically(&a, |file| Ok(file.write_all(b"new build")?)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn new_file_gets_default_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("place.rbxl");

        write_file_atomically(&destination, |file| Ok(file.write_all(b"new build")?)).unwrap();

        let mode = std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
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
