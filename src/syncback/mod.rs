mod file_names;
mod fs_snapshot;
mod hash;
mod property_filter;
mod ref_properties;
mod snapshot;

use anyhow::Context;
use indexmap::IndexMap;
use memofs::Vfs;
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, Instance, Ustr, UstrSet, WeakDom,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use crate::{
    glob::{Glob, IgnorableGlob},
    snapshot::{
        content_property_aliases, is_path_ignored, object_content_warning, remove_object_contents,
        InstanceWithMeta, PathIgnoreRule, RojoTree,
    },
    snapshot_middleware::{node_ignore_rules, Middleware},
    syncback::{
        ref_properties::{collect_referents, link_referents},
        snapshot::inst_path,
    },
    Project,
};

pub use file_names::{extension_for_middleware, name_for_inst, validate_file_name};
pub use fs_snapshot::FsSnapshot;
pub use hash::*;
pub use property_filter::{
    filter_properties, filter_properties_preallocated, is_excluded_by_rules,
};
pub use snapshot::{SyncbackData, SyncbackSnapshot};

/// The name of an enviroment variable to use to override the behavior of
/// syncback on model files.
/// By default, syncback will use `Rbxm` for model files.
/// If this is set to `1`, it will instead use `Rbxmx`. If it is set to `2`,
/// it will use `JsonModel`.
///
/// This will **not** override existing `Rbxm` middleware. It will only impact
/// new files.
const DEBUG_MODEL_FORMAT_VAR: &str = "ROJO_SYNCBACK_DEBUG";

/// A glob that can be used to tell if a path contains a `.git` folder.
static GIT_IGNORE_GLOB: OnceLock<Glob> = OnceLock::new();

pub fn syncback_loop(
    vfs: &Vfs,
    old_tree: &mut RojoTree,
    mut new_tree: WeakDom,
    project: &Project,
) -> anyhow::Result<FsSnapshot> {
    let ignore_patterns = project
        .syncback_rules
        .as_ref()
        .map(|rules| rules.compile_globs())
        .transpose()?;

    // TODO: Add a better way to tell if the root of a project is a directory
    let skip_pruning = if let Some(path) = &project.tree.path {
        let middleware =
            Middleware::middleware_for_path(vfs, &project.sync_rules, path.path()).unwrap();
        if let Some(middleware) = middleware {
            middleware.is_dir()
        } else {
            false
        }
    } else {
        false
    };
    if !skip_pruning {
        // Strip out any objects from the new tree that aren't in the old tree. This
        // is necessary so that hashing the roots of each tree won't always result
        // in different hashes. Shout out to Roblox for serializing a bunch of
        // Services nobody cares about.
        log::debug!("Pruning new tree");
        strip_unknown_root_children(&mut new_tree, old_tree);
    }

    log::debug!("Collecting referents for new DOM...");
    let deferred_referents = collect_referents(&new_tree);

    // Remove any properties that are manually blocked from syncback via the
    // project file.
    log::debug!("Pre-filtering properties on DOMs");
    let mut unreadable_properties = HashMap::new();
    for referent in descendants(&new_tree, new_tree.root_ref()) {
        let new_inst = new_tree.get_by_ref_mut(referent).unwrap();
        if let Some(filter) = get_property_filter(project, new_inst) {
            for prop in filter {
                new_inst.properties.remove(&prop);
            }
        }

        // A `Content` value that refers to an object, such as an
        // `EditableImage`, cannot be written to any file Rojo syncs back to
        // (rbx_xml cannot even represent it). Such a property is treated as
        // if `ignoreProperties` listed it for this instance: it is removed
        // from both trees below and remembered, so that files which set it
        // are left as they are instead of being treated as reset to default.
        let removed = remove_object_contents(&mut new_inst.properties);
        if !removed.is_empty() {
            let class = new_inst.class;
            let path = inst_path(&new_tree, referent);
            for name in &removed {
                log::warn!("{}", object_content_warning(&class, &path, name));
            }
            unreadable_properties.insert(referent, removed.into_iter().collect::<UstrSet>());
        }
    }
    for referent in descendants(old_tree.inner(), old_tree.get_root_id()) {
        let mut old_inst_rojo = old_tree.get_instance_mut(referent).unwrap();
        let old_inst = old_inst_rojo.inner_mut();
        if let Some(filter) = get_property_filter(project, old_inst) {
            for prop in filter {
                old_inst.properties.remove(&prop);
            }
        }
    }
    for (new_ref, names) in &unreadable_properties {
        // Without this, the file's own value would make the two trees differ
        // and the file would be rewritten on every run.
        if let Some(old_ref) = old_instance_at_same_path(&new_tree, *new_ref, old_tree) {
            let mut old_inst = old_tree.get_instance_mut(old_ref).unwrap();
            for name in names {
                old_inst.inner_mut().properties.remove(name);
            }
        }
    }

    // Handle removing the current camera.
    if let Some(syncback_rules) = &project.syncback_rules {
        if !syncback_rules.sync_current_camera.unwrap_or_default() {
            log::debug!("Removing CurrentCamera from new DOM");
            let mut camera_ref = None;
            for child_ref in new_tree.root().children() {
                let inst = new_tree.get_by_ref(*child_ref).unwrap();
                if inst.class == "Workspace" {
                    camera_ref = inst.properties.get(&ustr("CurrentCamera"));
                    break;
                }
            }
            if let Some(Variant::Ref(camera_ref)) = camera_ref {
                if new_tree.get_by_ref(*camera_ref).is_some() {
                    new_tree.destroy(*camera_ref);
                }
            }
        }
    }

    let ignore_referents = project
        .syncback_rules
        .as_ref()
        .and_then(|s| s.ignore_referents)
        .unwrap_or_default();
    if !ignore_referents {
        log::debug!("Linking referents for new DOM");
        link_referents(deferred_referents, &mut new_tree)?;
    } else {
        log::debug!("Skipping referent linking as per project syncback rules");
    }

    // As with pruning the children of the new root, we need to ensure the roots
    // for both DOMs have the same name otherwise their hashes will always be
    // different.
    new_tree.root_mut().name = old_tree.root().name().to_string();

    log::debug!("Hashing project DOM");
    let old_hashes = hash_tree(project, old_tree.inner(), old_tree.get_root_id());
    log::debug!("Hashing file DOM");
    let new_hashes = hash_tree(project, &new_tree, new_tree.root_ref());

    // Files a project node's `$globIgnorePaths` exclude never become instances,
    // so without this an instance Studio has at such a path would be written
    // there as new.
    let node_rules = collect_node_ignore_rules(vfs, old_tree)?;

    let project_path = project.folder_location();
    let is_writable = |path: &Path| {
        is_valid_path(&ignore_patterns, project_path, path) && !is_path_ignored(&node_rules, path)
    };

    let syncback_data = SyncbackData {
        vfs,
        old_tree,
        new_tree: &new_tree,
        project,
        unreadable_properties: &unreadable_properties,
    };

    let mut snapshots = vec![SyncbackSnapshot {
        data: syncback_data,
        old: Some(old_tree.get_root_id()),
        new: new_tree.root_ref(),
        path: project.file_location.clone(),
        middleware: Some(Middleware::Project),
    }];

    let mut fs_snapshot = FsSnapshot::new();

    'syncback: while let Some(snapshot) = snapshots.pop() {
        let inst_path = snapshot.get_new_inst_path(snapshot.new);
        // We can quickly check that two subtrees are identical and if they are,
        // skip reconciling them.
        if let Some(old_ref) = snapshot.old {
            match (old_hashes.get(&old_ref), new_hashes.get(&snapshot.new)) {
                (Some(old), Some(new)) => {
                    if old == new {
                        log::trace!(
                            "Skipping {inst_path} due to it being identically hashed as {old:?}"
                        );
                        continue;
                    }
                }
                _ => unreachable!("All Instances in both DOMs should have hashes"),
            }
        }

        if !is_writable(&snapshot.path) {
            log::debug!("Skipping {inst_path} because its path matches ignore pattern");
            continue;
        }
        if let Some(syncback_rules) = &project.syncback_rules {
            // Ignore trees;
            for ignored in &syncback_rules.ignore_trees {
                if inst_path.starts_with(ignored.as_str()) {
                    log::debug!("Tree {inst_path} is blocked by project");
                    continue 'syncback;
                }
            }
        }

        let middleware = get_best_middleware(&snapshot);

        log::trace!(
            "Middleware for {inst_path} is {:?} (path is {})",
            middleware,
            snapshot.path.display()
        );

        // Modules generated from data files keep their source file as it is.
        // A directory with an `init.data.csv` file is not skipped here, so
        // its children are still synced back.
        if matches!(
            middleware,
            Middleware::Json | Middleware::Toml | Middleware::DataCsv
        ) {
            log::warn!("Cannot syncback {middleware:?} at {inst_path}, skipping");
            continue;
        }

        let syncback = match middleware.syncback(&snapshot) {
            Ok(syncback) => syncback,
            Err(err) if middleware == Middleware::Dir => {
                let new_middleware = match env::var(DEBUG_MODEL_FORMAT_VAR) {
                    Ok(value) if value == "1" => Middleware::Rbxmx,
                    Ok(value) if value == "2" => Middleware::JsonModel,
                    _ => Middleware::Rbxm,
                };
                let file_name = snapshot
                    .path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .context("Directory middleware should have a name in its path")?;
                let mut path = snapshot.path.clone();
                path.set_file_name(format!(
                    "{file_name}.{}",
                    extension_for_middleware(new_middleware)
                ));
                let new_snapshot = snapshot.with_new_path(path, snapshot.new, snapshot.old);
                log::warn!(
                    "Could not syncback {inst_path} as a Directory because: {err}.\n\
                    It will instead be synced back as a {new_middleware:?}."
                );
                let new_syncback_result = new_middleware
                    .syncback(&new_snapshot)
                    .with_context(|| format!("Failed to syncback {inst_path}"));
                if new_syncback_result.is_ok() && snapshot.old_inst().is_some() {
                    // We need to remove the old FS representation if we're
                    // reserializing it as an rbxm.
                    remove_dir_keeping_ignored(vfs, &mut fs_snapshot, &snapshot.path, &node_rules)?;
                }
                new_syncback_result?
            }
            Err(err) => anyhow::bail!("Failed to syncback {inst_path} because {err}"),
        };

        if !syncback.removed_children.is_empty() {
            log::debug!(
                "removed children for {inst_path}: {}",
                syncback.removed_children.len()
            );
            'remove: for inst in &syncback.removed_children {
                let path = inst.metadata().instigating_source.as_ref().unwrap().path();
                let inst_path = snapshot.get_old_inst_path(inst.id());
                if !is_writable(path) {
                    log::debug!(
                        "Skipping removing {} because its matches an ignore pattern",
                        path.display()
                    );
                    continue;
                }
                if let Some(syncback_rules) = &project.syncback_rules {
                    for ignored in &syncback_rules.ignore_trees {
                        if inst_path.starts_with(ignored.as_str()) {
                            log::debug!("Skipping removing {inst_path} because its path is blocked by project");
                            continue 'remove;
                        }
                    }
                }
                if path.is_dir() {
                    remove_dir_keeping_ignored(vfs, &mut fs_snapshot, path, &node_rules)?;
                } else {
                    fs_snapshot.remove_file(path)
                }
            }
        }

        // TODO provide replacement snapshots for e.g. two way sync

        fs_snapshot.merge_with_filter(syncback.fs_snapshot, is_writable);

        snapshots.extend(syncback.children);
    }

    Ok(fs_snapshot)
}

/// Reads the `$globIgnorePaths` of every node in every project file the old
/// tree came from. They are read from the project files rather than from each
/// instance's `InstigatingSource`, because a project's root node is tagged
/// with the project file's path instead, which would lose its rules.
fn collect_node_ignore_rules(
    vfs: &Vfs,
    old_tree: &RojoTree,
) -> anyhow::Result<Vec<PathIgnoreRule>> {
    let mut project_paths = Vec::new();
    for inst in old_tree.descendants(old_tree.get_root_id()) {
        let metadata = inst.metadata();
        let instigating = metadata
            .instigating_source
            .as_ref()
            .map(|source| source.path());
        for path in metadata
            .relevant_paths
            .iter()
            .map(PathBuf::as_path)
            .chain(instigating)
        {
            if Project::is_project_file(path) && !project_paths.iter().any(|p| p == path) {
                project_paths.push(path.to_path_buf());
            }
        }
    }

    let mut rules = Vec::new();
    for project_path in project_paths {
        if !vfs.exists(&project_path)? {
            continue;
        }
        // The fallback name only keeps loading from failing for a project
        // without `name`; the name is not used here.
        let project = Project::load_exact(vfs, &project_path, Some("project"))?;
        let mut nodes = vec![&project.tree];
        while let Some(node) = nodes.pop() {
            for rule in node_ignore_rules(&project_path, node) {
                if !rules.contains(&rule) {
                    rules.push(rule);
                }
            }
            nodes.extend(node.children.values());
        }
    }
    Ok(rules)
}

/// Removes the directory at `path`, except for files beneath it that a node's
/// `$globIgnorePaths` exclude. Those files are not part of the tree syncback
/// compares, so removing them with their directory would delete files that
/// Studio never held. The directory is kept if anything in it is kept.
fn remove_dir_keeping_ignored(
    vfs: &Vfs,
    fs_snapshot: &mut FsSnapshot,
    path: &Path,
    rules: &[PathIgnoreRule],
) -> anyhow::Result<()> {
    let may_apply = rules
        .iter()
        .any(|rule| path.starts_with(&rule.base_path) || rule.base_path.starts_with(path));
    if !may_apply {
        fs_snapshot.remove_dir(path);
        return Ok(());
    }

    // Returns whether anything under `dir` is kept, and the removals that
    // leave only the kept entries.
    fn plan(
        vfs: &Vfs,
        dir: &Path,
        rules: &[PathIgnoreRule],
    ) -> anyhow::Result<(bool, Vec<(PathBuf, bool)>)> {
        let mut kept = false;
        let mut removals = Vec::new();
        for entry in vfs.read_dir(dir)? {
            let entry_path = entry?.path().to_path_buf();
            if is_path_ignored(rules, &entry_path) {
                kept = true;
            } else if vfs.metadata(&entry_path)?.is_dir() {
                let (child_kept, child_removals) = plan(vfs, &entry_path, rules)?;
                if child_kept {
                    kept = true;
                    removals.extend(child_removals);
                } else {
                    removals.push((entry_path, true));
                }
            } else {
                removals.push((entry_path, false));
            }
        }
        Ok((kept, removals))
    }

    let (kept, removals) = plan(vfs, path, rules)?;
    if !kept {
        fs_snapshot.remove_dir(path);
        return Ok(());
    }
    log::warn!(
        "Keeping {} because it holds files excluded by $globIgnorePaths",
        path.display()
    );
    for (removal, is_dir) in removals {
        if is_dir {
            fs_snapshot.remove_dir(removal);
        } else {
            fs_snapshot.remove_file(removal);
        }
    }
    Ok(())
}

pub struct SyncbackReturn<'sync> {
    pub fs_snapshot: FsSnapshot,
    pub children: Vec<SyncbackSnapshot<'sync>>,
    pub removed_children: Vec<InstanceWithMeta<'sync>>,
}

pub fn get_best_middleware(snapshot: &SyncbackSnapshot) -> Middleware {
    // At some point, we're better off using an O(1) method for checking
    // equality for classes like this.
    static JSON_MODEL_CLASSES: OnceLock<HashSet<&str>> = OnceLock::new();
    let json_model_classes = JSON_MODEL_CLASSES.get_or_init(|| {
        [
            "Actor",
            "Sound",
            "SoundGroup",
            "Sky",
            "Atmosphere",
            "BloomEffect",
            "BlurEffect",
            "ColorCorrectionEffect",
            "DepthOfFieldEffect",
            "SunRaysEffect",
            "ParticleEmitter",
            "TextChannel",
            "TextChatCommand",
            // TODO: Implement a way to use inheritance for this
            "ChatWindowConfiguration",
            "ChatInputBarConfiguration",
            "BubbleChatConfiguration",
            "ChannelTabsConfiguration",
            "RemoteEvent",
            "UnreliableRemoteEvent",
            "RemoteFunction",
            "BindableEvent",
            "BindableFunction",
        ]
        .into()
    });

    let old_middleware = snapshot
        .old_inst()
        .and_then(|inst| inst.metadata().middleware);
    let inst = snapshot.new_inst();

    let mut middleware;

    if let Some(override_middleware) = snapshot.middleware {
        return override_middleware;
    } else if let Some(old_middleware) = old_middleware {
        return old_middleware;
    } else if json_model_classes.contains(inst.class.as_str()) {
        middleware = Middleware::JsonModel;
    } else {
        middleware = match inst.class.as_str() {
            "Folder" | "Configuration" | "Tool" => Middleware::Dir,
            "StringValue" => Middleware::Text,
            "Script" => Middleware::ServerScript,
            "LocalScript" => Middleware::ClientScript,
            "ModuleScript" => Middleware::ModuleScript,
            "LocalizationTable" => Middleware::Csv,
            // This isn't the ideal way to handle this but it works.
            name if name.ends_with("Value") => Middleware::JsonModel,
            _ => Middleware::Rbxm,
        }
    }

    if !inst.children().is_empty() {
        middleware = match middleware {
            Middleware::ServerScript => Middleware::ServerScriptDir,
            Middleware::ClientScript => Middleware::ClientScriptDir,
            Middleware::PluginScript => Middleware::PluginScriptDir,
            Middleware::ModuleScript => Middleware::ModuleScriptDir,
            Middleware::Csv => Middleware::CsvDir,
            Middleware::JsonModel | Middleware::Text => Middleware::Dir,
            _ => middleware,
        }
    }

    if middleware == Middleware::Rbxm {
        middleware = match env::var(DEBUG_MODEL_FORMAT_VAR) {
            Ok(value) if value == "1" => Middleware::Rbxmx,
            Ok(value) if value == "2" => Middleware::JsonModel,
            _ => Middleware::Rbxm,
        }
    }

    middleware
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SyncbackRules {
    /// A list of subtrees in a file that will be ignored by Syncback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ignore_trees: Vec<String>,
    /// A list of patterns to check against the path an Instance would serialize
    /// to. If a path matches one of these, the Instance won't be syncbacked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ignore_paths: Vec<String>,
    /// A map of classes to properties to ignore for that class when doing
    /// syncback.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    ignore_properties: IndexMap<Ustr, Vec<Ustr>>,
    /// Whether or not the `CurrentCamera` of `Workspace` is included in the
    /// syncback or not. Defaults to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_current_camera: Option<bool>,
    /// Whether or not to sync properties that cannot be modified via scripts.
    /// Defaults to `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_unscriptable: Option<bool>,
    /// Whether to skip serializing referent properties like `Model.PrimaryPart`
    /// during syncback. Defaults to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    ignore_referents: Option<bool>,
    /// Whether the globs specified in `ignore_paths` should be modified to also
    /// match directories. Defaults to `true`.
    ///
    /// If this is `true`, it'll take ignore globs that end in `/**` and convert
    /// them to also handle the directory they're referring to. This is
    /// generally a better UX.
    #[serde(skip_serializing_if = "Option::is_none")]
    create_ignore_dir_paths: Option<bool>,
}

impl SyncbackRules {
    pub fn compile_globs(&self) -> anyhow::Result<Vec<IgnorableGlob>> {
        let mut globs = Vec::with_capacity(self.ignore_paths.len());
        let dir_ignore_paths = self.create_ignore_dir_paths.unwrap_or(true);

        for pattern in &self.ignore_paths {
            let glob = IgnorableGlob::new(pattern)
                .with_context(|| format!("the pattern '{pattern}' is not a valid glob"))?;
            globs.push(glob);

            if dir_ignore_paths {
                if let Some(dir_pattern) = pattern.strip_suffix("/**") {
                    if let Ok(glob) = IgnorableGlob::new(dir_pattern) {
                        globs.push(glob)
                    }
                }
            }
        }

        Ok(globs)
    }
}

fn is_valid_path(globs: &Option<Vec<IgnorableGlob>>, base_path: &Path, path: &Path) -> bool {
    let git_glob = GIT_IGNORE_GLOB.get_or_init(|| Glob::new(".git/**").unwrap());
    let test_path = match path.strip_prefix(base_path) {
        Ok(suffix) => suffix,
        Err(_) => path,
    };
    if git_glob.is_match(test_path) {
        return false;
    }
    if let Some(ref ignore_paths) = globs {
        // Gitignore-style "last match wins"
        let mut ignored = false;
        for glob in ignore_paths {
            if glob.is_match(test_path) {
                ignored = !glob.is_negation();
            }
        }
        if ignored {
            return false;
        }
    }
    true
}

/// Returns a set of properties that should not be written with syncback if
/// one exists. This list is read directly from the Project and takes
/// inheritance into effect.
///
/// A legacy asset property and its `Content` replacement, such as
/// `Sound.SoundId` and `Sound.AudioContent`, count as one property, so listing
/// either name ignores both. Places only hold the `Content` name, and project
/// files may use either.
///
/// It **does not** handle properties that should not serialize for other
/// reasons, such as being defaults or being marked as not serializing in the
/// ReflectionDatabase.
fn get_property_filter(project: &Project, new_inst: &Instance) -> Option<UstrSet> {
    let filter = &project.syncback_rules.as_ref()?.ignore_properties;
    let mut set = UstrSet::default();

    let database = rbx_reflection_database::get().unwrap();
    let mut current_class_name = new_inst.class.as_str();

    loop {
        if let Some(list) = filter.get(&ustr(current_class_name)) {
            set.extend(list)
        }

        let class = database.classes.get(current_class_name)?;
        if let Some(super_class) = class.superclass {
            current_class_name = super_class;
        } else {
            break;
        }
    }

    let aliases: Vec<Ustr> = set
        .iter()
        .flat_map(|name| content_property_aliases(&new_inst.class, name))
        .collect();
    set.extend(aliases);

    Some(set)
}

/// Finds the instance in `old` at the same path as `new_ref` in `new`, if
/// every name along the path picks out exactly one child of the same class.
fn old_instance_at_same_path(new: &WeakDom, new_ref: Ref, old: &RojoTree) -> Option<Ref> {
    let mut path = Vec::new();
    let mut current = new.get_by_ref(new_ref)?;
    while current.parent().is_some() {
        path.push(current);
        current = new.get_by_ref(current.parent())?;
    }

    let old_dom = old.inner();
    let mut old_ref = old.get_root_id();
    for new_inst in path.into_iter().rev() {
        let mut matches = old_dom
            .get_by_ref(old_ref)?
            .children()
            .iter()
            .filter_map(|child| old_dom.get_by_ref(*child))
            .filter(|child| child.name == new_inst.name && child.class == new_inst.class);
        let found = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        old_ref = found.referent();
    }

    Some(old_ref)
}

/// Produces a list of descendants in the WeakDom such that all children come
/// before their parents.
fn descendants(dom: &WeakDom, root_ref: Ref) -> Vec<Ref> {
    let mut queue = VecDeque::new();
    let mut ordered = Vec::new();
    queue.push_front(root_ref);

    while let Some(referent) = queue.pop_front() {
        let inst = dom
            .get_by_ref(referent)
            .expect("Invariant: WeakDom had a Ref that wasn't inside it");
        ordered.push(referent);
        for child in inst.children() {
            queue.push_back(*child)
        }
    }

    ordered
}

/// Removes the children of `new`'s root that are not also children of `old`'s
/// root.
///
/// This does not care about duplicates, and only filters based on names and
/// class names.
fn strip_unknown_root_children(new: &mut WeakDom, old: &RojoTree) {
    let old_root = old.root();
    let old_root_children: HashMap<&str, InstanceWithMeta> = old_root
        .children()
        .iter()
        .map(|referent| {
            let inst = old
                .get_instance(*referent)
                .expect("all children of a DOM's root should exist");
            (inst.name(), inst)
        })
        .collect();

    let root_children = new.root().children().to_vec();

    for child_ref in root_children {
        let child = new
            .get_by_ref(child_ref)
            .expect("all children of the root should exist in the DOM");
        if let Some(old) = old_root_children.get(child.name.as_str()) {
            if old.class_name() == child.class {
                continue;
            }
        }
        log::trace!("Pruning root child {} of class {}", child.name, child.class);
        new.destroy(child_ref);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn rules(ignore_paths: &[&str], create_ignore_dir_paths: Option<bool>) -> SyncbackRules {
        SyncbackRules {
            ignore_trees: Vec::new(),
            ignore_paths: ignore_paths.iter().map(|s| s.to_string()).collect(),
            ignore_properties: IndexMap::new(),
            sync_current_camera: None,
            sync_unscriptable: None,
            ignore_referents: None,
            create_ignore_dir_paths,
        }
    }

    #[test]
    fn ignore_paths_negation() {
        let globs = Some(
            rules(&["**/*.lua", "!keep.lua"], Some(false))
                .compile_globs()
                .unwrap(),
        );
        let base = Path::new("/test");

        // A later negation re-includes a path matched by an earlier pattern.
        assert!(!is_valid_path(&globs, base, Path::new("/test/foo.lua")));
        assert!(is_valid_path(&globs, base, Path::new("/test/keep.lua")));
        // Paths matched by no rule are valid.
        assert!(is_valid_path(&globs, base, Path::new("/test/plain.txt")));
    }

    #[test]
    fn ignore_paths_negation_with_dir_expansion() {
        // With `create_ignore_dir_paths`, a negated `foo/**` pattern should also
        // re-include the `foo` directory itself, mirroring the file rule.
        let globs = Some(
            rules(&["**/*", "!keep/**"], Some(true))
                .compile_globs()
                .unwrap(),
        );
        let base = Path::new("/test");

        assert!(!is_valid_path(&globs, base, Path::new("/test/drop/a.lua")));
        assert!(is_valid_path(&globs, base, Path::new("/test/keep")));
        assert!(is_valid_path(&globs, base, Path::new("/test/keep/a.lua")));
    }

    #[test]
    fn ignore_paths_escaped_bang_is_literal() {
        // `\!literal.lua` should ignore a file literally named `!literal.lua`
        // rather than being parsed as a negation.
        let globs = Some(
            rules(&[r"\!literal.lua"], Some(false))
                .compile_globs()
                .unwrap(),
        );
        let base = Path::new("/test");

        assert!(!globs.as_ref().unwrap()[0].is_negation());
        assert!(!is_valid_path(
            &globs,
            base,
            Path::new("/test/!literal.lua")
        ));
    }
}
