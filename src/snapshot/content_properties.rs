//! Roblox moved asset references from legacy properties, such as
//! `Sound.SoundId` (a `ContentId`) or `CharacterMesh.MeshId` (an `Int64`), to
//! `Content` properties, such as `Sound.AudioContent`. Studio saves the new
//! properties, while existing projects, models and scripts still use the old
//! ones, so a snapshot can contain either and sometimes both.
//!
//! rbx_binary and rbx_xml already move legacy properties to their `Content`
//! replacements when they read a file, but properties written in project and
//! JSON files are not read by them. Without this step an instance that sets
//! both names would be built with whichever value the file format happened to
//! write last, and Studio would be sent both values in an arbitrary order.
//! Moving every legacy property to its replacement here keeps the tree in the
//! same shape as a file that Studio saved, so build output, live sync, syncback
//! and the plugin's diff all agree on one value.

use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock},
};

use rbx_dom_weak::{
    types::{ContentType, Variant, VariantType},
    ustr, Ustr,
};
use rbx_reflection::{DataType, PropertyKind, PropertyMigration, PropertySerialization};

use super::InstanceSnapshot;

/// Moves legacy asset properties in `snapshot` and its descendants to their
/// `Content` replacements and removes `Content` values Rojo cannot represent.
pub fn normalize_content_properties(snapshot: &mut InstanceSnapshot) {
    let mut stack = vec![snapshot];

    while let Some(snapshot) = stack.pop() {
        normalize_instance(snapshot);
        stack.extend(snapshot.children.iter_mut());
    }
}

fn normalize_instance(snapshot: &mut InstanceSnapshot) {
    // Only ContentId, Int64 and Content values can be involved, so most
    // properties are skipped before the reflection database is consulted.
    let candidates: Vec<Ustr> = snapshot
        .properties
        .iter()
        .filter(|(_, value)| {
            matches!(
                value,
                Variant::ContentId(_) | Variant::Int64(_) | Variant::Content(_)
            )
        })
        .map(|(name, _)| *name)
        .collect();

    for name in candidates {
        if let Some(Variant::Content(content)) = snapshot.properties.get(&name) {
            if let ContentType::Object(_) = content.value() {
                // An object reference only means something inside the place
                // that holds the object. Rojo's tree, the XML format and the
                // plugin cannot carry it, so it is dropped rather than left
                // pointing at nothing.
                warn_once(format!(
                    "{} '{}' has a {} that refers to an object, which Rojo cannot sync. \
                     The property was left out.",
                    snapshot.class_name, snapshot.name, name
                ));
                snapshot.properties.remove(&name);
            }
            continue;
        }

        let Some(migration) = content_migration(&snapshot.class_name, &name) else {
            continue;
        };
        let Some(new_name) = migration.new_property_names().first().map(|s| ustr(s)) else {
            continue;
        };
        let legacy_value = snapshot.properties.remove(&name).unwrap();
        let migrated = match migration.perform(&legacy_value) {
            Ok(value) => value,
            Err(err) => {
                // A value of the wrong type is reported where the property is
                // used, as it was before; keep it as written.
                log::debug!(
                    "Could not move {}.{} to {}: {}",
                    snapshot.class_name,
                    name,
                    new_name,
                    err
                );
                snapshot.properties.insert(name, legacy_value);
                continue;
            }
        };

        match snapshot.properties.get(&new_name) {
            // Studio ignores a legacy property when its replacement is present
            // too, and so does rbx-dom when it reads a file.
            Some(existing) => {
                if existing != &migrated {
                    warn_once(format!(
                        "{} '{}' sets both {} and {} to different values. Using {}; \
                         remove {} to silence this warning.",
                        snapshot.class_name, snapshot.name, name, new_name, new_name, name
                    ));
                }
            }
            None => {
                snapshot.properties.insert(new_name, migrated);
            }
        }
    }
}

/// Logs each distinct warning once per process. Every file change snapshots
/// the affected instances again, which would otherwise repeat the warning
/// while `rojo serve` runs.
fn warn_once(message: String) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

    let mut seen = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.insert(message.clone()) {
        log::warn!("{message}");
    }
}

/// Returns the migration from a legacy property to a `Content` property, or
/// `None` for any other property. Other migrations, such as `Font` to
/// `FontFace`, are left to rbx-dom's serializers as before.
fn content_migration(
    class_name: &str,
    property_name: &str,
) -> Option<&'static PropertyMigration<'static>> {
    let database = rbx_reflection_database::get().ok()?;
    let class = database.classes.get(class_name)?;

    for class in database.superclasses_iter(class) {
        let Some(descriptor) = class.properties.get(property_name) else {
            continue;
        };
        let PropertyKind::Canonical {
            serialization: PropertySerialization::Migrate(migration),
        } = &descriptor.kind
        else {
            return None;
        };

        let targets_content = migration.new_property_names().iter().all(|new_name| {
            database
                .superclasses_iter(class)
                .find_map(|class| class.properties.get(*new_name))
                .is_some_and(|new| matches!(new.data_type, DataType::Value(VariantType::Content)))
        });

        return targets_content.then_some(migration);
    }

    None
}

#[cfg(test)]
mod test {
    use super::*;

    use rbx_dom_weak::types::{Content, ContentId};

    fn snapshot(class_name: &str, properties: Vec<(&str, Variant)>) -> InstanceSnapshot {
        InstanceSnapshot::new()
            .name("Test")
            .class_name(class_name)
            .properties(
                properties
                    .into_iter()
                    .map(|(name, value)| (ustr(name), value))
                    .collect::<rbx_dom_weak::UstrMap<_>>(),
            )
    }

    fn uri(value: &str) -> Variant {
        Content::from_uri(value).into()
    }

    #[test]
    fn moves_legacy_content_ids_to_content_properties() {
        let mut sound = snapshot(
            "Sound",
            vec![("SoundId", ContentId::from("rbxassetid://1").into())],
        );
        normalize_content_properties(&mut sound);

        assert_eq!(sound.properties.get(&ustr("SoundId")), None);
        assert_eq!(
            sound.properties.get(&ustr("AudioContent")),
            Some(&uri("rbxassetid://1"))
        );
    }

    #[test]
    fn follows_renamed_and_inherited_properties() {
        let cases = [
            ("ShirtGraphic", "Graphic", "TextureContent"),
            ("Sky", "SkyboxBk", "SkyboxBackContent"),
            ("SpecialMesh", "MeshId", "MeshContent"),
            ("Tool", "TextureId", "TextureContent"),
            (
                "WrapLayer",
                "TemporaryReferenceId",
                "TemporaryReferenceMeshContent",
            ),
            ("PackageLink", "PackageId", "PackageContent"),
            ("ImageButton", "HoverImage", "HoverImageContent"),
        ];

        for (class_name, legacy, new) in cases {
            let mut instance = snapshot(
                class_name,
                vec![(legacy, ContentId::from("rbxassetid://5").into())],
            );
            normalize_content_properties(&mut instance);

            assert_eq!(
                instance.properties.get(&ustr(new)),
                Some(&uri("rbxassetid://5")),
                "{class_name}.{legacy}"
            );
            assert_eq!(instance.properties.get(&ustr(legacy)), None);
        }
    }

    #[test]
    fn moves_int64_asset_ids_to_content_properties() {
        let mut mesh = snapshot(
            "CharacterMesh",
            vec![
                ("MeshId", Variant::Int64(123)),
                ("BaseTextureId", Variant::Int64(0)),
            ],
        );
        normalize_content_properties(&mut mesh);

        assert_eq!(
            mesh.properties.get(&ustr("MeshContent")),
            Some(&uri("rbxassetid://123"))
        );
        assert_eq!(
            mesh.properties.get(&ustr("BaseTextureContent")),
            Some(&Content::none().into())
        );
        assert_eq!(mesh.properties.get(&ustr("MeshId")), None);
    }

    #[test]
    fn empty_legacy_ids_become_no_content() {
        let mut decal = snapshot("Decal", vec![("Texture", ContentId::new().into())]);
        normalize_content_properties(&mut decal);

        assert_eq!(
            decal.properties.get(&ustr("TextureContent")),
            Some(&Content::none().into())
        );
    }

    #[test]
    fn the_content_property_wins_when_both_are_set() {
        let mut sound = snapshot(
            "Sound",
            vec![
                ("SoundId", ContentId::from("rbxassetid://1").into()),
                ("AudioContent", uri("rbxassetid://2")),
            ],
        );
        normalize_content_properties(&mut sound);

        assert_eq!(sound.properties.len(), 1);
        assert_eq!(
            sound.properties.get(&ustr("AudioContent")),
            Some(&uri("rbxassetid://2"))
        );
    }

    #[test]
    fn leaves_other_properties_and_migrations_alone() {
        let mut label = snapshot(
            "TextLabel",
            vec![
                ("Text", Variant::String("rbxassetid://1".into())),
                ("Font", rbx_dom_weak::types::Enum::from_u32(3).into()),
            ],
        );
        let before = label.properties.clone();
        normalize_content_properties(&mut label);

        assert_eq!(label.properties, before);

        // An Int64 that is not a legacy asset id stays as it is.
        let mut model = snapshot("Model", vec![("Unknown", Variant::Int64(5))]);
        normalize_content_properties(&mut model);
        assert_eq!(
            model.properties.get(&ustr("Unknown")),
            Some(&Variant::Int64(5))
        );
    }

    #[test]
    fn drops_content_that_refers_to_an_object() {
        let mut label = snapshot(
            "ImageLabel",
            vec![(
                "ImageContent",
                Content::from_referent(rbx_dom_weak::types::Ref::new()).into(),
            )],
        );
        normalize_content_properties(&mut label);

        assert!(label.properties.is_empty());
    }

    #[test]
    fn normalizes_descendants() {
        let mut parent = snapshot("Folder", Vec::new()).children(vec![snapshot(
            "Sound",
            vec![("SoundId", ContentId::from("rbxassetid://1").into())],
        )]);
        normalize_content_properties(&mut parent);

        assert_eq!(
            parent.children[0].properties.get(&ustr("AudioContent")),
            Some(&uri("rbxassetid://1"))
        );
    }
}
