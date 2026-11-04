use std::{fs, path::Path, process::Command};

use insta::assert_snapshot;
use tempfile::tempdir;

use crate::rojo_test::io_util::{get_working_dir_path, BUILD_TESTS_PATH, ROJO_PATH};

macro_rules! gen_build_tests {
    ( $($test_name: ident,)* ) => {
        $(
            paste::item! {
                #[test]
                fn [<build_ $test_name>]() {
                    let _ = env_logger::try_init();

                    run_build_test(stringify!($test_name));
                }
            }
        )*
    };
}

gen_build_tests! {
    init_csv_with_children,
    attributes,
    client_in_folder,
    client_init,
    csv_bug_145,
    csv_bug_147,
    csv_in_folder,
    deep_nesting,
    gitkeep,
    ignore_glob_inner,
    ignore_glob_nested,
    ignore_glob_spec,
    infer_service_name,
    infer_starter_player,
    init_meta_class_name,
    init_meta_properties,
    init_with_children,
    issue_546,
    json_as_lua,
    json_model_in_folder,
    json_model_legacy_name,
    module_in_folder,
    module_init,
    nested_runcontext,
    optional,
    project_composed_default,
    project_composed_file,
    project_root_name,
    rbxm_in_folder,
    rbxmx_in_folder,
    rbxmx_ref,
    script_meta_disabled,
    server_in_folder,
    server_init,
    txt,
    txt_in_folder,
    unresolved_values,
    weldconstraint,
    sync_rule_alone,
    sync_rule_complex,
    sync_rule_nested_projects,
    no_name_default_project,
    no_name_project,
    no_name_top_level_project,
    plugin_init,
}

fn run_build_test(test_name: &str) {
    let working_dir = get_working_dir_path();

    let input_path = Path::new(BUILD_TESTS_PATH).join(test_name);

    let output_dir = tempdir().expect("couldn't create temporary directory");
    let output_path = output_dir.path().join(format!("{}.rbxmx", test_name));

    let output = Command::new(ROJO_PATH)
        .args([
            "build",
            input_path.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "error")
        .current_dir(working_dir)
        .output()
        .expect("Couldn't start Rojo");

    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));

    assert!(output.status.success(), "Rojo did not exit successfully");

    let contents = fs::read_to_string(&output_path).expect("Couldn't read output file");

    let mut settings = insta::Settings::new();

    let snapshot_path = Path::new(BUILD_TESTS_PATH)
        .parent()
        .unwrap()
        .join("build-test-snapshots");

    settings.set_snapshot_path(snapshot_path);

    settings.bind(|| {
        assert_snapshot!(test_name, contents);
    });
}

#[test]
fn building_model_as_place_preserves_existing_output() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("existing.rbxl");
    fs::write(&path, "keep").unwrap();
    let result = Command::new(ROJO_PATH)
        .current_dir(get_working_dir_path())
        .arg("build")
        .arg(Path::new(BUILD_TESTS_PATH).join("module_init"))
        .arg("-o")
        .arg(&path)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("as a place"));
    assert_eq!(fs::read(path).unwrap(), b"keep");
}

#[test]
fn binary_compression_modes_round_trip() {
    let temp = tempdir().unwrap();
    for mode in ["lz4", "zstd", "none"] {
        let output = temp.path().join(format!("{mode}.rbxm"));
        let result = Command::new(ROJO_PATH)
            .current_dir(get_working_dir_path())
            .arg("build")
            .arg(Path::new(BUILD_TESTS_PATH).join("module_init"))
            .arg("-o")
            .arg(&output)
            .args(["--compress", mode])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let bytes = fs::read(output).unwrap();
        let dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();
        let root = dom.get_by_ref(dom.root().children()[0]).unwrap();
        assert_eq!(root.class, "ModuleScript");
        assert!(root.properties.contains_key(&rbx_dom_weak::ustr("Source")));
        if mode == "zstd" {
            assert!(bytes
                .windows(4)
                .any(|bytes| bytes == [0x28, 0xb5, 0x2f, 0xfd]));
        }
    }
}
