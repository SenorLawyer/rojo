use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use memofs::Vfs;
use rbx_dom_weak::{
    types::{Attributes, Ref, Variant},
    ustr, HashMapExt as _, Instance, Ustr, UstrMap, UstrSet,
};
use rbx_reflection::ClassTag;

use crate::{
    project::{PathNode, Project, ProjectNode},
    resolution::UnresolvedValue,
    snapshot::{
        content_property_aliases, migrate_content_property, InstanceContext, InstanceMetadata,
        InstanceSnapshot, InstanceWithMeta, InstigatingSource, PathIgnoreRule, SyncRule,
    },
    snapshot_middleware::Middleware,
    syncback::{
        filter_properties, is_excluded_by_rules, FsSnapshot, SyncbackReturn, SyncbackSnapshot,
    },
    variant_eq::variant_eq,
    RojoRef,
};

use super::{emit_legacy_scripts_default, snapshot_from_vfs, util::relevant_path_if_missing};

pub fn snapshot_project(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    // A project can refer back to itself through a directory, another project,
    // or a symlink. Track the current call chain, not all previously read projects.
    thread_local! {
        static ACTIVE: std::cell::RefCell<Vec<std::path::PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    struct ActiveProject;
    impl Drop for ActiveProject {
        fn drop(&mut self) {
            ACTIVE.with_borrow_mut(|paths| paths.pop());
        }
    }

    let canonical = vfs.canonicalize(path)?;
    ACTIVE.with_borrow_mut(|paths| {
        if paths.contains(&canonical) {
            bail!("Circular Rojo project reference: {}", canonical.display());
        }
        paths.push(canonical);
        Ok(())
    })?;
    let _active = ActiveProject;

    let project = Project::load_exact(vfs, path, Some(name))
        .with_context(|| format!("File was not a valid Rojo project: {}", path.display()))?;
    let project_name = match project.name.as_deref() {
        Some(name) => name,
        None => panic!("Project is missing a name"),
    };

    let inherited_context = context;
    let mut context = context.clone();
    context.clear_sync_rules();

    let rules = project.glob_ignore_paths.iter().map(|glob| PathIgnoreRule {
        glob: glob.clone(),
        base_path: project.folder_location().to_path_buf(),
        from_node: false,
        canonical_base: None,
    });

    let sync_rules = project.sync_rules.iter().map(|rule| SyncRule {
        base_path: project.folder_location().to_path_buf(),
        ..rule.clone()
    });

    context.add_sync_rules(sync_rules);
    context.add_path_ignore_rules(rules);
    context.set_emit_legacy_scripts(
        project
            .emit_legacy_scripts
            .or_else(emit_legacy_scripts_default)
            .unwrap(),
    );

    match snapshot_project_node(&context, path, project_name, &project.tree, vfs, None)? {
        Some(found_snapshot) => {
            let mut snapshot = found_snapshot;
            // Setting the instigating source to the project file path is a little
            // coarse.
            //
            // Ideally, we'd only snapshot the project file if the project file
            // actually changed. Because Rojo only has the concept of one
            // relevant path -> snapshot path mapping per instance, we pick the more
            // conservative approach of snapshotting the project file if any
            // relevant paths changed.
            snapshot.metadata.instigating_source = Some(path.to_path_buf().into());

            // Mark this snapshot (the root node of the project file) as being
            // related to the project file.
            //
            // We SHOULD NOT mark the project file as a relevant path for any
            // nodes that aren't roots. They'll be updated as part of the project
            // file being updated.
            snapshot.metadata.relevant_paths.push(path.to_path_buf());

            // A change to the project file re-snapshots it with this context.
            // It must not already hold this project's ignore rules, which are
            // only ever added to, or removing one would need a restart. The
            // snapshot's own context keeps them, since filtering this
            // instance's files (as sourcemap does) needs the effective rules.
            snapshot.metadata.reload_context = Some(Box::new(inherited_context.clone()));

            Ok(Some(snapshot))
        }
        None => Ok(None),
    }
}

/// Returns the folder a project node's `$globIgnorePaths` are relative to:
/// the node's `$path`, or the folder of the project file that `$path` names.
pub fn node_ignore_base(project_path: &Path, node: &ProjectNode) -> Option<PathBuf> {
    let path_node = node.path.as_ref()?;
    let mut base_path = project_path.parent().unwrap().join(path_node.path());
    if Project::is_project_file(&base_path) {
        base_path.pop();
    }
    Some(base_path)
}

/// Returns the ignore rules a project node's `$globIgnorePaths` add. Nodes
/// without a `$path` add none.
fn node_ignore_rules(vfs: &Vfs, project_path: &Path, node: &ProjectNode) -> Vec<PathIgnoreRule> {
    let Some(base_path) = node_ignore_base(project_path, node) else {
        return Vec::new();
    };
    // Instances record paths both as the project spells them and as the
    // file system resolves them, as their relevant paths do.
    let canonical_base = vfs
        .canonicalize(&base_path)
        .ok()
        .filter(|canonical| *canonical != base_path);

    node.glob_ignore_paths
        .iter()
        .map(|glob| PathIgnoreRule {
            glob: glob.clone(),
            base_path: base_path.clone(),
            from_node: true,
            canonical_base: canonical_base.clone(),
        })
        .collect()
}

pub fn snapshot_project_node(
    context: &InstanceContext,
    project_path: &Path,
    instance_name: &str,
    node: &ProjectNode,
    vfs: &Vfs,
    parent_class: Option<&str>,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let project_folder = project_path.parent().unwrap();

    let mut class_name_from_path = None;

    let name = Cow::Owned(instance_name.to_owned());
    let mut properties = UstrMap::new();
    let mut children = Vec::new();

    // If the path specified in the project is relative, we assume it's
    // relative to the folder that the project is in, project_folder.
    let full_path = node.path.as_ref().map(|path_node| {
        let path = path_node.path();
        if path.is_relative() {
            Cow::Owned(project_folder.join(path))
        } else {
            Cow::Borrowed(path)
        }
    });

    // The node's rules live in the context, not just this call, because live
    // sync re-snapshots a changed directory with the context stored on its
    // instance; that is what keeps a new ignored file from appearing later.
    // Child nodes inherit them too, so re-snapshotting this node (which passes
    // its stored context to its children) gives the same result as the
    // initial snapshot.
    let inherited_context = context;
    let node_context;
    let node_rules = node_ignore_rules(vfs, project_path, node);
    let context = if node_rules.is_empty() {
        context
    } else {
        let mut with_rules = context.clone();
        with_rules.add_path_ignore_rules(node_rules);
        node_context = with_rules;
        &node_context
    };

    let mut metadata = InstanceMetadata::new().context(context);

    if let Some(full_path) = &full_path {
        if let Some(snapshot) = snapshot_from_vfs(context, vfs, full_path)? {
            class_name_from_path = Some(snapshot.class_name);

            // Properties from the snapshot are pulled in unchanged, and
            // overridden by properties set on the project node.
            properties.reserve(snapshot.properties.len());
            for (key, value) in snapshot.properties.into_iter() {
                properties.insert(key, value);
            }

            // The snapshot's children will be merged with the children defined
            // in the project node, if there are any.
            children.reserve(snapshot.children.len());
            for child in snapshot.children.into_iter() {
                children.push(child);
            }

            // Take the snapshot's metadata as-is, which will be mutated later
            // on.
            metadata = snapshot.metadata;
        } else if let Some(missing) = relevant_path_if_missing(vfs, full_path) {
            // Keep the missing path mapped to this instance. Otherwise nothing
            // in the tree refers to it, and recreating it (after a package
            // reinstall or branch switch, for example) would never resync it.
            metadata.relevant_paths.push(missing);
        }
    }

    let class_name_from_inference = infer_class_name(&name, parent_class);

    let class_name = match (
        node.class_name,
        class_name_from_path,
        class_name_from_inference,
        &node.path,
    ) {
        // These are the easy, happy paths!
        (Some(project), None, None, _) => project,
        (None, Some(path), None, _) => path,
        (None, None, Some(inference), _) => inference,

        // If the user specifies a class name, but there's an inferred class
        // name, we prefer the name listed explicitly by the user.
        (Some(project), None, Some(_), _) => project,

        // If the user has a $path pointing to a folder and we're able to infer
        // a class name, let's use the inferred name. If the path we're pointing
        // to isn't a folder, though, that's a user error.
        (None, Some(path), Some(inference), _) => {
            if path == "Folder" {
                inference
            } else {
                path
            }
        }

        (Some(project), Some(path), _, _) => {
            if path == "Folder" {
                project
            } else {
                bail!(
                    "ClassName for Instance \"{}\" was specified in both the project file (as \"{}\") and from the filesystem (as \"{}\").\n\
                     If $className and $path are both set, $path must refer to a Folder.
                     \n\
                     Project path: {}\n\
                     Filesystem path: {}\n",
                    instance_name,
                    project,
                    path,
                    project_path.display(),
                    node.path.as_ref().unwrap().path().display()
                );
            }
        }

        (None, None, None, Some(PathNode::Optional(_))) => {
            return Ok(None);
        }

        (_, None, _, Some(PathNode::Required(path))) => {
            anyhow::bail!(
                "Rojo project referred to a file using $path that could not be turned into a Roblox Instance by Rojo.\n\
                Check that the file exists and is a file type known by Rojo.\n\
                \n\
                Project path: {}\n\
                File $path: {}",
                project_path.display(),
                path.display(),
            );
        }

        (None, None, None, None) => {
            bail!(
                "Instance \"{}\" is missing some required information.\n\
                 One of the following must be true:\n\
                 - $className must be set to the name of a Roblox class\n\
                 - $path must be set to a path of an instance\n\
                 - The instance must be a known service, like ReplicatedStorage\n\
                 \n\
                 Project path: {}",
                instance_name,
                project_path.display(),
            );
        }
    };

    for (child_name, child_project_node) in &node.children {
        match snapshot_project_node(
            context,
            project_path,
            child_name,
            child_project_node,
            vfs,
            Some(&class_name),
        )? {
            Some(child) => children.push(child),
            None => {
                // An optional child whose path is missing has no instance to
                // map that path to, so map it to this one. Its creation then
                // resyncs this node, which adds the child back.
                if let Some(path_node) = &child_project_node.path {
                    let full_path = project_folder.join(path_node.path());
                    if let Some(missing) = relevant_path_if_missing(vfs, &full_path) {
                        metadata.relevant_paths.push(missing);
                    }
                }
            }
        }
    }

    for (key, unresolved) in &node.properties {
        let value = unresolved
            .clone()
            .resolve(&class_name, key)
            .with_context(|| {
                format!(
                    "Unresolvable property in project at path {}",
                    project_path.display()
                )
            })?;

        match key.as_str() {
            "Name" | "Parent" => {
                log::warn!(
                    "Property '{}' cannot be set manually, ignoring. Attempted to set in '{}' at {}",
                    key,
                    instance_name,
                    project_path.display()
                );
                continue;
            }

            _ => {}
        }

        // A node's `$properties` override what its `$path` file sets. Files
        // read by rbx-dom only carry the `Content` name of an asset property,
        // such as `AudioContent`, so a node that overrides it with the legacy
        // name, such as `SoundId`, removes the file's value; otherwise the
        // file's `Content` value would win once legacy names are moved.
        for alias in content_property_aliases(&class_name, key) {
            if !node.properties.contains_key(&alias) {
                properties.remove(&alias);
            }
        }
        properties.insert(*key, value);
    }

    if !node.attributes.is_empty() {
        let mut attributes = Attributes::new();

        for (key, unresolved) in &node.attributes {
            let value = unresolved.clone().resolve_unambiguous().with_context(|| {
                format!(
                    "Unresolvable attribute in project at path {}",
                    project_path.display()
                )
            })?;

            attributes.insert(key.clone(), value);
        }

        properties.insert("Attributes".into(), attributes.into());
    }

    // If the user specified $ignoreUnknownInstances, overwrite the existing
    // value.
    //
    // If the user didn't specify it AND $path was not specified (meaning
    // there's no existing value we'd be stepping on from a project file or meta
    // file), set it to true.
    if let Some(ignore) = node.ignore_unknown_instances {
        metadata.ignore_unknown_instances = ignore;
    } else if node.path.is_none() {
        // TODO: Introduce a strict mode where $ignoreUnknownInstances is never
        // set implicitly.
        metadata.ignore_unknown_instances = true;
    }

    if let Some(id) = &node.id {
        metadata.specified_id = Some(RojoRef::new(id.clone()))
    }

    metadata.instigating_source = Some(InstigatingSource::ProjectNode {
        path: project_path.to_path_buf(),
        name: instance_name.to_string(),
        node: Box::new(node.clone()),
        parent_class: parent_class.map(|name| name.to_owned()),
    });
    // Re-snapshotting this node adds its own rules again, so it must start
    // from the context this call was given, not the one that already holds
    // them. The instance keeps the effective context for filtering its files.
    metadata.reload_context =
        (metadata.context != *inherited_context).then(|| Box::new(inherited_context.clone()));

    Ok(Some(InstanceSnapshot {
        snapshot_id: Ref::none(),
        name,
        class_name,
        properties,
        children,
        metadata,
    }))
}

pub fn syncback_project<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let old_inst = snapshot
        .old_inst()
        .expect("projects should always exist in both trees");
    // Generally, the path of a project is the first thing added to the relevant
    // paths. So, we take the last one.
    let project_path = old_inst
        .metadata()
        .relevant_paths
        .last()
        .expect("all projects should have a relevant path");
    let vfs = snapshot.vfs();

    log::debug!("Reloading project {} from vfs", project_path.display(),);
    let mut project = Project::load_exact(vfs, project_path, None)?;
    let base_path = project.folder_location().to_path_buf();

    // Sync rules for this project do not have their base rule set but it is
    // important when performing syncback on other projects.
    for rule in &mut project.sync_rules {
        rule.base_path.clone_from(&base_path)
    }

    let mut descendant_snapshots = Vec::new();
    let mut removed_descendants = Vec::new();

    let mut ref_to_path_map = HashMap::new();
    let mut old_child_map = HashMap::new();
    let mut new_child_map = HashMap::new();

    let mut node_changed_map = Vec::new();
    // Tracks whether any stale default-valued properties were removed from
    // project nodes. If so, we must reserialize even if
    // project_node_should_reserialize wouldn't otherwise detect a change
    // (it only compares node properties forward, not in reverse).
    let mut removed_stale_properties = false;
    let mut node_queue = VecDeque::with_capacity(1);
    node_queue.push_back((&mut project.tree, old_inst, snapshot.new_inst()));

    // A node whose `$path` is in a protected partition is skipped whole,
    // before anything checks its contents: they may be what makes syncback
    // fail, such as children with the same name, and they are not synced
    // back anyway.
    let protected = snapshot.protected();
    let is_protected = |node: &ProjectNode| {
        node.path.as_ref().is_some_and(|path| {
            let path = path.path();
            if path.is_absolute() {
                protected.contains(path)
            } else {
                protected.contains(&base_path.join(path))
            }
        })
    };

    while let Some((node, old_inst, new_inst)) = node_queue.pop_front() {
        if is_protected(node) {
            log::debug!("Skipping node {} in a protected partition", old_inst.name());
            continue;
        }
        log::debug!("Processing node {}", old_inst.name());
        let untouched = snapshot.untouched_properties(new_inst.referent());
        if old_inst.class_name() != new_inst.class {
            anyhow::bail!(
                "Cannot change the class of {} in project file {}.\n\
                Current class is {}, it is a {} in the input file.",
                old_inst.name(),
                project_path.display(),
                old_inst.class_name(),
                new_inst.class
            );
        }

        // TODO handle meta.json files in this branch. Right now, we perform
        // syncback if a node has `$path` set but the Middleware aren't aware
        // that the Instances they're running on originate in a project.json.
        // As a result, the `meta.json` syncback code is hardcoded to not work
        // if the Instance originates from a project file. However, we should
        // ideally use a .meta.json over the project node if it exists already.
        if node.path.is_some() {
            // Since the node has a path, we have to run syncback on it.
            let node_path = node.path.as_ref().map(PathNode::path).expect(
                "Project nodes with a path must have a path \
                If you see this message, something went seriously wrong. Please report it.",
            );
            let full_path = if node_path.is_absolute() {
                node_path.to_path_buf()
            } else {
                base_path.join(node_path)
            };

            let middleware = match Middleware::middleware_for_path(
                snapshot.vfs(),
                &project.sync_rules,
                &full_path,
            )? {
                Some(middleware) => middleware,
                // The only way this can happen at this point is if the path does
                // not exist on the file system or there's no middleware for it.
                None => anyhow::bail!(
                    "path does not exist or could not be turned into a file Rojo understands: {}",
                    full_path.display()
                ),
            };

            descendant_snapshots.push(
                snapshot
                    .with_new_path(full_path.clone(), new_inst.referent(), Some(old_inst.id()))
                    .middleware(middleware),
            );

            ref_to_path_map.insert(new_inst.referent(), full_path);

            // We only want to set properties if it needs it.
            if !middleware.handles_own_properties() {
                removed_stale_properties |=
                    project_node_property_syncback_path(snapshot, new_inst, node, &untouched);
            }
        } else {
            removed_stale_properties |=
                project_node_property_syncback_no_path(snapshot, new_inst, node, &untouched);
        }

        for child_ref in new_inst.children() {
            let child = snapshot
                .get_new_instance(*child_ref)
                .expect("all children of Instances should be in new DOM");
            if new_child_map.insert(&child.name, child).is_some() {
                anyhow::bail!(
                    "Instances that are direct children of an Instance that is made by a project file \
                    must have a unique name.\nThe child '{}' of '{}' is duplicated in the place file.", child.name, old_inst.name()
                );
            }
        }
        for child_ref in old_inst.children() {
            let child = snapshot
                .get_old_instance(*child_ref)
                .expect("all children of Instances should be in old DOM");
            if old_child_map.insert(child.name(), child).is_some() {
                anyhow::bail!(
                    "Instances that are direct children of an Instance that is made by a project file \
                    must have a unique name.\nThe child '{}' of '{}' is duplicated on the file system.", child.name(), old_inst.name()
                );
            }
        }

        // This loop does basic matching of Instance children to the node's
        // children. It ensures that `new_child_map` and `old_child_map` will
        // only contain Instances that don't belong to the project after this.
        for (child_name, child_node) in &mut node.children {
            // Neither added, removed nor checked; see `is_protected`.
            if is_protected(child_node) {
                new_child_map.remove(child_name);
                old_child_map.remove(child_name.as_str());
                continue;
            }
            // If a node's path is optional, we want to skip it if the path
            // doesn't exist since it isn't in the current old DOM.
            if let Some(path) = &child_node.path {
                if path.is_optional() {
                    let real_path = if path.path().is_absolute() {
                        path.path().to_path_buf()
                    } else {
                        base_path.join(path.path())
                    };
                    if !real_path.exists() {
                        log::warn!(
                            "Skipping node '{child_name}' of project because it is optional and not present on the disk.\n\
                            If this is not deliberate, please create a file or directory at {}", real_path.display()
                        );
                        continue;
                    }
                }
            }
            let new_equivalent = new_child_map.remove(child_name);
            let old_equivalent = old_child_map.remove(child_name.as_str());
            match (new_equivalent, old_equivalent) {
                (Some(new), Some(old)) => node_queue.push_back((child_node, old, new)),
                (_, None) => anyhow::bail!(
                    "The child '{child_name}' of Instance '{}' would be removed.\n\
                    Syncback cannot add or remove Instances from project {}",
                    old_inst.name(),
                    project_path.display()
                ),
                (None, _) => anyhow::bail!(
                    "The child '{child_name}' of Instance '{}' is present only in a project file,\n\
                    and not the provided file. Syncback cannot add or remove Instances from project:\n{}.",
                    old_inst.name(), project_path.display(),
                )
            }
        }

        // All of the children in this loop are by their nature not in the
        // project, so we just need to run syncback on them.
        for (name, new_child) in new_child_map.drain() {
            let parent_path = match ref_to_path_map.get(&new_child.parent()) {
                Some(path) => path.clone(),
                None => {
                    log::debug!("Skipping child {name} of node because it has no parent_path");
                    continue;
                }
            };

            // If a child also exists in the old tree, it will be caught in the
            // syncback on the project node path above (or is itself a node).
            // So the only things we need to run seperately is new children.
            if old_child_map.remove(name.as_str()).is_none() {
                let parent_middleware =
                    Middleware::middleware_for_path(vfs, &project.sync_rules, &parent_path)?
                        .expect("project nodes should have a middleware if they have children.");
                // If this node points directly to a project, it may still have
                // children but they'll be handled by syncback. This isn't a
                // concern with directories because they're singular things,
                // files that contain their own children.
                if parent_middleware != Middleware::Project {
                    descendant_snapshots.push(snapshot.with_base_path(
                        &parent_path,
                        new_child.referent(),
                        None,
                    )?);
                }
            }
        }
        removed_descendants.extend(old_child_map.drain().map(|(_, v)| v));
        node_changed_map.push((&node.properties, &node.attributes, old_inst, untouched))
    }
    let mut fs_snapshot = FsSnapshot::new();

    let mut needs_reserialize = removed_stale_properties;
    if !needs_reserialize {
        for (node_properties, node_attributes, old_inst, untouched) in node_changed_map {
            if project_node_should_reserialize(
                node_properties,
                node_attributes,
                old_inst,
                &untouched,
            )? {
                needs_reserialize = true;
                break;
            }
        }
    }
    if needs_reserialize {
        fs_snapshot.add_file(project_path, serde_json::to_vec_pretty(&project)?);
    }

    Ok(SyncbackReturn {
        fs_snapshot,
        children: descendant_snapshots,
        removed_children: removed_descendants,
    })
}

/// Syncs properties from the new instance into the project node.
/// Returns `true` if any stale properties were removed (i.e. properties
/// that existed in the project node but are now at their engine default).
///
/// Entries named in `untouched` (see
/// [`SyncbackSnapshot::untouched_properties`]) are left as they are.
fn project_node_property_syncback(
    project: &Project,
    filtered_properties: UstrMap<&Variant>,
    new_inst: &Instance,
    node: &mut ProjectNode,
    untouched: &UstrSet,
) -> bool {
    // A property the syncback rules leave out, such as an unscriptable one
    // while `syncUnscriptable` is off, has no value to sync, so its entry is
    // kept instead of being taken for a property reset to its default.
    let excluded = |name: &Ustr| {
        new_inst
            .properties
            .get(name)
            .is_some_and(|value| is_excluded_by_rules(project, &new_inst.class, name, value))
    };
    let class_data = rbx_reflection_database::get()
        .ok()
        .and_then(|db| db.classes.get(new_inst.class.as_str()));
    let properties = &mut node.properties;

    // The new DOM only has `Content` properties, such as `Sound.AudioContent`,
    // while the node may still use the legacy name, such as `Sound.SoundId`.
    // A legacy entry whose value still matches is kept as written, and the
    // `Content` name is not added next to it: having both in the file would
    // make the `Content` value win over later edits to the legacy one. A
    // legacy entry whose value changed is replaced by the `Content` name.
    let mut kept_legacy = HashSet::new();
    let mut covered_content = HashSet::new();
    let mut removed_legacy = false;
    let legacy_entries: Vec<(Ustr, Ustr, Variant)> = properties
        .iter()
        .filter_map(|(&name, unresolved)| {
            let value = unresolved.clone().resolve(&new_inst.class, &name).ok()?;
            let (content_name, migrated) =
                migrate_content_property(&new_inst.class, &name, &value)?;
            Some((name, content_name, migrated))
        })
        .collect();
    for (legacy_name, content_name, migrated) in legacy_entries {
        // The new DOM holds no value for an ignored or unreadable property,
        // which must not be mistaken for a value reset to its default.
        if untouched.contains(&legacy_name)
            || untouched.contains(&content_name)
            || excluded(&content_name)
        {
            kept_legacy.insert(legacy_name);
            continue;
        }
        // A property at its default is left out of `filtered_properties`.
        let current = filtered_properties.get(&content_name).copied().or_else(|| {
            class_data.and_then(|data| data.default_properties.get(content_name.as_str()))
        });
        if current.is_some_and(|current| variant_eq(current, &migrated)) {
            kept_legacy.insert(legacy_name);
            // If the node also names the `Content` property, that entry is
            // the one Rojo uses, so it is still updated below.
            if !properties.contains_key(&content_name) {
                covered_content.insert(content_name);
            }
        } else {
            properties.remove(&legacy_name);
            removed_legacy = true;
        }
    }

    let mut attributes = BTreeMap::new();
    for (&name, &value) in &filtered_properties {
        match value {
            Variant::Attributes(attrs) => {
                for (attr_name, attr_value) in attrs.iter() {
                    // We (probably) don't want to preserve internal attributes,
                    // only user defined ones.
                    if attr_name.starts_with("RBX") {
                        continue;
                    }
                    attributes.insert(
                        attr_name.clone(),
                        UnresolvedValue::from_variant_unambiguous(attr_value.clone()),
                    );
                }
            }
            _ if covered_content.contains(&name) => {}
            _ => {
                properties.insert(
                    name,
                    UnresolvedValue::from_variant(value.clone(), &new_inst.class, &name),
                );
            }
        }
    }

    // Remove stale properties: entries that exist in the project node's
    // $properties but are no longer in the filtered (non-default) properties
    // from the instance. This handles the case where Studio resets a property
    // to its engine default — filter_properties won't include it, so we need
    // to clean up the now-stale project entry.
    let len_before = properties.len();
    properties.retain(|prop_name, _| {
        if filtered_properties.contains_key(prop_name)
            || kept_legacy.contains(prop_name)
            || untouched.contains(prop_name)
            || excluded(prop_name)
        {
            return true;
        }
        // Only remove if the property has a known default value in the
        // reflection database. If there's no default, the property might be
        // absent from the instance for other reasons (e.g. unknown property),
        // so we conservatively keep it.
        if let Some(data) = &class_data {
            if data.default_properties.contains_key(prop_name.as_str()) {
                log::debug!(
                    "Removing stale property '{}' from project node for class '{}': \
                     value has been reset to engine default",
                    prop_name,
                    new_inst.class
                );
                return false;
            }
        }
        true
    });
    let removed_stale = removed_legacy || properties.len() < len_before;

    node.attributes = attributes;
    removed_stale
}

fn project_node_property_syncback_path(
    snapshot: &SyncbackSnapshot,
    new_inst: &Instance,
    node: &mut ProjectNode,
    untouched: &UstrSet,
) -> bool {
    let filtered_properties = snapshot
        .get_path_filtered_properties(new_inst.referent())
        .unwrap();
    project_node_property_syncback(
        snapshot.project(),
        filtered_properties,
        new_inst,
        node,
        untouched,
    )
}

fn project_node_property_syncback_no_path(
    snapshot: &SyncbackSnapshot,
    new_inst: &Instance,
    node: &mut ProjectNode,
    untouched: &UstrSet,
) -> bool {
    let filtered_properties = filter_properties(snapshot.project(), new_inst);
    project_node_property_syncback(
        snapshot.project(),
        filtered_properties,
        new_inst,
        node,
        untouched,
    )
}

fn project_node_should_reserialize(
    node_properties: &BTreeMap<Ustr, UnresolvedValue>,
    node_attributes: &BTreeMap<String, UnresolvedValue>,
    instance: InstanceWithMeta,
    untouched: &UstrSet,
) -> anyhow::Result<bool> {
    for (prop_name, unresolved_node_value) in node_properties {
        // These entries were left as they are, and the old tree has no value
        // for an ignored property to compare them with.
        if untouched.contains(prop_name) {
            continue;
        }
        if let Some(inst_value) = instance.properties().get(prop_name) {
            let node_value = unresolved_node_value
                .clone()
                .resolve(&instance.class_name(), prop_name)?;
            if !variant_eq(inst_value, &node_value) {
                return Ok(true);
            }
        } else if let Some((content_name, migrated)) = unresolved_node_value
            .clone()
            .resolve(&instance.class_name(), prop_name)
            .ok()
            .and_then(|value| migrate_content_property(&instance.class_name(), prop_name, &value))
        {
            // The old tree holds legacy asset properties under their `Content`
            // names, so a legacy entry is compared in that form.
            match instance.properties().get(&content_name) {
                _ if untouched.contains(&content_name) => {}
                Some(inst_value) if variant_eq(inst_value, &migrated) => {}
                _ => return Ok(true),
            }
        } else {
            return Ok(true);
        }
    }

    match instance.properties().get(&ustr("Attributes")) {
        Some(Variant::Attributes(inst_attributes)) => {
            // This will also catch if one is empty but the other isn't
            if node_attributes.len() != inst_attributes.len() {
                Ok(true)
            } else {
                for (attr_name, unresolved_node_value) in node_attributes {
                    if let Some(inst_value) = inst_attributes.get(attr_name.as_str()) {
                        let node_value = unresolved_node_value.clone().resolve_unambiguous()?;
                        if !variant_eq(inst_value, &node_value) {
                            return Ok(true);
                        }
                    } else {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
        Some(_) => Ok(true),
        None => {
            if !node_attributes.is_empty() {
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}

fn infer_class_name(name: &str, parent_class: Option<&str>) -> Option<Ustr> {
    // If className wasn't defined from another source, we may be able
    // to infer one.

    let parent_class = parent_class?;

    if parent_class == "DataModel" {
        // Members of DataModel with names that match known services are
        // probably supposed to be those services.

        let descriptor = rbx_reflection_database::get().unwrap().classes.get(name)?;

        if descriptor.tags.contains(&ClassTag::Service) {
            return Some(ustr(name));
        }
    } else if parent_class == "StarterPlayer" {
        // StarterPlayer has two special members with their own classes.

        if name == "StarterPlayerScripts" || name == "StarterCharacterScripts" {
            return Some(ustr(name));
        }
    } else if parent_class == "Workspace" {
        // Workspace has a special Terrain class inside it
        if name == "Terrain" {
            return Some(ustr(name));
        }
    }

    None
}

// #[cfg(feature = "broken-tests")]
#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};
    use std::path::PathBuf;

    #[test]
    fn rejects_project_referring_to_its_own_directory() {
        let mut fs = InMemoryFs::new();
        fs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([(
                "default.project.json",
                VfsSnapshot::file(r#"{"name":"cycle","tree":{"$path":"."}}"#),
            )]),
        )
        .unwrap();
        let err = snapshot_project(
            &InstanceContext::default(),
            &Vfs::new(fs),
            Path::new("/foo/default.project.json"),
            "cycle",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("Circular Rojo project reference"),
            "{err:#}"
        );
    }

    #[test]
    fn only_missing_paths_without_an_instance_are_relevant() {
        let mut fs = InMemoryFs::new();
        fs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "paths",
                            "tree": {
                                "$className": "Folder",
                                "Notes": {"$className": "Folder", "$path": "notes.unknown"},
                                "Unknown": {"$path": {"optional": "other.unknown"}},
                                "Packages": {"$path": {"optional": "Packages"}}
                            }
                        }"#,
                    ),
                ),
                ("notes.unknown", VfsSnapshot::file("")),
                ("other.unknown", VfsSnapshot::file("")),
            ]),
        )
        .unwrap();
        let snapshot = snapshot_project(
            &InstanceContext::default(),
            &Vfs::new(fs),
            Path::new("/foo/default.project.json"),
            "paths",
        )
        .unwrap()
        .unwrap();

        // Paths that exist but produce no instance are not watched for their
        // creation, which would watch them recursively.
        let notes = snapshot
            .children
            .iter()
            .find(|child| child.name == "Notes")
            .unwrap();
        assert!(!notes
            .metadata
            .relevant_paths
            .contains(&PathBuf::from("/foo/notes.unknown")));
        let relevant = &snapshot.metadata.relevant_paths;
        assert!(!relevant.contains(&PathBuf::from("/foo/other.unknown")));
        assert!(relevant.contains(&PathBuf::from("/foo/Packages")));
    }

    fn child<'a>(snapshot: &'a InstanceSnapshot, name: &str) -> &'a InstanceSnapshot {
        snapshot
            .children
            .iter()
            .find(|child| child.name == name)
            .unwrap_or_else(|| panic!("{} has no child {name}", snapshot.name))
    }

    fn child_names(snapshot: &InstanceSnapshot) -> Vec<&str> {
        let mut names: Vec<_> = snapshot
            .children
            .iter()
            .map(|child| child.name.as_ref())
            .collect();
        names.sort_unstable();
        names
    }

    fn partition_fixture(project: &str) -> Vfs {
        let partition = || {
            VfsSnapshot::dir([
                ("Foo.luau", VfsSnapshot::file("")),
                ("Foo.spec.luau", VfsSnapshot::file("")),
                (
                    "Nested",
                    VfsSnapshot::dir([
                        ("Bar.luau", VfsSnapshot::file("")),
                        ("Foo.spec.luau", VfsSnapshot::file("")),
                    ]),
                ),
                (
                    "Generated",
                    VfsSnapshot::dir([("Gen.luau", VfsSnapshot::file(""))]),
                ),
            ])
        };
        let mut fs = InMemoryFs::new();
        fs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([
                ("default.project.json", VfsSnapshot::file(project)),
                ("filtered", partition()),
                ("unfiltered", partition()),
            ]),
        )
        .unwrap();
        Vfs::new(fs)
    }

    fn snapshot_fixture(project: &str) -> InstanceSnapshot {
        snapshot_project(
            &InstanceContext::default(),
            &partition_fixture(project),
            Path::new("/foo/default.project.json"),
            "partitions",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances")
    }

    #[test]
    fn node_glob_ignore_paths_apply_only_beneath_that_node() {
        let snapshot = snapshot_fixture(
            r#"{
                "name": "partitions",
                "tree": {
                    "$className": "Folder",
                    "Filtered": {
                        "$path": "filtered",
                        "$globIgnorePaths": ["**/*.spec.luau", "Generated"]
                    },
                    "Unfiltered": { "$path": "unfiltered" }
                }
            }"#,
        );

        let filtered = child(&snapshot, "Filtered");
        assert_eq!(child_names(filtered), ["Foo", "Nested"]);
        assert_eq!(child_names(child(filtered, "Nested")), ["Bar"]);

        let unfiltered = child(&snapshot, "Unfiltered");
        assert_eq!(
            child_names(unfiltered),
            ["Foo", "Foo.spec", "Generated", "Nested"]
        );
        assert_eq!(
            child_names(child(unfiltered, "Nested")),
            ["Bar", "Foo.spec"]
        );
    }

    #[test]
    fn node_glob_ignore_paths_are_relative_to_node_path() {
        // `Foo.spec.luau` only matches at the top of the node's `$path`, and
        // a glob written relative to the project folder matches nothing.
        let snapshot = snapshot_fixture(
            r#"{
                "name": "partitions",
                "tree": {
                    "$className": "Folder",
                    "Filtered": {
                        "$path": "filtered",
                        "$globIgnorePaths": ["Foo.spec.luau", "filtered/Generated"]
                    }
                }
            }"#,
        );

        let filtered = child(&snapshot, "Filtered");
        assert_eq!(child_names(filtered), ["Foo", "Generated", "Nested"]);
        assert_eq!(child_names(child(filtered, "Nested")), ["Bar", "Foo.spec"]);
    }

    #[test]
    fn node_glob_ignore_paths_combine_with_project_glob_ignore_paths() {
        // The node's rules come after the project's, so its negation wins.
        let snapshot = snapshot_fixture(
            r#"{
                "name": "partitions",
                "globIgnorePaths": ["**/Gen.luau", "**/Foo.spec.luau"],
                "tree": {
                    "$className": "Folder",
                    "Filtered": {
                        "$path": "filtered",
                        "$globIgnorePaths": ["Nested/**", "!Foo.spec.luau"]
                    },
                    "Unfiltered": { "$path": "unfiltered" }
                }
            }"#,
        );

        let filtered = child(&snapshot, "Filtered");
        assert_eq!(
            child_names(filtered),
            ["Foo", "Foo.spec", "Generated", "Nested"]
        );
        assert!(child_names(child(filtered, "Nested")).is_empty());
        assert!(child_names(child(filtered, "Generated")).is_empty());

        let unfiltered = child(&snapshot, "Unfiltered");
        assert_eq!(child_names(unfiltered), ["Foo", "Generated", "Nested"]);
        assert_eq!(child_names(child(unfiltered, "Nested")), ["Bar"]);
    }

    #[test]
    fn node_glob_ignore_paths_are_kept_in_descendant_contexts() {
        // Live sync re-snapshots a changed directory with the context stored
        // on its instance, so that context must still carry the node's rules.
        let snapshot = snapshot_fixture(
            r#"{
                "name": "partitions",
                "tree": {
                    "$className": "Folder",
                    "Filtered": {
                        "$path": "filtered",
                        "$globIgnorePaths": ["**/*.spec.luau"]
                    },
                    "Unfiltered": { "$path": "unfiltered" }
                }
            }"#,
        );
        let rule = PathIgnoreRule {
            base_path: PathBuf::from("/foo/filtered"),
            glob: crate::glob::IgnorableGlob::new("**/*.spec.luau").unwrap(),
            from_node: true,
            canonical_base: None,
        };

        let filtered = child(&snapshot, "Filtered");
        for instance in [filtered, child(filtered, "Nested")] {
            assert_eq!(
                *instance.metadata.context.path_ignore_rules,
                std::slice::from_ref(&rule)
            );
        }
        let unfiltered = child(&snapshot, "Unfiltered");
        assert!(unfiltered.metadata.context.path_ignore_rules.is_empty());
        assert!(snapshot.metadata.context.path_ignore_rules.is_empty());

        let nested = child(filtered, "Nested");
        let resnapshot = snapshot_from_vfs(
            &nested.metadata.context,
            &partition_fixture(r#"{"tree": {"$path": "filtered"}}"#),
            Path::new("/foo/filtered/Nested"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(child_names(&resnapshot), ["Bar"]);
    }

    #[test]
    fn node_glob_ignore_paths_treat_ignored_init_files_as_absent() {
        let server = || {
            VfsSnapshot::dir([(
                "Server",
                VfsSnapshot::dir([
                    ("init.server.luau", VfsSnapshot::file("print(1)")),
                    ("Helper.luau", VfsSnapshot::file("")),
                ]),
            )])
        };
        let mut fs = InMemoryFs::new();
        fs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "partitions",
                            "tree": {
                                "$className": "Folder",
                                "Filtered": {
                                    "$path": "filtered",
                                    "$globIgnorePaths": ["**/*.server.luau"]
                                },
                                "Unfiltered": { "$path": "unfiltered" }
                            }
                        }"#,
                    ),
                ),
                ("filtered", server()),
                ("unfiltered", server()),
            ]),
        )
        .unwrap();
        let snapshot = snapshot_project(
            &InstanceContext::default(),
            &Vfs::new(fs),
            Path::new("/foo/default.project.json"),
            "partitions",
        )
        .unwrap()
        .unwrap();

        let filtered = child(child(&snapshot, "Filtered"), "Server");
        assert_eq!(filtered.class_name, "Folder");
        assert_eq!(child_names(filtered), ["Helper"]);
        let unfiltered = child(child(&snapshot, "Unfiltered"), "Server");
        assert_ne!(unfiltered.class_name, "Folder");
        assert_eq!(child_names(unfiltered), ["Helper"]);
    }

    #[test]
    fn node_glob_ignore_paths_are_based_at_a_project_files_folder() {
        let node: ProjectNode = crate::json::from_str(
            r#"{"$path": "package/default.project.json", "$globIgnorePaths": ["**"]}"#,
        )
        .unwrap();
        let rules = node_ignore_rules(
            &Vfs::new(InMemoryFs::new()),
            Path::new("/foo/default.project.json"),
            &node,
        );
        assert_eq!(rules[0].base_path, PathBuf::from("/foo/package"));

        let node: ProjectNode =
            crate::json::from_str(r#"{"$path": "src", "$globIgnorePaths": ["**"]}"#).unwrap();
        let rules = node_ignore_rules(
            &Vfs::new(InMemoryFs::new()),
            Path::new("/foo/default.project.json"),
            &node,
        );
        assert_eq!(rules[0].base_path, PathBuf::from("/foo/src"));
    }

    #[test]
    fn project_root_reloads_from_the_inherited_context() {
        // Reloading the project file re-snapshots it with the root's reload
        // context, and ignore rules are only ever added, so the root's own
        // rules must not be in it. Its effective context keeps them.
        let snapshot = snapshot_fixture(
            r#"{
                "name": "partitions",
                "globIgnorePaths": ["**/Gen.luau"],
                "tree": { "$path": "filtered", "$globIgnorePaths": ["**/*.spec.luau"] }
            }"#,
        );
        let reload = snapshot.metadata.reload_context.as_deref().unwrap();
        assert!(reload.path_ignore_rules.is_empty());
        assert_eq!(snapshot.metadata.context.path_ignore_rules.len(), 2);
        assert_eq!(child_names(&snapshot), ["Foo", "Generated", "Nested"]);
    }

    #[test]
    fn project_nodes_reload_without_their_own_rules() {
        // Live sync re-snapshots a project node with its reload context and
        // adds the node's rules again, so that context must not hold them.
        let snapshot = snapshot_fixture(
            r#"{
                "name": "partitions",
                "tree": {
                    "$className": "Folder",
                    "Filtered": {
                        "$path": "filtered",
                        "$globIgnorePaths": ["**/*.spec.luau"]
                    },
                    "Unfiltered": { "$path": "unfiltered" }
                }
            }"#,
        );
        let filtered = child(&snapshot, "Filtered");
        assert_eq!(filtered.metadata.context.path_ignore_rules.len(), 1);
        assert!(filtered
            .metadata
            .reload_context()
            .path_ignore_rules
            .is_empty());
        let unfiltered = child(&snapshot, "Unfiltered");
        assert!(unfiltered.metadata.reload_context.is_none());
    }

    #[test]
    fn project_glob_ignore_paths_do_not_hide_init_files() {
        // Only node-level `$globIgnorePaths` decide whether an init file
        // counts. The project-wide list never did, so it still does not.
        let mut fs = InMemoryFs::new();
        fs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "partitions",
                            "globIgnorePaths": ["**/*.server.luau"],
                            "tree": { "$path": "src" }
                        }"#,
                    ),
                ),
                (
                    "src",
                    VfsSnapshot::dir([(
                        "Server",
                        VfsSnapshot::dir([("init.server.luau", VfsSnapshot::file("print(1)"))]),
                    )]),
                ),
            ]),
        )
        .unwrap();
        let snapshot = snapshot_project(
            &InstanceContext::default(),
            &Vfs::new(fs),
            Path::new("/foo/default.project.json"),
            "partitions",
        )
        .unwrap()
        .unwrap();
        assert_eq!(child(&snapshot, "Server").class_name, "Script");
    }

    #[ignore = "Functionality moved to root snapshot middleware"]
    #[test]
    fn project_from_folder() {
        let _ = env_logger::try_init();

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([(
                "default.project.json",
                VfsSnapshot::file(
                    r#"
                    {
                        "name": "indirect-project",
                        "tree": {
                            "$className": "Folder"
                        }
                    }
                "#,
                ),
            )]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_project(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo"),
            "NOT_IN_SNAPSHOT",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances");

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn project_from_direct_file() {
        let _ = env_logger::try_init();

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([(
                "hello.project.json",
                VfsSnapshot::file(
                    r#"
                    {
                        "name": "direct-project",
                        "tree": {
                            "$className": "Model"
                        }
                    }
                "#,
                ),
            )]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_project(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo/hello.project.json"),
            "NOT_IN_SNAPSHOT",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances");

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn project_with_resolved_properties() {
        let _ = env_logger::try_init();

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.project.json",
            VfsSnapshot::file(
                r#"
                    {
                        "name": "resolved-properties",
                        "tree": {
                            "$className": "StringValue",
                            "$properties": {
                                "Value": {
                                    "String": "Hello, world!"
                                }
                            }
                        }
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_project(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.project.json"),
            "NOT_IN_SNAPSHOT",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances");

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn project_with_unresolved_properties() {
        let _ = env_logger::try_init();

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.project.json",
            VfsSnapshot::file(
                r#"
                    {
                        "name": "unresolved-properties",
                        "tree": {
                            "$className": "StringValue",
                            "$properties": {
                                "Value": "Hi!"
                            }
                        }
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_project(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.project.json"),
            "NOT_IN_SNAPSHOT",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances");

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn project_with_children() {
        let _ = env_logger::try_init();

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.project.json",
            VfsSnapshot::file(
                r#"
                    {
                        "name": "children",
                        "tree": {
                            "$className": "Folder",

                            "Child": {
                                "$className": "Model"
                            }
                        }
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_project(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.project.json"),
            "NOT_IN_SNAPSHOT",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances");

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn project_with_path_to_txt() {
        let _ = env_logger::try_init();

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"
                    {
                        "name": "path-project",
                        "tree": {
                            "$path": "other.txt"
                        }
                    }
                "#,
                    ),
                ),
                ("other.txt", VfsSnapshot::file("Hello, world!")),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_project(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo/default.project.json"),
            "NOT_IN_SNAPSHOT",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances");

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn project_with_path_to_project() {
        let _ = env_logger::try_init();

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"
                    {
                        "name": "path-project",
                        "tree": {
                            "$path": "other.project.json"
                        }
                    }
                "#,
                    ),
                ),
                (
                    "other.project.json",
                    VfsSnapshot::file(
                        r#"
                    {
                        "name": "other-project",
                        "tree": {
                            "$className": "Model"
                        }
                    }
                "#,
                    ),
                ),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_project(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo/default.project.json"),
            "NOT_IN_SNAPSHOT",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances");

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn project_with_path_to_project_with_children() {
        let _ = env_logger::try_init();

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"
                    {
                        "name": "path-child-project",
                        "tree": {
                            "$path": "other.project.json"
                        }
                    }
                "#,
                    ),
                ),
                (
                    "other.project.json",
                    VfsSnapshot::file(
                        r#"
                    {
                        "name": "other-project",
                        "tree": {
                            "$className": "Folder",

                            "SomeChild": {
                                "$className": "Model"
                            }
                        }
                    }
                "#,
                    ),
                ),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_project(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo/default.project.json"),
            "NOT_IN_SNAPSHOT",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances");

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    /// Ensures that if a property is defined both in the resulting instance
    /// from $path and also in $properties, that the $properties value takes
    /// precedence.
    #[test]
    fn project_path_property_overrides() {
        let _ = env_logger::try_init();

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"
                    {
                        "name": "path-property-override",
                        "tree": {
                            "$path": "other.project.json",
                            "$properties": {
                                "Value": "Changed"
                            }
                        }
                    }
                "#,
                    ),
                ),
                (
                    "other.project.json",
                    VfsSnapshot::file(
                        r#"
                    {
                        "name": "other-project",
                        "tree": {
                            "$className": "StringValue",
                            "$properties": {
                                "Value": "Original"
                            }
                        }
                    }
                "#,
                    ),
                ),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_project(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo/default.project.json"),
            "NOT_IN_SNAPSHOT",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances");

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn no_name_project() {
        let _ = env_logger::try_init();

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([(
                "default.project.json",
                VfsSnapshot::file(
                    r#"
                    {
                        "tree": {
                            "$className": "Model"
                        }
                    }
                "#,
                ),
            )]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_project(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo/default.project.json"),
            "no_name_project",
        )
        .expect("snapshot error")
        .expect("snapshot returned no instances");

        insta::assert_yaml_snapshot!(instance_snapshot);
    }
}
