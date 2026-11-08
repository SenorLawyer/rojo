use std::{
    borrow::Cow,
    collections::HashMap,
    io::{BufWriter, Write},
    mem::forget,
    path::{self, Path, PathBuf},
};

use anyhow::Context;
use clap::Parser;
use fs_err::File;
use memofs::Vfs;
use rayon::prelude::*;
use rbx_dom_weak::{types::Ref, Ustr};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

use crate::{
    serve_session::ServeSession,
    snapshot::{AppliedPatchSet, InstanceWithMeta, InstigatingSource, RojoTree},
};

use super::resolve_path;

const PATH_STRIP_FAILED_ERR: &str = "Failed to create relative paths for project file!";
const ABSOLUTE_PATH_FAILED_ERR: &str = "Failed to turn relative path into absolute path!";

/// Representation of a node in the generated sourcemap tree.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourcemapNode<'a> {
    name: &'a str,
    class_name: Ustr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_context: Option<String>,

    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "crate::path_serializer::serialize_vec_absolute"
    )]
    file_paths: Vec<Cow<'a, Path>>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    children: Vec<SourcemapNode<'a>>,
}

/// Generates a sourcemap file from the Rojo project.
#[derive(Debug, Parser)]
pub struct SourcemapCommand {
    /// Path to the project to use for the sourcemap. Defaults to the current
    /// directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Where to output the sourcemap. Omit this to use stdout instead of
    /// writing to a file.
    ///
    /// Should end in .json.
    #[clap(long, short)]
    pub output: Option<PathBuf>,

    /// If non-script files should be included or not. Defaults to false.
    #[clap(long)]
    pub include_non_scripts: bool,

    /// Whether to automatically recreate a snapshot when any input files change.
    #[clap(long)]
    pub watch: bool,

    /// Whether the sourcemap should use absolute paths instead of relative paths.
    #[clap(long)]
    pub absolute: bool,
}

impl SourcemapCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let project_path = dunce::canonicalize(resolve_path(&self.project)?)?;

        log::trace!("Constructing filesystem with StdBackend");
        let vfs = if self.watch {
            Vfs::new_default()?
        } else {
            Vfs::new_default_without_watch()
        };

        log::trace!("Setting up session for sourcemap generation");
        let session = ServeSession::new(vfs, project_path)?;
        let mut cursor = session.message_queue().cursor();

        let filter = if self.include_non_scripts {
            filter_nothing
        } else {
            filter_non_scripts
        };

        // Pre-build a rayon threadpool with a low number of threads to avoid
        // dynamic creation overhead on systems with a high number of cpus.
        log::trace!("Setting rayon global threadpool");
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_cpus::get().min(6))
            .build_global()
            .ok();

        log::trace!("Writing initial sourcemap");
        write_sourcemap(&session, self.output.as_deref(), filter, self.absolute)?;

        if self.watch {
            log::trace!("Setting up runtime for watch mode");
            let rt = Runtime::new().context("Failed to start the async runtime for watch mode")?;

            loop {
                let affected = match session.message_queue().subscribe(cursor) {
                    Ok(receiver) => match rt.block_on(receiver) {
                        Ok((new_cursor, patch_set)) => {
                            cursor = new_cursor;
                            let patches: Vec<_> =
                                patch_set.into_iter().map(|message| message.patch).collect();
                            patch_set_affects_sourcemap(&session, &patches, filter)
                        }
                        // The message queue was dropped, so there is nothing
                        // left to watch. Stop watching gracefully.
                        Err(_) => break,
                    },
                    // Changes were trimmed from history before we saw them,
                    // so rewrite from the current tree.
                    Err(_) => {
                        cursor = session.message_queue().cursor();
                        true
                    }
                };

                if affected {
                    write_sourcemap(&session, self.output.as_deref(), filter, self.absolute)?;
                }
            }
        }

        // Avoid dropping ServeSession: it's potentially VERY expensive to drop
        // and we're about to exit anyways.
        forget(session);

        Ok(())
    }
}

fn filter_nothing(_instance: &InstanceWithMeta) -> bool {
    true
}

pub(super) fn filter_non_scripts(instance: &InstanceWithMeta) -> bool {
    matches!(
        instance.class_name().as_str(),
        "Script" | "LocalScript" | "ModuleScript"
    )
}

pub(super) fn patch_set_affects_sourcemap(
    session: &ServeSession,
    patch_set: &[AppliedPatchSet],
    filter: fn(&InstanceWithMeta) -> bool,
) -> bool {
    let tree = session.tree();

    // A sourcemap has probably changed when:
    patch_set.par_iter().any(|set| {
        // 1. An instance was removed, in which case it will no
        // longer exist in the tree and we cant check the filter
        !set.removed.is_empty()
            // 2. A newly added instance passes the filter
            || set.added.iter().any(|referent| {
                // A later patch may already have removed this instance. Rebuild
                // conservatively when its class can no longer be checked.
                tree.get_instance(*referent)
                    .map(|instance| filter(&instance))
                    .unwrap_or(true)
            })
            // 3. An existing instance has its class name, name,
            // or file paths changed, and passes the filter
            || set.updated.iter().any(|updated| {
                let changed = updated.changed_class_name.is_some()
                    || updated.changed_properties.contains_key(&rbx_dom_weak::ustr("RunContext"))
                    || updated.changed_name.is_some()
                    || updated.changed_metadata.is_some();
                if changed {
                    tree.get_instance(updated.id)
                        .map(|instance| filter(&instance))
                        .unwrap_or(true)
                } else {
                    false
                }
            })
    })
}

fn recurse_create_node<'a>(
    tree: &'a RojoTree,
    referent: Ref,
    project_dir: &Path,
    filter: fn(&InstanceWithMeta) -> bool,
    use_absolute_paths: bool,
    file_overrides: &HashMap<Ref, PathBuf>,
) -> Option<SourcemapNode<'a>> {
    let instance = tree.get_instance(referent).expect("instance did not exist");

    let children: Vec<_> = instance
        .children()
        .par_iter()
        .filter_map(|&child_id| {
            recurse_create_node(
                tree,
                child_id,
                project_dir,
                filter,
                use_absolute_paths,
                file_overrides,
            )
        })
        .collect();

    // If this object has no children and doesn't pass the filter, it doesn't
    // contain any information we're looking for.
    if children.is_empty() && !filter(&instance) {
        return None;
    }

    if let Some(path) = file_overrides.get(&referent) {
        return Some(SourcemapNode {
            name: instance.name(),
            class_name: instance.class_name(),
            run_context: None,
            file_paths: vec![Cow::Owned(path.clone())],
            children,
        });
    }

    // A project node's own `$path` is used even when a parent node's
    // `$globIgnorePaths` match it, as `rojo build` does: naming a file is a
    // deliberate choice to include it.
    let explicit_path = match &instance.metadata().instigating_source {
        Some(InstigatingSource::ProjectNode { path, node, .. }) => {
            node.path.as_ref().and_then(|node_path| {
                let node_path = node_path.path();
                let full = if node_path.is_absolute() {
                    node_path.to_owned()
                } else {
                    path.parent()?.join(node_path)
                };
                dunce::canonicalize(full).ok()
            })
        }
        _ => None,
    };
    let file_paths = instance
        .metadata()
        .relevant_paths
        .iter()
        // Not all paths listed as relevant are guaranteed to exist.
        .filter(|path| path.is_file())
        // A folder lists its possible init files as relevant, including ones
        // excluded by a node's `$globIgnorePaths`, which do not make up the
        // instance.
        .filter(|path| {
            !crate::snapshot_middleware::is_ignored_by_node_rules(
                &instance.metadata().context.path_ignore_rules,
                path,
            ) || explicit_path
                .as_ref()
                .is_some_and(|explicit| dunce::canonicalize(path).ok().as_ref() == Some(explicit))
        })
        .map(|path| path.as_path());

    let mut output_file_paths: Vec<Cow<'a, Path>> =
        Vec::with_capacity(instance.metadata().relevant_paths.len());

    if use_absolute_paths {
        // It's somewhat important to note here that `path::absolute` takes in a Path and returns a PathBuf
        for val in file_paths {
            output_file_paths.push(Cow::Owned(
                path::absolute(val).expect(ABSOLUTE_PATH_FAILED_ERR),
            ));
        }
    } else {
        for val in file_paths {
            output_file_paths.push(Cow::from(
                pathdiff::diff_paths(val, project_dir).expect(PATH_STRIP_FAILED_ERR),
            ));
        }
    };

    Some(SourcemapNode {
        name: instance.name(),
        class_name: instance.class_name(),
        run_context: if instance.class_name() == "Script" {
            instance
                .properties()
                .get(&rbx_dom_weak::ustr("RunContext"))
                .and_then(|value| {
                    let rbx_dom_weak::types::Variant::Enum(context) = value else {
                        return None;
                    };
                    let db = rbx_reflection_database::get().ok()?;
                    db.enums
                        .get("RunContext")?
                        .items
                        .iter()
                        .find_map(|(name, id)| (*id == context.to_u32()).then(|| name.to_string()))
                })
        } else {
            None
        },
        file_paths: output_file_paths,
        children,
    })
}

fn write_sourcemap(
    session: &ServeSession,
    output: Option<&Path>,
    filter: fn(&InstanceWithMeta) -> bool,
    use_absolute_paths: bool,
) -> anyhow::Result<()> {
    let tree = session.tree();

    let root_node = recurse_create_node(
        &tree,
        tree.get_root_id(),
        session.root_dir(),
        filter,
        use_absolute_paths,
        &HashMap::new(),
    );

    if let Some(output_path) = output {
        let mut file = BufWriter::new(File::create(output_path)?);
        serde_json::to_writer(&mut file, &root_node)?;
        file.flush()?;

        println!("Created sourcemap at {}", output_path.display());
    } else {
        let output = serde_json::to_string(&root_node)?;
        println!("{}", output);
    }

    Ok(())
}

/// Writes the session's sourcemap as JSON, with paths relative to the
/// project folder.
///
/// Instances listed in `file_overrides` get that one path as their file
/// instead of their own files, which lets `rojo typecheck` point luau-lsp at
/// Luau written out for modules generated from other formats.
pub(super) fn write_sourcemap_json(
    session: &ServeSession,
    writer: impl Write,
    include_non_scripts: bool,
    file_overrides: &HashMap<Ref, PathBuf>,
) -> anyhow::Result<()> {
    let filter = if include_non_scripts {
        filter_nothing
    } else {
        filter_non_scripts
    };
    let tree = session.tree();
    let root_node = recurse_create_node(
        &tree,
        tree.get_root_id(),
        session.root_dir(),
        filter,
        false,
        file_overrides,
    );
    serde_json::to_writer(writer, &root_node)?;
    Ok(())
}

#[cfg(test)]
mod test {
    use crate::cli::sourcemap::SourcemapNode;
    use crate::cli::SourcemapCommand;
    use insta::internals::Content;
    use std::path::Path;

    #[test]
    fn removed_historical_instances_still_invalidate_sourcemap() {
        use crate::cli::sourcemap::{filter_non_scripts, patch_set_affects_sourcemap};
        use crate::serve_session::ServeSession;
        use crate::snapshot::{AppliedPatchSet, AppliedPatchUpdate};
        use memofs::Vfs;
        use rbx_dom_weak::types::Ref;

        let project_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("test-projects/relative_paths/project");
        let session = ServeSession::new(Vfs::new_default_without_watch(), project_path).unwrap();
        let missing = Ref::new();
        let mut update = AppliedPatchUpdate::new(missing);
        update.changed_name = Some("Renamed".into());

        for patch in [
            AppliedPatchSet {
                removed: Vec::new(),
                added: vec![missing],
                updated: Vec::new(),
            },
            AppliedPatchSet {
                removed: Vec::new(),
                added: Vec::new(),
                updated: vec![update],
            },
        ] {
            assert!(patch_set_affects_sourcemap(
                &session,
                &[patch],
                filter_non_scripts,
            ));
        }
    }

    #[test]
    fn internal_metadata_change_still_invalidates_sourcemap() {
        use crate::cli::sourcemap::{filter_nothing, patch_set_affects_sourcemap};
        use crate::serve_session::ServeSession;
        use crate::snapshot::{AppliedPatchSet, AppliedPatchUpdate, InstanceMetadata};
        use memofs::Vfs;

        let project_path = fs_err::canonicalize(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-projects")
                .join("relative_paths")
                .join("project"),
        )
        .unwrap();
        let session = ServeSession::new(Vfs::new_default().unwrap(), project_path).unwrap();

        let mut update = AppliedPatchUpdate::new(session.tree().get_root_id());
        update.changed_metadata = Some(InstanceMetadata::new());
        assert_eq!(update.changed_ignore_unknown_instances, None);

        let patch_set = vec![AppliedPatchSet {
            removed: Vec::new(),
            added: Vec::new(),
            updated: vec![update],
        }];

        assert!(patch_set_affects_sourcemap(
            &session,
            &patch_set,
            filter_nothing
        ));
    }

    #[test]
    fn maps_relative_paths() {
        let sourcemap_dir = tempfile::tempdir().unwrap();
        let sourcemap_output = sourcemap_dir.path().join("sourcemap.json");
        let project_path = fs_err::canonicalize(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-projects")
                .join("relative_paths")
                .join("project"),
        )
        .unwrap();
        let sourcemap_command = SourcemapCommand {
            project: project_path,
            output: Some(sourcemap_output.clone()),
            include_non_scripts: false,
            watch: false,
            absolute: false,
        };
        assert!(sourcemap_command.run().is_ok());

        let raw_sourcemap_contents = fs_err::read_to_string(sourcemap_output.as_path()).unwrap();
        let sourcemap_contents =
            serde_json::from_str::<SourcemapNode>(&raw_sourcemap_contents).unwrap();
        insta::assert_json_snapshot!(sourcemap_contents);
    }

    #[test]
    fn includes_script_run_contexts() {
        let sourcemap_dir = tempfile::tempdir().unwrap();
        let sourcemap_output = sourcemap_dir.path().join("sourcemap.json");
        let project_path = fs_err::canonicalize(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-projects")
                .join("run_contexts"),
        )
        .unwrap();
        let sourcemap_command = SourcemapCommand {
            project: project_path,
            output: Some(sourcemap_output.clone()),
            include_non_scripts: false,
            watch: false,
            absolute: false,
        };
        assert!(sourcemap_command.run().is_ok());

        let raw_sourcemap_contents = fs_err::read_to_string(sourcemap_output.as_path()).unwrap();
        let sourcemap_contents =
            serde_json::from_str::<SourcemapNode>(&raw_sourcemap_contents).unwrap();
        insta::assert_json_snapshot!(sourcemap_contents);
    }

    #[test]
    fn leaves_out_files_excluded_by_node_glob_ignore_paths() {
        let sourcemap_dir = tempfile::tempdir().unwrap();
        let sourcemap_output = sourcemap_dir.path().join("sourcemap.json");
        let sourcemap_command = SourcemapCommand {
            project: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("rojo-test")
                .join("build-tests")
                .join("partition_ignore_globs"),
            output: Some(sourcemap_output.clone()),
            include_non_scripts: false,
            watch: false,
            absolute: false,
        };
        assert!(sourcemap_command.run().is_ok());

        let raw = fs_err::read_to_string(sourcemap_output.as_path()).unwrap();
        let root = serde_json::from_str::<SourcemapNode>(&raw).unwrap();
        let mut file_paths = Vec::new();
        let mut queue = vec![&root];
        while let Some(node) = queue.pop() {
            file_paths.extend(
                node.file_paths
                    .iter()
                    .map(|path| path.to_string_lossy().replace('\\', "/")),
            );
            queue.extend(&node.children);
        }
        file_paths.sort_unstable();
        assert_eq!(
            file_paths,
            [
                "default.project.json",
                "filtered/Foo.luau",
                "filtered/Keep.spec.luau",
                "filtered/Nested/Bar.luau",
                "filtered/Server/Helper.luau",
                "package/default.project.json",
                "package/src/Foo.luau",
                "unfiltered/Foo.luau",
                "unfiltered/Foo.spec.luau",
                "unfiltered/Generated/Gen.luau",
                "unfiltered/Keep.spec.luau",
                "unfiltered/Nested/Bar.luau",
                "unfiltered/Nested/Bar.spec.luau",
                "unfiltered/Server/Helper.luau",
                "unfiltered/Server/init.server.luau",
            ]
        );
    }

    #[test]
    fn leaves_out_init_files_excluded_on_the_root_node() {
        // Also when `$path` reaches the folder another way, while the paths
        // the sourcemap lists are resolved ones.
        for path in ["src", "other/../src"] {
            leaves_out_init_files_excluded_on_the_root_node_at(path);
        }
    }

    fn leaves_out_init_files_excluded_on_the_root_node_at(path: &str) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs_err::create_dir_all(project.join("src")).unwrap();
        fs_err::create_dir_all(project.join("other")).unwrap();
        fs_err::write(
            project.join("default.project.json"),
            format!(
                r#"{{"name": "root", "tree": {{"$path": "{path}", "$globIgnorePaths": ["init.server.luau"]}}}}"#
            ),
        )
        .unwrap();
        fs_err::write(project.join("src/init.server.luau"), "print(1)").unwrap();
        fs_err::write(project.join("src/Helper.luau"), "return 1").unwrap();
        let sourcemap_output = dir.path().join("sourcemap.json");
        let sourcemap_command = SourcemapCommand {
            project,
            output: Some(sourcemap_output.clone()),
            include_non_scripts: true,
            watch: false,
            absolute: false,
        };
        assert!(sourcemap_command.run().is_ok());

        let raw = fs_err::read_to_string(sourcemap_output.as_path()).unwrap();
        let root = serde_json::from_str::<SourcemapNode>(&raw).unwrap();
        assert_eq!(root.class_name, "Folder", "{path}");
        let root_paths: Vec<_> = root
            .file_paths
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(root_paths, ["default.project.json"], "{path}");
    }

    /// A node's own `$path` naming a file that a parent node's
    /// `$globIgnorePaths` match is built, so the sourcemap lists it too.
    #[test]
    fn keeps_a_file_a_node_names_although_a_parent_excludes_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs_err::create_dir_all(project.join("filtered")).unwrap();
        fs_err::write(
            project.join("default.project.json"),
            r#"{
                "name": "root",
                "tree": {
                    "$path": "filtered",
                    "$globIgnorePaths": ["**/*.spec.luau"],
                    "Spec": { "$path": "filtered/Foo.spec.luau" }
                }
            }"#,
        )
        .unwrap();
        fs_err::write(project.join("filtered/Foo.spec.luau"), "return 1").unwrap();
        fs_err::write(project.join("filtered/Bar.spec.luau"), "return 1").unwrap();
        let sourcemap_output = dir.path().join("sourcemap.json");
        let sourcemap_command = SourcemapCommand {
            project,
            output: Some(sourcemap_output.clone()),
            include_non_scripts: true,
            watch: false,
            absolute: false,
        };
        assert!(sourcemap_command.run().is_ok());

        let raw = fs_err::read_to_string(sourcemap_output.as_path()).unwrap();
        let root = serde_json::from_str::<SourcemapNode>(&raw).unwrap();
        let names: Vec<_> = root.children.iter().map(|child| child.name).collect();
        assert_eq!(names, ["Spec"], "{raw}");
        let paths: Vec<_> = root.children[0]
            .file_paths
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(paths, ["filtered/Foo.spec.luau"], "{raw}");
    }

    #[test]
    fn maps_absolute_paths() {
        let sourcemap_dir = tempfile::tempdir().unwrap();
        let sourcemap_output = sourcemap_dir.path().join("sourcemap.json");
        let project_path = fs_err::canonicalize(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-projects")
                .join("relative_paths")
                .join("project"),
        )
        .unwrap();
        let sourcemap_command = SourcemapCommand {
            project: project_path,
            output: Some(sourcemap_output.clone()),
            include_non_scripts: false,
            watch: false,
            absolute: true,
        };
        assert!(sourcemap_command.run().is_ok());

        let raw_sourcemap_contents = fs_err::read_to_string(sourcemap_output.as_path()).unwrap();
        let sourcemap_contents =
            serde_json::from_str::<SourcemapNode>(&raw_sourcemap_contents).unwrap();
        insta::assert_json_snapshot!(sourcemap_contents, {
            ".**.filePaths" => insta::dynamic_redaction(|mut value, _path| {
                let mut paths_count = 0;

                match value {
                    Content::Seq(ref mut vec) => {
                        for path in vec.iter().map(|i| i.as_str().unwrap()) {
                            assert!(fs_err::canonicalize(path).is_ok(), "path was not valid");
                            assert!(Path::new(path).is_absolute(), "path was not absolute");

                            paths_count += 1;
                        }
                    }
                    _ => panic!("Expected filePaths to be a sequence"),
                }
                format!("[...{} path{} omitted...]", paths_count, if paths_count != 1 { "s" } else { "" } )
            })
        });
    }
}
