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
    collections::{HashMap, HashSet},
    sync::{Mutex, OnceLock},
};

use rbx_dom_weak::{
    types::{ContentType, Variant, VariantType},
    ustr, Ustr, UstrMap,
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
    // Object references are removed before any legacy property is moved, so
    // that a legacy value is never discarded in favour of a `Content` value
    // that is about to be dropped. Doing both in one pass would make the
    // result depend on the property map's iteration order.
    for name in remove_object_contents(&mut snapshot.properties) {
        // An object reference only means something inside the place that
        // holds the object. Rojo's tree, the XML format and the plugin cannot
        // carry it, so it is dropped rather than left pointing at nothing.
        warn_once(object_content_warning(
            &snapshot.class_name,
            &snapshot.name,
            &name,
        ));
    }

    // Only ContentId and Int64 values can be legacy asset properties, so most
    // properties are skipped before the reflection database is consulted.
    let candidates: Vec<Ustr> = snapshot
        .properties
        .iter()
        .filter(|(_, value)| matches!(value, Variant::ContentId(_) | Variant::Int64(_)))
        .map(|(name, _)| *name)
        .collect();

    for name in candidates {
        let Some(legacy_value) = snapshot.properties.get(&name) else {
            continue;
        };
        let Some((new_name, migrated)) =
            migrate_content_property(&snapshot.class_name, &name, legacy_value)
        else {
            continue;
        };
        snapshot.properties.remove(&name);

        match snapshot.properties.get(&new_name) {
            // Studio ignores a legacy property when its replacement is present
            // too, and so does rbx-dom when it reads a file.
            Some(existing) => {
                if existing != &migrated {
                    warn_once(conflict_warning(
                        &snapshot.class_name,
                        &snapshot.name,
                        &name,
                        &new_name,
                    ));
                }
            }
            None => {
                snapshot.properties.insert(new_name, migrated);
            }
        }
    }
}

/// Removes every `Content` value in `properties` that refers to an object,
/// and returns the names of the removed properties.
pub fn remove_object_contents(properties: &mut UstrMap<Variant>) -> Vec<Ustr> {
    let names: Vec<Ustr> = properties
        .iter()
        .filter(|(_, value)| {
            matches!(value, Variant::Content(content) if matches!(content.value(), ContentType::Object(_)))
        })
        .map(|(name, _)| *name)
        .collect();

    for name in &names {
        properties.remove(name);
    }
    names
}

/// The warning for a `Content` property that refers to an object.
pub fn object_content_warning(class_name: &str, name: &str, property_name: &str) -> String {
    format!(
        "{class_name} '{name}': {property_name} refers to an object, which Rojo cannot sync. \
         The property was left out."
    )
}

fn conflict_warning(class_name: &str, name: &str, legacy: &str, new: &str) -> String {
    format!(
        "{class_name} '{name}' sets both {legacy} and {new} to different values. \
         Using {new}; remove {legacy} to silence this warning."
    )
}

/// Alias lists already worked out, per class and property name.
type AliasCache = HashMap<(Ustr, Ustr), Vec<Ustr>>;

/// Returns the other names of the asset property `property_name` of
/// `class_name`: the `Content` replacement of a legacy property, or the legacy
/// properties that a `Content` property replaced. Returns nothing for any
/// other property.
pub fn content_property_aliases(class_name: &str, property_name: &str) -> Vec<Ustr> {
    // Syncback asks for every ignored name on every instance, and project
    // loading for every `$properties` entry, so answers are kept per class
    // and property instead of walking the class's properties each time.
    static CACHE: OnceLock<Mutex<AliasCache>> = OnceLock::new();

    let key = (ustr(class_name), ustr(property_name));
    let cache = CACHE.get_or_init(Default::default);
    if let Some(aliases) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
    {
        return aliases.clone();
    }
    let aliases = find_content_property_aliases(class_name, property_name);
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, aliases.clone());
    aliases
}

fn find_content_property_aliases(class_name: &str, property_name: &str) -> Vec<Ustr> {
    let mut aliases = Vec::new();
    if let Some(migration) = content_migration(class_name, property_name) {
        aliases.extend(migration.new_property_names().iter().map(|name| ustr(name)));
    }

    let Ok(database) = rbx_reflection_database::get() else {
        return aliases;
    };
    let Some(class) = database.classes.get(class_name) else {
        return aliases;
    };
    for class in database.superclasses_iter(class) {
        for (legacy_name, descriptor) in &class.properties {
            if let PropertyKind::Canonical {
                serialization: PropertySerialization::Migrate(migration),
            } = &descriptor.kind
            {
                if migration.new_property_names().contains(&property_name)
                    && content_migration(class_name, legacy_name).is_some()
                {
                    aliases.push(ustr(legacy_name));
                }
            }
        }
    }

    aliases
}

/// If `property_name` is a legacy asset property of `class_name` that Roblox
/// replaced with a `Content` property, returns the replacement's name and
/// `value` converted to it. Returns `None` for any other property, and for a
/// value of the wrong type, which is then reported where the property is used
/// as it was before.
pub fn migrate_content_property(
    class_name: &str,
    property_name: &str,
    value: &Variant,
) -> Option<(Ustr, Variant)> {
    let migration = content_migration(class_name, property_name)?;
    let new_name = ustr(migration.new_property_names().first()?);

    match migration.perform(value) {
        Ok(migrated) => Some((new_name, migrated)),
        Err(err) => {
            log::debug!("Could not move {class_name}.{property_name} to {new_name}: {err}");
            None
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
    fn a_legacy_value_outlives_an_object_content() {
        // The Object-sourced value is dropped first, so the legacy URI always
        // ends up in the Content property whatever order the map yields.
        for _ in 0..16 {
            let mut label = snapshot(
                "ImageLabel",
                vec![
                    ("Image", ContentId::from("rbxassetid://7").into()),
                    (
                        "ImageContent",
                        Content::from_referent(rbx_dom_weak::types::Ref::new()).into(),
                    ),
                ],
            );
            normalize_content_properties(&mut label);

            assert_eq!(label.properties.len(), 1);
            assert_eq!(
                label.properties.get(&ustr("ImageContent")),
                Some(&uri("rbxassetid://7"))
            );
        }
    }

    #[test]
    fn migrates_single_legacy_values() {
        assert_eq!(
            migrate_content_property(
                "Sound",
                "SoundId",
                &ContentId::from("rbxassetid://1").into()
            ),
            Some((ustr("AudioContent"), uri("rbxassetid://1")))
        );
        assert_eq!(
            migrate_content_property("CharacterMesh", "MeshId", &Variant::Int64(9)),
            Some((ustr("MeshContent"), uri("rbxassetid://9")))
        );
        assert_eq!(
            migrate_content_property("Sound", "Volume", &Variant::Float32(1.0)),
            None
        );
    }

    #[test]
    fn finds_aliases_in_both_directions() {
        assert_eq!(
            content_property_aliases("Sound", "SoundId"),
            vec![ustr("AudioContent")]
        );
        assert_eq!(
            content_property_aliases("Sound", "AudioContent"),
            vec![ustr("SoundId")]
        );
        // Tool inherits TextureId from BackpackItem.
        assert_eq!(
            content_property_aliases("ImageButton", "HoverImageContent"),
            vec![ustr("HoverImage")]
        );
        assert_eq!(
            content_property_aliases("Tool", "TextureId"),
            vec![ustr("TextureContent")]
        );
        assert!(content_property_aliases("Sound", "Volume").is_empty());
        assert!(content_property_aliases("TextLabel", "FontFace").is_empty());
    }

    #[test]
    fn warnings_read_as_one_sentence_each() {
        assert_eq!(
            object_content_warning("ImageLabel", "Icon", "ImageContent"),
            "ImageLabel 'Icon': ImageContent refers to an object, which Rojo cannot sync. The property was left out."
        );
        assert_eq!(
            conflict_warning("Sound", "Music", "SoundId", "AudioContent"),
            "Sound 'Music' sets both SoundId and AudioContent to different values. Using AudioContent; remove SoundId to silence this warning."
        );
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
