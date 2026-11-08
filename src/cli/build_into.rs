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
    fs,
    path::Path,
};

use anyhow::{bail, Context};
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, Ustr, WeakDom,
};
use rbx_reflection::{ClassTag, PropertyKind, PropertySerialization};

use crate::{
    snapshot::{apply_patch_set, compute_patch_set, InstanceSnapshot, PatchSet, RojoTree},
    RojoRef, REF_ID_ATTRIBUTE_NAME, REF_POINTER_ATTRIBUTE_PREFIX,
};

/// Reads the place that `rojo build --into` starts from, along with a digest
/// of the bytes it was read from.
pub fn read_base_place(path: &Path) -> anyhow::Result<(WeakDom, blake3::Hash)> {
    // Read once, so the digest is of exactly the bytes that were parsed.
    let bytes =
        fs::read(path).with_context(|| format!("Could not open base place {}", path.display()))?;
    let digest = blake3::hash(&bytes);
    Ok((parse_base_place(path, &bytes)?, digest))
}

/// Returns the digest of the file at `path`, or `None` if it does not exist.
pub fn file_digest(path: &Path) -> anyhow::Result<Option<blake3::Hash>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(blake3::hash(&bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("Could not read {} again", path.display()))
        }
    }
}

fn parse_base_place(path: &Path, bytes: &[u8]) -> anyhow::Result<WeakDom> {
    let extension = path.extension().and_then(|extension| extension.to_str());
    let reader = bytes;
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
    remove_base_property_aliases(&tree, &mut patch_set);

    // Instances whose attributes the project replaces keep none of the
    // base's, so only the others get the base's pointers back afterwards.
    let attributes_key = ustr("Attributes");
    let project_sets_attributes: HashSet<Ref> = patch_set
        .updated_instances
        .iter()
        .filter(|update| update.changed_properties.contains_key(&attributes_key))
        .map(|update| update.id)
        .collect();
    let base_pointers = take_base_ref_pointers(&mut tree);

    replace_base_ids(&tree, &mut patch_set);
    let null_refs = take_null_ref_assignments(&mut patch_set);
    apply_patch_set(&mut tree, patch_set);

    for (id, name) in null_refs {
        tree.get_instance_mut(id)
            .unwrap()
            .properties_mut()
            .insert(name, Variant::Ref(Ref::none()));
    }
    restore_base_ref_pointers(&mut tree, base_pointers, &project_sets_attributes);

    check_base_id_collisions(project, &tree)?;
    clear_dangling_refs(&mut tree);

    Ok(tree)
}

/// Takes the `Rojo_Target_*` attributes out of every base instance, returning
/// them so they can be put back after the patch is applied.
///
/// Patch application resolves the pointer attributes of every instance it
/// updates. A pointer the base place kept, for example from an earlier
/// `rojo build`, would then override a ref the project sets on a paired
/// instance, or a ref edited in Studio since. The base's pointers are kept as
/// data only: Ref properties stay what the project or the base has.
fn take_base_ref_pointers(tree: &mut RojoTree) -> Vec<(Ref, Vec<(String, Variant)>)> {
    let attributes_key = ustr("Attributes");
    let holders: Vec<Ref> = tree
        .inner()
        .descendants()
        .filter(|instance| match instance.properties.get(&attributes_key) {
            Some(Variant::Attributes(attributes)) => attributes
                .iter()
                .any(|(name, _)| name.starts_with(REF_POINTER_ATTRIBUTE_PREFIX)),
            _ => false,
        })
        .map(|instance| instance.referent())
        .collect();

    let mut taken = Vec::new();
    for id in holders {
        let mut instance = tree.get_instance_mut(id).unwrap();
        let Some(Variant::Attributes(attributes)) =
            instance.properties_mut().get_mut(&attributes_key)
        else {
            continue;
        };
        let pointers: Vec<(String, Variant)> = attributes
            .iter()
            .filter(|(name, _)| name.starts_with(REF_POINTER_ATTRIBUTE_PREFIX))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        for (name, _) in &pointers {
            attributes.remove(name.as_str());
        }
        taken.push((id, pointers));
    }
    taken
}

/// Puts back the pointer attributes `take_base_ref_pointers` took, except on
/// removed instances and on instances whose attributes the project replaced.
fn restore_base_ref_pointers(
    tree: &mut RojoTree,
    pointers: Vec<(Ref, Vec<(String, Variant)>)>,
    project_sets_attributes: &HashSet<Ref>,
) {
    let attributes_key = ustr("Attributes");
    for (id, entries) in pointers {
        if project_sets_attributes.contains(&id) {
            continue;
        }
        let Some(mut instance) = tree.get_instance_mut(id) else {
            continue;
        };
        if let Some(Variant::Attributes(attributes)) =
            instance.properties_mut().get_mut(&attributes_key)
        {
            for (name, value) in entries {
                attributes.insert(name, value);
            }
        }
    }
}

/// Removes base properties that are another name for, or the migrated form
/// of, a property the project sets.
///
/// A base place read from a file has `Color` where a project sets
/// `BrickColor`, or `FontFace` where it sets `Font`. Keeping the base's would
/// leave both on the instance, and the serializer writes the newer one, so
/// the project's value would be lost.
fn remove_base_property_aliases(tree: &RojoTree, patch_set: &mut PatchSet) {
    for update in &mut patch_set.updated_instances {
        let Some(instance) = tree.get_instance(update.id) else {
            continue;
        };
        let class_name = update
            .changed_class_name
            .unwrap_or_else(|| instance.class_name());
        let mut removals = Vec::new();
        for (name, value) in &update.changed_properties {
            if value.is_none() {
                continue;
            }
            for related in related_property_names(&class_name, name) {
                if !update.changed_properties.contains_key(&related)
                    && instance.properties().contains_key(&related)
                {
                    removals.push(related);
                }
            }
        }
        for name in removals {
            update.changed_properties.insert(name, None);
        }
    }
}

/// Returns the other names `property_name` of `class_name` goes by: the
/// canonical property of an alias, other aliases of the same property, and
/// properties one migrates to or from.
fn related_property_names(class_name: &str, property_name: &str) -> Vec<Ustr> {
    let Ok(database) = rbx_reflection_database::get() else {
        return Vec::new();
    };
    let Some(class) = database.classes.get(class_name) else {
        return Vec::new();
    };
    let properties: Vec<(&str, &PropertyKind)> = database
        .superclasses_iter(class)
        .flat_map(|class| {
            class
                .properties
                .iter()
                .map(|(name, descriptor)| (*name, &descriptor.kind))
        })
        .collect();

    let canonical = properties
        .iter()
        .find(|(name, _)| *name == property_name)
        .map(|(_, kind)| match kind {
            PropertyKind::Alias { alias_for } => *alias_for,
            _ => property_name,
        })
        .unwrap_or(property_name);

    // Names that are, or are migrated from or to, the canonical property.
    let mut group: HashSet<&str> = HashSet::from([canonical]);
    for (name, kind) in &properties {
        match kind {
            PropertyKind::Alias { alias_for } if *alias_for == canonical => {
                group.insert(name);
            }
            PropertyKind::Canonical {
                serialization: PropertySerialization::Migrate(migration),
            } => {
                let targets = migration.new_property_names();
                if *name == canonical {
                    group.extend(targets.iter().copied());
                } else if targets.contains(&canonical) {
                    group.insert(name);
                }
            }
            _ => {}
        }
    }

    group
        .into_iter()
        .filter(|name| *name != property_name)
        .map(ustr)
        .collect()
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
///   removed when its parent ignores no unknown instances. Where the parent
///   ignores them, it is kept even when it shares a name with a project
///   child, as the Studio plugin keeps it.
/// - Services are never removed for lacking a project node, whatever the
///   root's `$ignoreUnknownInstances`, since a place needs them. Other
///   top-level instances follow the root's setting like any child.
fn keep_unclaimed_base_content(tree: &RojoTree, patch_set: &mut PatchSet) {
    let root_id = tree.get_root_id();

    for update in &mut patch_set.updated_instances {
        update.changed_properties.retain(|_, value| value.is_some());
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
            let is_unclaimed_service = parent == root_id && is_service(&instance.class_name());
            is_unclaimed_service || ignores_unknown(parent)
        })
        .collect();

    patch_set.removed_instances.retain(|id| !keep.contains(id));
}
