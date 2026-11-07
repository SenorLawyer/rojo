//! Applies a Rojo project onto an existing place for `rojo build --into`.
//!
//! The merge reuses the patch machinery that `rojo serve` uses to keep its
//! tree in step with the filesystem: the project is turned into a snapshot,
//! a patch is computed against a tree built from the base place, and that
//! patch is applied. The patch is filtered first, because a serve patch makes
//! the tree match the project exactly, while a build into a place has to keep
//! everything the project does not claim. See docs/build-into.md for the
//! rules.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::BufReader,
    path::Path,
};

use anyhow::Context;
use rbx_dom_weak::{
    types::{Ref, Variant},
    WeakDom,
};

use crate::snapshot::{apply_patch_set, compute_patch_set, InstanceSnapshot, PatchSet, RojoTree};

/// Reads the place that `rojo build --into` starts from.
pub fn read_base_place(path: &Path) -> anyhow::Result<WeakDom> {
    let extension = path.extension().and_then(|extension| extension.to_str());
    let file = File::open(path)
        .with_context(|| format!("Could not open base place {}", path.display()))?;
    let reader = BufReader::new(file);
    match extension {
        Some("rbxl") => rbx_binary::from_reader(reader)
            .with_context(|| format!("Could not read {} as a binary place file", path.display())),
        Some("rbxlx") => rbx_xml::from_reader(
            reader,
            rbx_xml::DecodeOptions::new()
                .property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown),
        )
        .with_context(|| format!("Could not read {} as an XML place file", path.display())),
        _ => anyhow::bail!(
            "The base place {} must be a place file ending in .rbxl or .rbxlx",
            path.display()
        ),
    }
}

/// Applies the project in `project` onto `base`, returning the merged tree.
///
/// `project` is expected to have a DataModel root, matching `base`'s root.
pub fn merge_into_place(project: &RojoTree, base: WeakDom) -> RojoTree {
    let mut tree = RojoTree::from_dom(base);
    let root_id = tree.get_root_id();

    let snapshot = snapshot_from_tree(project, project.get_root_id());
    let mut patch_set = compute_patch_set(Some(snapshot), &tree, root_id);
    keep_unclaimed_base_content(&tree, &mut patch_set);
    apply_patch_set(&mut tree, patch_set);
    clear_dangling_refs(&mut tree);

    tree
}

/// Sets refs to instances that are no longer in the tree to nil.
///
/// A base instance can point at one the project removed or replaced. The XML
/// serializer would write the removed instance's referent, which names no
/// item in the file, so the ref is cleared here for both formats alike.
fn clear_dangling_refs(tree: &mut RojoTree) {
    let mut dangling = Vec::new();
    for instance in tree.inner().descendants() {
        for (name, value) in &instance.properties {
            if let Variant::Ref(target) = value {
                if target.is_some() && tree.inner().get_by_ref(*target).is_none() {
                    dangling.push((instance.referent(), *name));
                }
            }
        }
    }
    for (id, name) in dangling {
        tree.get_instance_mut(id)
            .unwrap()
            .properties_mut()
            .insert(name, Variant::Ref(Ref::none()));
    }
}

/// Copies a tree back into a snapshot, metadata included.
///
/// Each snapshot ID is the instance's ID in `tree`, which is what the tree's
/// Ref properties point at, so patch computation and application can rewrite
/// them to the IDs the instances end up with in the base place.
fn snapshot_from_tree(tree: &RojoTree, id: Ref) -> InstanceSnapshot {
    let instance = tree.get_instance(id).unwrap();
    InstanceSnapshot {
        snapshot_id: id,
        metadata: instance.metadata().clone(),
        name: instance.name().to_owned().into(),
        class_name: instance.class_name(),
        properties: instance.properties().clone(),
        children: instance
            .children()
            .iter()
            .map(|&child| snapshot_from_tree(tree, child))
            .collect(),
    }
}

/// Removes the parts of `patch_set` that would delete base content the
/// project does not claim.
///
/// A serve patch removes every property and child the snapshot lacks. Here:
///
/// - Properties the project does not set keep the base value.
/// - A base child the project did not pair up (by name and class) is only
///   removed when its parent ignores no unknown instances, or when the
///   project defines a child with the same name there, which replaces it.
/// - Services are never removed for lacking a project node, whatever the
///   root's `$ignoreUnknownInstances`, since a place needs them.
fn keep_unclaimed_base_content(tree: &RojoTree, patch_set: &mut PatchSet) {
    let root_id = tree.get_root_id();

    for update in &mut patch_set.updated_instances {
        update.changed_properties.retain(|_, value| value.is_some());
    }

    // Patch computation removes every base child it could not pair with a
    // project child, so the base instances left are exactly the ones paired
    // with a project instance.
    let removed: HashSet<Ref> = patch_set.removed_instances.iter().copied().collect();

    // The names the project defines under each paired parent: paired
    // children, which keep their name, plus the children being added.
    let mut project_names: HashMap<Ref, HashSet<&str>> = HashMap::new();
    for addition in &patch_set.added_instances {
        project_names
            .entry(addition.parent_id)
            .or_default()
            .insert(addition.instance.name.as_ref());
    }
    let parents: HashSet<Ref> = patch_set
        .removed_instances
        .iter()
        .filter_map(|&id| tree.get_instance(id).map(|instance| instance.parent()))
        .collect();
    for &parent in &parents {
        let names = project_names.entry(parent).or_default();
        for &child in tree.get_instance(parent).unwrap().children() {
            if !removed.contains(&child) {
                names.insert(tree.get_instance(child).unwrap().name());
            }
        }
    }

    // A paired instance only gets a metadata update when the project's
    // metadata differs from the base's; otherwise they are equal and the
    // base's value is the project's.
    let ignores_unknown = |id: Ref| {
        patch_set
            .updated_instances
            .iter()
            .find(|update| update.id == id)
            .and_then(|update| update.changed_metadata.as_ref())
            .unwrap_or_else(|| tree.get_metadata(id).unwrap())
            .ignore_unknown_instances
    };

    let keep: HashSet<Ref> = patch_set
        .removed_instances
        .iter()
        .copied()
        .filter(|&id| {
            let instance = tree.get_instance(id).unwrap();
            let parent = instance.parent();
            let replaced = project_names
                .get(&parent)
                .is_some_and(|names| names.contains(instance.name()));
            let unclaimed = parent == root_id || ignores_unknown(parent);
            unclaimed && !replaced
        })
        .collect();

    patch_set.removed_instances.retain(|id| !keep.contains(id));
}
