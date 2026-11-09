use std::ffi::OsStr;

use insta::assert_snapshot;

use crate::rojo_test::syncback_util::{run_syncback_test, snapshot_rbxm};

macro_rules! syncback_tests {
    ($($test_name:ident => $list:expr$(,)?),*) => {$(
        #[test]
        fn $test_name() {
            run_syncback_test(stringify!($test_name), |path| {
                for name in $list {
                    let snapshot_name = format!(concat!(stringify!($test_name), "-{}"), name);
                    let new = path.join::<&str>(name);
                    if !new.exists() {
                        panic!("the path stub '{}' does not exist after syncback runs. consider double checking for typos.", name);
                    }
                    if let Some("rbxm") = new.extension().and_then(OsStr::to_str) {
                        let content = fs_err::read(new).unwrap();
                        snapshot_rbxm(&snapshot_name, content, name);
                    } else {
                        let content = read_lf(new).unwrap();
                        assert_snapshot!(snapshot_name, content, name);
                    }
                }
            });
        }
    )*};
}

syncback_tests! {
    // Ensures that there's only one copy written to disk if navigating a
    // project file might yield two copies
    child_but_not => ["OnlyOneCopy/child_of_one.luau", "ReplicatedStorage/child_replicated_storage.luau"],
    // Ensures that a place saved with Studio's Content asset properties leaves
    // files using the legacy names alone while their values match, and writes
    // the new names when a value changes or an instance is added
    content_properties => ["default.project.json", "unchanged.project.json", "src/Unchanged.model.json", "src/Changed.model.json", "src/Cleared.model.json", "src/AlreadyNew.model.json", "src/Added.model.json", "src/AddedEmpty.model.json"],
    // Ensures that with `syncUnscriptable` off, entries for unscriptable
    // asset properties are kept rather than taken for values reset to default
    content_properties_unscriptable => ["default.project.json"],
    // Ensures that ignoring a Content property also leaves entries that use
    // its legacy name alone, rather than removing them as reset to default
    content_properties_ignored => ["default.project.json", "unchanged.project.json", "src/Ignored.model.json"],
    // Ensures that ignoring a legacy asset property also ignores the Content
    // property that places hold in its place
    content_properties_ignored_legacy => ["default.project.json", "unchanged.project.json", "src/Ignored.model.json"],
    // Ensures that a Content property that refers to an object is neither
    // written nor allowed to remove the asset files set
    content_properties_object => ["default.project.json", "unchanged.project.json", "src/Label.model.json"],
    // Ensures that syncback works with CSVs
    csv => ["src/csv_init/init.csv", "src/csv.csv"],
    // Ensures that the children of a directory with an `init.data.csv` are
    // synced back, while the data CSV files themselves are left as they are
    data_csv_init => ["src/Items.data.csv", "src/Levels/init.data.csv", "src/Levels/Helper.luau", "src/Levels/Added.luau"],
    // Ensures that if a RojoId is duplicated somewhere in the project, it's
    // rewritten rather than synced back as a conflict
    duplicate_rojo_id => ["container.model.json"],
    // Ensures that the `ignorePaths` setting works for additions
    ignore_paths_adding => ["src/int_value.model.json", "src/subfolder/string_value.txt"],
    // Ensures that the `ignorePaths` setting works for `init` files
    ignore_paths_init => ["src/non-init.luau", "src/init-file/init.luau"],
    // Ensures that the `ignorePaths` setting works for removals
    ignore_paths_removing => ["src/Message.rbxm"],
    // Ensures that `ignoreTrees` works for additions
    ignore_trees_adding => [],
    // Ensures that `ignoreTrees` works for removals
    ignore_trees_removing => [],
    // Ensures that all of the JSON middlewares are handled as expected
    json_middlewares => ["src/dir_with_meta/init.meta.json", "src/model_json.model.json", "src/project_json.project.json"],
    // Ensures projects that refer to other projects work as expected
    nested_projects => ["nested.project.json", "string_value.txt"],
    // Ensures files that are ignored by nested projects are picked up if
    // they're included in second project. Unusual but perfectly workable
    // pattern that syncback has to support.
    nested_projects_weird => ["src/modules/ClientModule.luau", "src/modules/ServerModule.luau"],
    // Ensures that projects respect `init` files when they're directly referenced from a node
    project_init => ["src/init.luau"],
    // Ensures that projects can be reserialized by syncback and that
    // default.project.json doesn't change unexpectedly.
    project_reserialize => ["attribute_mismatch.luau", "property_mismatch.project.json"],
    // Confirms that Instances that cannot serialize as directories serialize as rbxms
    rbxm_fallback => ["src/ChildWithDuplicates.rbxm"],
    // Ensures that ref properties are linked properly on the file system
    ref_properties => ["src/pointer.model.json", "src/target.model.json"],
    // Ensures that ref properties are linked when no attributes are manually
    // set in the DataModel
    ref_properties_blank => ["src/pointer.model.json", "src/target.meta.json", "src/target.txt"],
    // Ensures that if there is a conflict in RojoRefs, one of them is rewritten.
    ref_properties_conflict => ["src/Pointer_2.model.json", "src/Target_2.model.json"],
    // Ensures that having multiple pointers that are aimed at the same target doesn't trigger ref rewrites.
    ref_properties_duplicate => [],
    // Ensures that ref properties that point to nothing after the prune both
    // do not leave any trace of themselves
    ref_properties_pruned => ["src/Pointer1.model.json", "src/Pointer2.model.json", "src/Pointer3.model.json"],
    // Ensures that the old middleware is respected during syncback
    respect_old_middleware => ["default.project.json", "src/model_json.model.json", "src/rbxm.rbxm", "src/rbxmx.rbxmx"],
    // Ensures that the `$schema` field roundtrips with syncback
    schema_roundtrip => ["default.project.json", "src/model.model.json", "src/init/init.meta.json", "src/adjacent.meta.json"],
    // Ensures that StringValues inside project files are written to the
    // project file, but only if they don't have `$path` set
    string_value_project => ["default.project.json"],
    // Ensures that sync rules are respected. This is really just a test to
    // ensure it uses the old path when possible, but we want the coverage.
    sync_rules => ["src/module.modulescript", "src/text.text"],
    // Ensures that the `syncUnscriptable` setting works
    unscriptable_properties => ["default.project.json"],
    // Ensures that syncback correctly removes default values from projects rather
    // than leaving an incorrect value.
    project_default_properties_remove => ["default.project.json"],
}

/// Syncback leaves a partition with `$globIgnorePaths` alone, since the files
/// those globs exclude are not in the tree it compares. A sibling partition
/// without them is synced back as usual.
#[test]
fn partition_ignore_globs() {
    run_syncback_test("partition_ignore_globs", |path| {
        // Nothing is written, added or removed under the partition.
        assert_eq!(
            read_lf(path.join("filtered/Foo.luau")).unwrap(),
            "return \"Foo\"\n"
        );
        assert!(!path.join("filtered/Foo.spec.luau").exists());
        assert!(path.join("filtered/Old.luau").exists());

        assert!(!path.join("unfiltered/Old.luau").exists());
        for name in ["unfiltered/Foo.luau", "unfiltered/Foo.spec.luau"] {
            let content = read_lf(path.join(name)).unwrap();
            assert_snapshot!(format!("partition_ignore_globs-{name}"), content, name);
        }
    });
}

/// A project's root node is a partition too, although its instance is tagged
/// with the project file. Studio changed, added, deleted and replaced files
/// under it, and none of that is written.
#[test]
fn partition_ignore_globs_root() {
    run_syncback_test("partition_ignore_globs_root", |path| {
        let src = path.join("src");
        for (name, content) in [
            ("Foo.luau", "return \"Foo\"\n"),
            ("Foo.spec.luau", "return \"Foo.spec on disk\"\n"),
            ("Nested/Bar.luau", "return \"Bar\"\n"),
            ("Nested/Bar.spec.luau", "return \"Bar.spec on disk\"\n"),
            ("Dup/A.luau", "return \"A\"\n"),
            ("Dup/A.spec.luau", "return \"A.spec on disk\"\n"),
            ("Gone/Old.luau", "return \"Old\"\n"),
        ] {
            assert_eq!(read_lf(src.join(name)).unwrap(), content, "{name}");
        }
        assert!(!src.join("New.spec.luau").exists());
        assert!(!src.join("Dup.rbxm").exists());
    });
}

/// A directory that holds a partition is not replaced by a model file when
/// it cannot be synced back as a directory, which would delete the partition
/// with it. The rest of the tree is still synced back.
#[test]
fn partition_ignore_globs_holder() {
    run_syncback_test("partition_ignore_globs_holder", |path| {
        let holder = path.join("other/Holder");
        assert!(!path.join("other/Holder.rbxm").exists());
        for (name, content) in [
            ("pkg/src/Foo.luau", "return \"Foo\"\n"),
            ("pkg/src/Foo.spec.luau", "return \"Foo.spec\"\n"),
        ] {
            assert_eq!(read_lf(holder.join(name)).unwrap(), content, "{name}");
        }
        let content = read_lf(path.join("other/Plain.luau")).unwrap();
        assert_snapshot!(
            "partition_ignore_globs_holder-other/Plain.luau",
            content,
            "other/Plain.luau"
        );
    });
}

/// Studio has two children with the same name inside a partition with
/// `$globIgnorePaths`, which syncback would refuse in a project node. The
/// partition is skipped before anything checks it, so the rest still syncs.
#[test]
fn partition_ignore_globs_dup() {
    run_syncback_test("partition_ignore_globs_dup", |path| {
        assert_eq!(
            read_lf(path.join("filtered/Foo.luau")).unwrap(),
            "return \"Foo\"\n"
        );
        assert!(path.join("filtered/Foo.spec.luau").exists());
        assert!(!path.join("filtered/Dup").exists());
        assert!(!path.join("filtered/Dup.model.json").exists());
        assert_eq!(
            read_lf(path.join("unfiltered/Foo.luau")).unwrap(),
            "return \"Foo from Studio\"\n"
        );
    });
}

/// Another node reaches the partition's folder as `src/../filtered`. It is
/// the same folder on disk, so it is protected too.
#[test]
fn partition_ignore_globs_alias() {
    run_syncback_test("partition_ignore_globs_alias", |path| {
        assert_eq!(
            read_lf(path.join("filtered/Foo.luau")).unwrap(),
            "return \"Foo\"\n"
        );
        assert!(path.join("filtered/Foo.spec.luau").exists());
        assert!(!path.join("filtered/New.luau").exists());
        assert_eq!(
            read_lf(path.join("unfiltered/Foo.luau")).unwrap(),
            "return \"Foo from Studio\"\n"
        );
    });
}

/// A ref from a synced partition into one syncback leaves alone. A target
/// whose files have no ID is not given one, since that ID would never be
/// written, so its pointer keeps what its file had (here nothing) instead of
/// naming it. A target whose files have an ID is linked as usual.
#[test]
fn partition_ignore_globs_ref() {
    run_syncback_test("partition_ignore_globs_ref", |path| {
        assert_eq!(
            read_lf(path.join("filtered/Target.model.json")).unwrap(),
            "{\n  \"className\": \"Folder\"\n}\n"
        );
        let pointer = read_lf(path.join("unfiltered/Pointer.model.json")).unwrap();
        assert!(!pointer.contains("Rojo_Target_Value"), "{pointer}");
        let kept = read_lf(path.join("unfiltered/KeptPointer.model.json")).unwrap();
        assert!(kept.contains("\"Rojo_Target_Value\": \"kept\""), "{kept}");
        assert_eq!(
            read_lf(path.join("unfiltered/Foo.luau")).unwrap(),
            "return \"Foo from Studio\"\n"
        );
    });
}

/// A pointer in a partition syncback leaves alone names a writable target by
/// its model file's `id`. Studio's copy of the target has no `Rojo_Id`, but
/// the pointer file is not rewritten, so the target's file must keep that ID.
#[test]
fn partition_ignore_globs_reverse_ref() {
    run_syncback_test("partition_ignore_globs_reverse_ref", |path| {
        let pointer = read_lf(path.join("filtered/Pointer.model.json")).unwrap();
        assert!(
            pointer.contains("\"Rojo_Target_Value\": \"target\""),
            "{pointer}"
        );
        let target = read_lf(path.join("unfiltered/Target.model.json")).unwrap();
        assert!(target.contains("\"target\""), "{target}");
    });
}

/// As above, but Studio renamed the target. It is found through the
/// pointer's own Ref, so it keeps the ID the pointer file uses.
#[test]
fn partition_ignore_globs_reverse_ref_renamed() {
    run_syncback_test("partition_ignore_globs_reverse_ref_renamed", |path| {
        let target = read_lf(path.join("unfiltered/Renamed/init.meta.json")).unwrap();
        assert!(target.contains("\"Rojo_Id\": \"target\""), "{target}");
    });
}

/// As above, but the target already has the ID as `Rojo_Id` and Studio
/// copied it. The copy gets a new ID; the pointer's target keeps its own.
#[test]
fn partition_ignore_globs_reverse_ref_dup() {
    run_syncback_test("partition_ignore_globs_reverse_ref_dup", |path| {
        let target = read_lf(path.join("unfiltered/Target.model.json")).unwrap();
        assert!(target.contains("\"Rojo_Id\": \"target\""), "{target}");
        let copy = read_lf(path.join("unfiltered/ACopy/init.meta.json")).unwrap();
        assert!(!copy.contains("\"target\""), "{copy}");
    });
}

/// Studio duplicated an instance with an ID inside a partition syncback
/// leaves alone, and a synced partition points at both copies. The ID is the
/// original's in the files, so only the pointer at the original is linked;
/// the copy is not in the files, so its pointer keeps what its file had.
#[test]
fn partition_ignore_globs_dup_ref() {
    run_syncback_test("partition_ignore_globs_dup_ref", |path| {
        assert_eq!(
            read_lf(path.join("filtered/Kept.model.json")).unwrap(),
            "{\n  \"className\": \"Folder\",\n  \"attributes\": {\n    \"Rojo_Id\": \"kept\"\n  }\n}\n"
        );
        assert!(!path.join("filtered/KeptCopy.model.json").exists());
        let read = |name: &str| {
            let text = read_lf(path.join(format!("unfiltered/{name}.model.json"))).unwrap();
            serde_json::from_str::<serde_json::Value>(&text).unwrap()
        };
        assert_eq!(
            read("KeptPointer")["attributes"]["Rojo_Target_Value"],
            "kept"
        );
        let copy = read("CopyPointer");
        assert!(
            copy["attributes"].get("Rojo_Target_Value").is_none(),
            "{copy}"
        );
    });
}

/// Studio added an instance under a nested project's root, whose partition is
/// the nested project's `$path` while the root itself is made from its
/// project file. It is in the partition all the same, so a pointer at it is
/// not linked with an ID no file would hold.
#[test]
fn partition_ignore_globs_nested_ref() {
    run_syncback_test("partition_ignore_globs_nested_ref", |path| {
        assert!(!path.join("pkg/src/NewTarget.model.json").exists());
        assert_eq!(
            read_lf(path.join("pkg/src/Foo.luau")).unwrap(),
            "return \"Foo\"\n"
        );
        let pointer = read_lf(path.join("unfiltered/Pointer.model.json")).unwrap();
        assert!(!pointer.contains("Rojo_Target_Value"), "{pointer}");
        assert_eq!(
            read_lf(path.join("unfiltered/Foo.luau")).unwrap(),
            "return \"Foo from Studio\"\n"
        );
    });
}

/// Studio replaced the protected `Filtered` Folder with a Model of the same
/// name. Syncback still skips it by its `$path`, so what is under it is
/// still protected, and a pointer at an instance Studio added there is not
/// linked with an ID no file would hold.
#[test]
fn partition_ignore_globs_class_ref() {
    run_syncback_test("partition_ignore_globs_class_ref", |path| {
        assert!(!path.join("filtered/NewTarget.model.json").exists());
        assert!(!path.join("filtered/init.meta.json").exists());
        assert_eq!(
            read_lf(path.join("filtered/Foo.luau")).unwrap(),
            "return \"Foo\"\n"
        );
        let pointer = read_lf(path.join("unfiltered/Pointer.model.json")).unwrap();
        assert!(!pointer.contains("Rojo_Target_Value"), "{pointer}");
    });
}

/// Rewriting a `.model.json` keeps `ignoreUnknownInstances` on the model and
/// on the children that had it, in either spelling, since Studio has no such
/// setting to send back. What is written reads back the same.
#[test]
fn json_model_ignore_unknown_instances() {
    run_syncback_test("json_model_ignore_unknown_instances", |path| {
        let text = read_lf(path.join("src/Arena.model.json")).unwrap();
        let model: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(model["attributes"]["Changed"], true, "{text}");
        assert_eq!(model["ignoreUnknownInstances"], true, "{text}");
        let child = |name: &str| {
            model["children"]
                .as_array()
                .unwrap()
                .iter()
                .find(|child| child["name"] == name)
                .unwrap()
                .clone()
        };
        assert_eq!(child("Inner")["ignoreUnknownInstances"], true, "{text}");
        assert!(
            child("Plain").get("ignoreUnknownInstances").is_none(),
            "{text}"
        );
    });
}

/// Studio copied a protected instance, ID and all, into a synced partition,
/// and points at the copy only. The ID stays the protected file's, so the
/// copy is written with a new one, which its pointer uses.
#[test]
fn partition_ignore_globs_reserved_id() {
    run_syncback_test("partition_ignore_globs_reserved_id", |path| {
        assert_eq!(
            read_lf(path.join("filtered/Kept.model.json")).unwrap(),
            "{\n  \"className\": \"Folder\",\n  \"attributes\": {\n    \"Rojo_Id\": \"kept\"\n  }\n}\n"
        );
        let read = |name: &str| {
            let text = read_lf(path.join(name)).unwrap();
            serde_json::from_str::<serde_json::Value>(&text).unwrap()
        };
        // A Folder is written as a directory with its attributes in its
        // init.meta.json.
        let copy = read("unfiltered/Copy/init.meta.json");
        let id = copy["attributes"]["Rojo_Id"].as_str().unwrap();
        assert_ne!(id, "kept", "{copy}");
        let pointer = read("unfiltered/Pointer.model.json");
        assert_eq!(pointer["attributes"]["Rojo_Target_Value"], id, "{pointer}");
    });
}

/// Partitions are found from every project file: a nested project's root
/// node, and a node whose `$path` names a project file (based at that file's
/// folder), are both skipped.
#[test]
fn partition_ignore_globs_nested() {
    run_syncback_test("partition_ignore_globs_nested", |path| {
        for dir in ["package/src", "outer/src"] {
            assert_eq!(
                read_lf(path.join(dir).join("Foo.luau")).unwrap(),
                "return \"Foo\"\n",
                "{dir}"
            );
            assert!(!path.join(dir).join("New.luau").exists(), "{dir}");
        }
        for name in ["other/Foo.luau", "other/New.luau"] {
            let content = read_lf(path.join(name)).unwrap();
            assert_snapshot!(
                format!("partition_ignore_globs_nested-{name}"),
                content,
                name
            );
        }
    });
}

/// Reads a text file with its line endings as `\n`, since a Windows checkout
/// may give the fixtures `\r\n`.
fn read_lf(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    Ok(fs_err::read_to_string(path)?.replace("\r\n", "\n"))
}
