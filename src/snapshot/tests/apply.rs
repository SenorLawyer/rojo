use insta::assert_yaml_snapshot;

use rbx_dom_weak::{
    types::{Attributes, Ref, Variant},
    ustr, UstrMap,
};
use rojo_insta_ext::RedactionMap;

use std::path::PathBuf;

use crate::{
    snapshot::{
        apply_patch_set, InstanceMetadata, InstanceSnapshot, PatchSet, PatchUpdate, RojoTree,
    },
    tree_view::{intern_tree, view_tree},
    RojoRef,
};

#[test]
fn set_name_and_class_name() {
    let mut redactions = RedactionMap::default();

    let mut tree = empty_tree();
    intern_tree(&tree, &mut redactions);

    let patch_set = PatchSet {
        updated_instances: vec![PatchUpdate {
            id: tree.get_root_id(),
            changed_name: Some("Hello, world!".to_owned()),
            changed_class_name: Some(ustr("Folder")),
            changed_properties: Default::default(),
            changed_metadata: None,
        }],
        ..Default::default()
    };

    let applied_patch_set = apply_patch_set(&mut tree, patch_set);

    let tree_view = view_tree(&tree, &mut redactions);
    assert_yaml_snapshot!(tree_view);

    let applied_patch_value = redactions.redacted_yaml(applied_patch_set);
    assert_yaml_snapshot!(applied_patch_value);
}

#[test]
fn add_property() {
    let mut redactions = RedactionMap::default();

    let mut tree = empty_tree();
    intern_tree(&tree, &mut redactions);

    let patch_set = PatchSet {
        updated_instances: vec![PatchUpdate {
            id: tree.get_root_id(),
            changed_name: None,
            changed_class_name: None,
            changed_properties: UstrMap::from_iter([(ustr("Foo"), Some("Value of Foo".into()))]),
            changed_metadata: None,
        }],
        ..Default::default()
    };

    let applied_patch_set = apply_patch_set(&mut tree, patch_set);

    let tree_view = view_tree(&tree, &mut redactions);
    assert_yaml_snapshot!(tree_view);

    let applied_patch_value = redactions.redacted_yaml(applied_patch_set);
    assert_yaml_snapshot!(applied_patch_value);
}

#[test]
fn remove_property() {
    let mut redactions = RedactionMap::default();

    let mut tree = empty_tree();
    intern_tree(&tree, &mut redactions);

    {
        let root_id = tree.get_root_id();
        let mut root_instance = tree.get_instance_mut(root_id).unwrap();

        root_instance
            .properties_mut()
            .insert(ustr("Foo"), "Should be removed".into());
    }

    let tree_view = view_tree(&tree, &mut redactions);
    assert_yaml_snapshot!("remove_property_initial", tree_view);

    let patch_set = PatchSet {
        updated_instances: vec![PatchUpdate {
            id: tree.get_root_id(),
            changed_name: None,
            changed_class_name: None,
            changed_properties: UstrMap::from_iter([(ustr("Foo"), None)]),
            changed_metadata: None,
        }],
        ..Default::default()
    };

    let applied_patch_set = apply_patch_set(&mut tree, patch_set);

    let tree_view = view_tree(&tree, &mut redactions);
    assert_yaml_snapshot!("remove_property_after_patch", tree_view);

    let applied_patch_value = redactions.redacted_yaml(applied_patch_set);
    assert_yaml_snapshot!("remove_property_appied_patch", applied_patch_value);
}

#[test]
fn changed_ignore_unknown_instances_is_set_when_it_changes() {
    let mut tree = empty_tree();
    let id = tree.get_root_id();

    let patch_set = PatchSet {
        updated_instances: vec![PatchUpdate {
            id,
            changed_name: None,
            changed_class_name: None,
            changed_properties: Default::default(),
            changed_metadata: Some(InstanceMetadata::new().ignore_unknown_instances(true)),
        }],
        ..Default::default()
    };

    let applied_patch_set = apply_patch_set(&mut tree, patch_set);
    let update = &applied_patch_set.updated[0];

    assert_eq!(update.changed_ignore_unknown_instances, Some(true));
    assert!(update.changed_metadata.is_some());
}

#[test]
fn changed_ignore_unknown_instances_is_unset_when_only_internal_metadata_changes() {
    let mut tree = empty_tree();
    let id = tree.get_root_id();

    let patch_set = PatchSet {
        updated_instances: vec![PatchUpdate {
            id,
            changed_name: None,
            changed_class_name: None,
            changed_properties: Default::default(),
            changed_metadata: Some(
                InstanceMetadata::new().relevant_paths(vec![PathBuf::from("some/new/path.lua")]),
            ),
        }],
        ..Default::default()
    };

    let applied_patch_set = apply_patch_set(&mut tree, patch_set);
    let update = &applied_patch_set.updated[0];

    assert_eq!(update.changed_ignore_unknown_instances, None);
    assert!(update.changed_metadata.is_some());
}

fn target(specified_id: &str) -> InstanceSnapshot {
    InstanceSnapshot::new()
        .name("target")
        .class_name("Folder")
        .metadata(InstanceMetadata::new().specified_id(Some(RojoRef::new(specified_id.into()))))
}

/// Builds a tree with an ObjectValue whose Value was resolved from a
/// `Rojo_Target_Value` attribute, as patch application does for attribute refs.
fn tree_with_pointer() -> (RojoTree, Ref, Ref) {
    let mut tree = empty_tree();
    let root_id = tree.get_root_id();
    let target_id = tree.insert_instance(root_id, target("the target"));

    let mut attributes = Attributes::new();
    attributes.insert("Rojo_Target_Value".into(), "the target".into());
    let pointer_id = tree.insert_instance(
        root_id,
        InstanceSnapshot::new()
            .name("pointer")
            .class_name("ObjectValue")
            .property(ustr("Attributes"), attributes)
            .property(ustr("Value"), Variant::Ref(target_id)),
    );

    (tree, target_id, pointer_id)
}

#[test]
fn removing_ref_target_clears_pointer() {
    let (mut tree, target_id, pointer_id) = tree_with_pointer();

    let applied_patch_set = apply_patch_set(
        &mut tree,
        PatchSet {
            removed_instances: vec![target_id],
            ..Default::default()
        },
    );

    let pointer = tree.get_instance(pointer_id).unwrap();
    assert!(!pointer.properties().contains_key(&ustr("Value")));

    assert_eq!(applied_patch_set.removed, vec![target_id]);
    assert_eq!(applied_patch_set.updated.len(), 1);
    let update = &applied_patch_set.updated[0];
    assert_eq!(update.id, pointer_id);
    // The plugin cannot see a msgpack nil inside a table, so the cleared ref
    // is reported as an explicit null Ref.
    assert_eq!(
        update.changed_properties.get(&ustr("Value")),
        Some(&Some(Variant::Ref(Ref::none())))
    );
}

#[test]
fn removing_ref_target_ignores_refs_without_pointer_attributes() {
    let (mut tree, target_id, _pointer_id) = tree_with_pointer();
    let root_id = tree.get_root_id();

    // A ref that did not come from a `Rojo_Target_*` attribute belongs to the
    // same model as its target and is updated when that model is
    // re-snapshotted, so the repair pass leaves it alone.
    let plain_id = tree.insert_instance(
        root_id,
        InstanceSnapshot::new()
            .name("plain")
            .class_name("ObjectValue")
            .property(ustr("Value"), Variant::Ref(target_id)),
    );

    let applied_patch_set = apply_patch_set(
        &mut tree,
        PatchSet {
            removed_instances: vec![target_id],
            ..Default::default()
        },
    );

    assert!(applied_patch_set
        .updated
        .iter()
        .all(|update| update.id != plain_id));
}

#[test]
fn replacing_ref_target_in_one_patch_repoints_pointer() {
    let (mut tree, target_id, pointer_id) = tree_with_pointer();
    let root_id = tree.get_root_id();

    let applied_patch_set = apply_patch_set(
        &mut tree,
        PatchSet {
            removed_instances: vec![target_id],
            added_instances: vec![crate::snapshot::PatchAdd {
                parent_id: root_id,
                instance: target("the target"),
            }],
            ..Default::default()
        },
    );

    let new_target_id = applied_patch_set.added[0];
    let pointer = tree.get_instance(pointer_id).unwrap();
    assert_eq!(
        pointer.properties().get(&ustr("Value")),
        Some(&Variant::Ref(new_target_id))
    );

    let update = &applied_patch_set.updated[0];
    assert_eq!(update.id, pointer_id);
    assert_eq!(
        update.changed_properties.get(&ustr("Value")),
        Some(&Some(Variant::Ref(new_target_id)))
    );
}

#[test]
fn re_adding_ref_target_in_later_patch_restores_pointer() {
    let (mut tree, target_id, pointer_id) = tree_with_pointer();
    let root_id = tree.get_root_id();

    // Branch switches and package reinstalls can deliver the removal and the
    // re-creation of a target as separate patches.
    apply_patch_set(
        &mut tree,
        PatchSet {
            removed_instances: vec![target_id],
            ..Default::default()
        },
    );
    let pointer = tree.get_instance(pointer_id).unwrap();
    assert!(!pointer.properties().contains_key(&ustr("Value")));

    let applied_patch_set = apply_patch_set(
        &mut tree,
        PatchSet {
            added_instances: vec![crate::snapshot::PatchAdd {
                parent_id: root_id,
                instance: target("the target"),
            }],
            ..Default::default()
        },
    );

    let new_target_id = applied_patch_set.added[0];
    let pointer = tree.get_instance(pointer_id).unwrap();
    assert_eq!(
        pointer.properties().get(&ustr("Value")),
        Some(&Variant::Ref(new_target_id))
    );

    assert_eq!(applied_patch_set.updated.len(), 1);
    let update = &applied_patch_set.updated[0];
    assert_eq!(update.id, pointer_id);
    assert_eq!(
        update.changed_properties.get(&ustr("Value")),
        Some(&Some(Variant::Ref(new_target_id)))
    );
}

#[test]
fn adding_unrelated_specified_id_leaves_pointers_alone() {
    let (mut tree, target_id, pointer_id) = tree_with_pointer();
    let root_id = tree.get_root_id();

    let applied_patch_set = apply_patch_set(
        &mut tree,
        PatchSet {
            added_instances: vec![crate::snapshot::PatchAdd {
                parent_id: root_id,
                instance: target("another target"),
            }],
            ..Default::default()
        },
    );

    assert!(applied_patch_set.updated.is_empty());
    let pointer = tree.get_instance(pointer_id).unwrap();
    assert_eq!(
        pointer.properties().get(&ustr("Value")),
        Some(&Variant::Ref(target_id))
    );
}

#[test]
fn removed_pointer_holders_are_forgotten() {
    let (mut tree, _target_id, pointer_id) = tree_with_pointer();
    assert_eq!(tree.ref_pointer_holders(), vec![pointer_id]);

    apply_patch_set(
        &mut tree,
        PatchSet {
            removed_instances: vec![pointer_id],
            ..Default::default()
        },
    );

    assert!(tree.ref_pointer_holders().is_empty());
}

fn empty_tree() -> RojoTree {
    RojoTree::new(InstanceSnapshot::new().name("ROOT").class_name("ROOT"))
}
