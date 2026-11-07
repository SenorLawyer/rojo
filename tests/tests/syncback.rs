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
                        let content = fs_err::read_to_string(new).unwrap();
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

/// Studio holds `Foo.spec` in both partitions, but only the partition without
/// `$globIgnorePaths` gets the file; the other partition's non-ignored files
/// are still written.
#[test]
fn partition_ignore_globs() {
    run_syncback_test("partition_ignore_globs", |path| {
        assert!(
            !path.join("filtered/Foo.spec.luau").exists(),
            "syncback wrote into a path the node's $globIgnorePaths exclude"
        );
        for name in [
            "filtered/Foo.luau",
            "unfiltered/Foo.luau",
            "unfiltered/Foo.spec.luau",
        ] {
            let content = fs_err::read_to_string(path.join(name)).unwrap();
            assert_snapshot!(format!("partition_ignore_globs-{name}"), content, name);
        }
    });
}
