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

use anyhow::{bail, Context};
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, Ustr, WeakDom,
};
use rbx_reflection::ClassTag;

use crate::{
    snapshot::{apply_patch_set, compute_patch_set, InstanceSnapshot, PatchSet, RojoTree},
    RojoRef, REF_ID_ATTRIBUTE_NAME,
};

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
pub fn merge_into_place(project: &RojoTree, base: WeakDom) -> anyhow::Result<RojoTree> {
    let mut tree = RojoTree::from_dom(base);
    let root_id = tree.get_root_id();

    let snapshot = snapshot_from_tree(project, project.get_root_id());
    let mut patch_set = compute_patch_set(Some(snapshot), &tree, root_id);

    keep_unclaimed_base_content(&tree, &mut patch_set);
    replace_base_ids(&tree, &mut patch_set);
    let null_refs = take_null_ref_assignments(&mut patch_set);
    apply_patch_set(&mut tree, patch_set);

    for (id, name) in null_refs {
        tree.get_instance_mut(id)
            .unwrap()
            .properties_mut()
            .insert(name, Variant::Ref(Ref::none()));
    }

    check_base_id_collisions(project, &tree)?;
    clear_dangling_refs(&mut tree);

    Ok(tree)
}

/// Makes the project's ID win over a base `Rojo_Id` attribute on a paired
/// instance.
///
/// The project's ID only lives in metadata unless the project sets
/// attributes, so a paired instance keeps the base's attributes, and patch
/// application reads the specified ID back from a `Rojo_Id` among them. The
/// base's old ID would replace the project's, and `Rojo_Target_*` pointers at
/// the project's ID would not resolve.
fn replace_base_ids(tree: &RojoTree, patch_set: &mut PatchSet) {
    let attributes_key = ustr("Attributes");

    for update in &mut patch_set.updated_instances {
        let Some(specified) = update
            .changed_metadata
            .as_ref()
            .and_then(|metadata| metadata.specified_id.clone())
        else {
            continue;
        };
        if update.changed_properties.contains_key(&attributes_key) {
            // The project's attributes replace the base's entirely.
            continue;
        }
        let Some(Variant::Attributes(attributes)) = tree
            .get_instance(update.id)
            .and_then(|instance| instance.properties().get(&attributes_key))
        else {
            continue;
        };
        if attributes.get(REF_ID_ATTRIBUTE_NAME).is_none() {
            continue;
        }

        let mut attributes = attributes.clone();
        attributes.insert(
            REF_ID_ATTRIBUTE_NAME.to_owned(),
            Variant::String(specified.as_str().to_owned()),
        );
        update
            .changed_properties
            .insert(attributes_key, Some(Variant::Attributes(attributes)));
    }
}

/// Takes the assignments of nil to Ref properties out of `patch_set`.
///
/// Patch application skips nil refs in updates, which would keep the base's
/// target where the project explicitly has none. They are assigned after the
/// patch is applied instead.
fn take_null_ref_assignments(patch_set: &mut PatchSet) -> Vec<(Ref, Ustr)> {
    let mut assignments = Vec::new();
    for update in &mut patch_set.updated_instances {
        update.changed_properties.retain(|name, value| match value {
            Some(Variant::Ref(target)) if target.is_none() => {
                assignments.push((update.id, *name));
                false
            }
            _ => true,
        });
    }
    assignments
}

/// Fails when an instance carries a `Rojo_Id` from the base place that the
/// project also gives one of its instances.
///
/// Neither ID would resolve, so every `Rojo_Target_*` pointing at it would
/// silently be left unset.
fn check_base_id_collisions(project: &RojoTree, tree: &RojoTree) -> anyhow::Result<()> {
    fn count_ids(tree: &RojoTree) -> HashMap<&RojoRef, Vec<Ref>> {
        let mut counts: HashMap<&RojoRef, Vec<Ref>> = HashMap::new();
        for instance in tree.descendants(tree.get_root_id()) {
            if let Some(specified) = &instance.metadata().specified_id {
                counts.entry(specified).or_default().push(instance.id());
            }
        }
        counts
    }

    // Comparing counts leaves duplicate IDs within the project itself to the
    // usual warning, as in any build, and only catches the ones the base
    // place adds: kept base-only instances, and paired instances that keep a
    // base Rojo_Id the project uses elsewhere.
    let project_counts = count_ids(project);
    let merged_counts = count_ids(tree);
    let mut collisions = Vec::new();
    for (specified, project_instances) in &project_counts {
        let merged = merged_counts.get(specified).map_or(&[][..], Vec::as_slice);
        if merged.len() > project_instances.len() {
            let paths: Vec<String> = merged.iter().map(|&id| instance_path(tree, id)).collect();
            collisions.push(format!(
                "\"{}\" at {}",
                specified.as_str(),
                paths.join(" and ")
            ));
        }
    }
    collisions.sort();

    if !collisions.is_empty() {
        bail!(
            "The base place has instances whose Rojo_Id is also an ID in the project, \
             so references to those IDs could not be resolved: {}. \
             Remove or rename the Rojo_Id attribute in the base place, or change the project's ID.",
            collisions.join(", ")
        );
    }
    Ok(())
}

fn instance_path(tree: &RojoTree, id: Ref) -> String {
    let mut names = Vec::new();
    let mut current = id;
    while current != tree.get_root_id() {
        let instance = tree.get_instance(current).unwrap();
        names.push(instance.name().to_owned());
        current = instance.parent();
    }
    names.reverse();
    names.join("/")
}

/// Whether a top-level instance of this class is a service. A class the
/// reflection database does not know is assumed to be a service newer than
/// it, since keeping one by mistake is harmless and removing one is not.
fn is_service(class_name: &str) -> bool {
    match rbx_reflection_database::get()
        .ok()
        .and_then(|database| database.classes.get(class_name))
    {
        Some(descriptor) => descriptor.tags.contains(&ClassTag::Service),
        None => true,
    }
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
///   root's `$ignoreUnknownInstances`, since a place needs them. Other
///   top-level instances follow the root's setting like any child.
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
            let is_unclaimed_service = parent == root_id && is_service(&instance.class_name());
            let unclaimed = is_unclaimed_service || ignores_unknown(parent);
            unclaimed && !replaced
        })
        .collect();

    patch_set.removed_instances.retain(|id| !keep.contains(id));
}
