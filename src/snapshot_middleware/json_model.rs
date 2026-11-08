use std::{borrow::Cow, path::Path, str};

use anyhow::Context;
use indexmap::IndexMap;
use memofs::Vfs;
use rbx_dom_weak::{
    types::{Attributes, Ref, Variant},
    HashMapExt as _, Ustr, UstrMap,
};
use serde::{Deserialize, Serialize};

use crate::{
    json,
    resolution::UnresolvedValue,
    snapshot::{InstanceContext, InstanceMetadata, InstanceSnapshot, InstigatingSource},
    snapshot_middleware::meta_file::UnknownFields,
    syncback::{filter_properties_preallocated, FsSnapshot, SyncbackReturn, SyncbackSnapshot},
    RojoRef,
};

pub fn snapshot_json_model(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let contents = vfs.read(path)?;
    let contents_str = str::from_utf8(&contents)
        .with_context(|| format!("File was not valid UTF-8: {}", path.display()))?;

    if contents_str.trim().is_empty() {
        return Ok(None);
    }

    let mut instance: JsonModel = json::from_str_with_context(contents_str, || {
        format!("File is not a valid JSON model: {}", path.display())
    })?;

    if let Some(top_level_name) = &instance.name {
        let new_name = format!("{}.model.json", top_level_name);

        log::warn!(
            "Model at path {} had a top-level Name field. \
            This field has been ignored since Rojo 6.0.\n\
            Consider removing this field and renaming the file to {}.",
            new_name,
            path.display()
        );
    }

    instance.name = Some(name.to_owned());

    let id = instance.id.take().map(RojoRef::new);
    let schema = instance.schema.take();

    let mut snapshot = instance
        .into_snapshot(path)
        .with_context(|| format!("Could not load JSON model: {}", path.display()))?;

    snapshot.metadata = snapshot
        .metadata
        .instigating_source(path)
        .relevant_paths(vec![vfs.canonicalize(path)?])
        .context(context)
        .specified_id(id)
        .schema(schema);

    Ok(Some(snapshot))
}

pub fn syncback_json_model<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let mut property_buffer = Vec::with_capacity(snapshot.new_inst().properties.len());

    // The root's own `ignoreUnknownInstances` is the file's only when the
    // file made it: a project node naming the file can set it instead.
    let root_flag_from_file =
        snapshot
            .old_inst()
            .is_some_and(|old_inst| match &old_inst.metadata().instigating_source {
                Some(InstigatingSource::Path(path)) => *path == snapshot.path,
                Some(InstigatingSource::ProjectNode { node, .. }) => {
                    node.ignore_unknown_instances.is_none()
                }
                None => false,
            });
    let mut model =
        json_model_from_pair(snapshot, &mut property_buffer, snapshot.new, snapshot.old);
    if !root_flag_from_file {
        model.ignore_unknown_instances = None;
    }
    // We don't need the name on the root, but we do for children.
    model.name = None;

    if let Some(old_inst) = snapshot.old_inst() {
        // TODO: Is it worth this being an Arc or Rc? I doubt that enough
        // schemas will ever exist in one project for it to matter, but it
        // could have a performance cost.
        model.schema = old_inst.metadata().schema.clone();
    }

    Ok(SyncbackReturn {
        fs_snapshot: FsSnapshot::new().with_added_file(
            &snapshot.path,
            serde_json::to_vec_pretty(&model).context("failed to serialize new JSON Model")?,
        ),
        children: Vec::new(),
        removed_children: Vec::new(),
    })
}

/// The model for `new`. `old` is the instance the file made for it, whose
/// `ignoreUnknownInstances` is kept, since Studio has no such setting to
/// send back.
fn json_model_from_pair<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
    prop_buffer: &mut Vec<(Ustr, &'sync Variant)>,
    new: Ref,
    old: Option<Ref>,
) -> JsonModel {
    let new_inst = snapshot
        .get_new_instance(new)
        .expect("all new referents passed to json_model_from_pair should exist");

    filter_properties_preallocated(snapshot.project(), new_inst, prop_buffer);

    let mut properties = IndexMap::new();
    let mut attributes = IndexMap::new();
    for (name, value) in prop_buffer.drain(..) {
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
            _ => {
                properties.insert(
                    name,
                    UnresolvedValue::from_variant(value.clone(), &new_inst.class, &name),
                );
            }
        }
    }

    let old_inst = old.and_then(|old| snapshot.get_old_instance(old));
    let mut children = Vec::with_capacity(new_inst.children().len());

    for new_child_ref in new_inst.children() {
        // Children are matched by name and class, and only when that is
        // unambiguous; nothing else in a model file tells them apart.
        let old_child = old_inst.as_ref().and_then(|old_inst| {
            let new_child = snapshot.get_new_instance(*new_child_ref)?;
            let mut matches = old_inst.children().iter().filter(|child| {
                snapshot.get_old_instance(**child).is_some_and(|child| {
                    child.name() == new_child.name && child.class_name() == new_child.class
                })
            });
            let found = *matches.next()?;
            matches.next().is_none().then_some(found)
        });
        children.push(json_model_from_pair(
            snapshot,
            prop_buffer,
            *new_child_ref,
            old_child,
        ))
    }

    JsonModel {
        name: Some(new_inst.name.clone()),
        class_name: new_inst.class,
        children,
        properties,
        attributes,
        id: None,
        schema: None,
        ignore_unknown_instances: old_inst
            .filter(|old_inst| old_inst.metadata().ignore_unknown_instances)
            .map(|_| true),
        unknown_fields: UnknownFields::default(),
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonModel {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    schema: Option<String>,

    #[serde(alias = "Name", skip_serializing_if = "Option::is_none")]
    name: Option<String>,

    // The `$` spellings are what project files use for the same fields, and
    // are accepted so that copying a node's fields into a model file works.
    #[serde(alias = "ClassName", alias = "$className")]
    class_name: Ustr,

    #[serde(alias = "$id", skip_serializing_if = "Option::is_none")]
    id: Option<String>,

    #[serde(
        alias = "Children",
        default = "Vec::new",
        skip_serializing_if = "Vec::is_empty"
    )]
    children: Vec<JsonModel>,

    #[serde(
        alias = "Properties",
        alias = "$properties",
        default,
        skip_serializing_if = "IndexMap::is_empty"
    )]
    properties: IndexMap<Ustr, UnresolvedValue>,

    #[serde(
        alias = "$attributes",
        default = "IndexMap::new",
        skip_serializing_if = "IndexMap::is_empty"
    )]
    attributes: IndexMap<String, UnresolvedValue>,

    /// Whether Studio keeps children of this instance that the file does not
    /// have, as `ignoreUnknownInstances` does in meta files and project
    /// nodes.
    #[serde(
        alias = "$ignoreUnknownInstances",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    ignore_unknown_instances: Option<bool>,

    #[serde(flatten, skip_serializing)]
    unknown_fields: UnknownFields,
}

impl JsonModel {
    fn into_snapshot(self, path: &Path) -> anyhow::Result<InstanceSnapshot> {
        self.unknown_fields.warn(path);

        let name = self.name.unwrap_or_else(|| self.class_name.to_owned());
        let class_name = self.class_name;

        let mut children = Vec::with_capacity(self.children.len());
        for child in self.children {
            children.push(child.into_snapshot(path)?);
        }

        let mut properties = UstrMap::with_capacity(self.properties.len());
        for (key, unresolved) in self.properties {
            let value = unresolved.resolve(&class_name, &key)?;
            properties.insert(key, value);
        }

        if !self.attributes.is_empty() {
            let mut attributes = Attributes::new();

            for (key, unresolved) in self.attributes {
                let value = unresolved.resolve_unambiguous()?;
                attributes.insert(key, value);
            }

            properties.insert("Attributes".into(), attributes.into());
        }

        // Children keep their IDs so `Rojo_Target_*` attributes, such as a
        // Model's PrimaryPart, can point at them. The top-level ID is taken
        // out before this and applied by the caller.
        let mut metadata = InstanceMetadata::new().specified_id(self.id.map(RojoRef::new));
        if let Some(ignore) = self.ignore_unknown_instances {
            metadata.ignore_unknown_instances = ignore;
        }

        Ok(InstanceSnapshot {
            snapshot_id: Ref::none(),
            metadata,
            name: Cow::Owned(name),
            class_name,
            properties,
            children,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};

    #[test]
    fn model_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.model.json",
            VfsSnapshot::file(
                r#"
                    {
                      "className": "IntValue",
                      "properties": {
                        "Value": 5
                      },
                      "children": [
                        {
                          "name": "The Child",
                          "className": "StringValue"
                        }
                      ]
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_json_model(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.model.json"),
            "foo",
        )
        .unwrap()
        .unwrap();

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    /// Project files spell these fields with a `$`. Model files used to drop
    /// that spelling silently, so Tags written that way never appeared.
    ///
    /// See: https://github.com/rojo-rbx/rojo/issues/1017
    #[test]
    fn model_accepts_project_spelling() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.model.json",
            VfsSnapshot::file(
                r#"{
                    "$className": "Folder",
                    "$properties": { "Tags": ["Hello", "World"] },
                    "$attributes": { "Speed": 5 },
                    "children": [
                        {
                            "name": "Child",
                            "className": "Part",
                            "$properties": { "Tags": ["Child"] },
                            "notAField": true
                        }
                    ]
                }"#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let snapshot = snapshot_json_model(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.model.json"),
            "foo",
        )
        .unwrap()
        .unwrap();

        let tags = |snapshot: &InstanceSnapshot| match snapshot
            .properties
            .get(&rbx_dom_weak::ustr("Tags"))
        {
            Some(Variant::Tags(tags)) => tags.iter().map(str::to_owned).collect::<Vec<_>>(),
            other => panic!("expected Tags, got {other:?}"),
        };
        assert_eq!(snapshot.class_name, "Folder");
        assert_eq!(tags(&snapshot), vec!["Hello", "World"]);
        assert!(snapshot
            .properties
            .contains_key(&rbx_dom_weak::ustr("Attributes")));
        assert_eq!(tags(&snapshot.children[0]), vec!["Child"]);
    }

    /// What syncback writes for `ignoreUnknownInstances` reads back the same,
    /// whichever spelling the file had.
    #[test]
    fn model_ignore_unknown_instances_round_trips() {
        let read = |text: &str| crate::json::from_str::<JsonModel>(text).unwrap();
        let model = read(
            r#"{"className": "Model", "$ignoreUnknownInstances": true,
                "children": [{"name": "Inner", "className": "Folder", "ignoreUnknownInstances": true}]}"#,
        );
        let written = serde_json::to_string(&model).unwrap();
        assert!(
            written.contains(r#""ignoreUnknownInstances":true"#),
            "{written}"
        );
        assert!(!written.contains("$ignoreUnknownInstances"), "{written}");
        let again = read(&written);
        assert_eq!(again.ignore_unknown_instances, Some(true));
        assert_eq!(again.children[0].ignore_unknown_instances, Some(true));
    }

    /// `ignoreUnknownInstances` (or `$ignoreUnknownInstances`) used to be
    /// reported as an unknown field and dropped.
    #[test]
    fn model_ignore_unknown_instances_is_applied() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.model.json",
            VfsSnapshot::file(
                r#"{
                    "className": "Model",
                    "$ignoreUnknownInstances": true,
                    "children": [
                        { "name": "Kept", "className": "Folder", "ignoreUnknownInstances": true },
                        { "name": "Plain", "className": "Folder" }
                    ]
                }"#,
            ),
        )
        .unwrap();
        let vfs = Vfs::new(imfs);
        let snapshot = snapshot_json_model(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.model.json"),
            "foo",
        )
        .unwrap()
        .unwrap();
        assert!(snapshot.metadata.ignore_unknown_instances);
        assert!(snapshot.children[0].metadata.ignore_unknown_instances);
        assert!(!snapshot.children[1].metadata.ignore_unknown_instances);
    }

    /// IDs on nested children used to be dropped, so a parent's
    /// `Rojo_Target_PrimaryPart` could never point at one of its own parts.
    #[test]
    fn model_children_keep_their_ids() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.model.json",
            VfsSnapshot::file(
                r#"{
                    "className": "Model",
                    "id": "model",
                    "attributes": { "Rojo_Target_PrimaryPart": "pole" },
                    "children": [
                        { "name": "Pole", "className": "Part", "$id": "pole" },
                        { "name": "Flag", "className": "Part", "id": "flag" },
                        { "name": "Plain", "className": "Part" }
                    ]
                }"#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);
        let snapshot = snapshot_json_model(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.model.json"),
            "foo",
        )
        .unwrap()
        .unwrap();

        let id = |snapshot: &InstanceSnapshot| {
            snapshot
                .metadata
                .specified_id
                .as_ref()
                .map(ToString::to_string)
        };
        assert_eq!(id(&snapshot).as_deref(), Some("model"));
        assert_eq!(id(&snapshot.children[0]).as_deref(), Some("pole"));
        assert_eq!(id(&snapshot.children[1]).as_deref(), Some("flag"));
        assert_eq!(id(&snapshot.children[2]), None);

        // Building the tree resolves the parent's pointer to the child.
        let mut tree = crate::snapshot::RojoTree::new(InstanceSnapshot::new());
        let root_id = tree.get_root_id();
        let patch = crate::snapshot::compute_patch_set(Some(snapshot), &tree, root_id);
        crate::snapshot::apply_patch_set(&mut tree, patch);
        let model = tree.get_instance(root_id).unwrap();
        let pole = tree
            .get_specified_id(&RojoRef::new("pole".to_owned()))
            .unwrap();
        assert_eq!(model.children()[0], pole);
        assert_eq!(
            model.properties().get(&rbx_dom_weak::ustr("PrimaryPart")),
            Some(&Variant::Ref(pole))
        );
    }

    #[test]
    fn model_from_vfs_legacy() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.model.json",
            VfsSnapshot::file(
                r#"
                    {
                      "ClassName": "IntValue",
                      "Properties": {
                        "Value": 5
                      },
                      "Children": [
                        {
                          "Name": "The Child",
                          "ClassName": "StringValue"
                        }
                      ]
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_json_model(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.model.json"),
            "foo",
        )
        .unwrap()
        .unwrap();

        insta::assert_yaml_snapshot!(instance_snapshot);
    }
}
