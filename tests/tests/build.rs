use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use insta::assert_snapshot;
use tempfile::tempdir;

use crate::rojo_test::io_util::{get_working_dir_path, KillOnDrop, BUILD_TESTS_PATH, ROJO_PATH};

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
    data_csv,
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

fn watch_project(root_class: &str, child: &str) -> String {
    format!(
        r#"{{"name": "watch", "tree": {{"$className": "{root_class}", "{child}": {{"$className": "Folder"}}}}}}"#
    )
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// A `rojo build --watch` process whose stderr is collected in the background.
struct WatchBuild {
    child: KillOnDrop,
    stderr: Arc<Mutex<Vec<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
}

impl WatchBuild {
    fn spawn(project_path: &Path, output: &Path) -> Self {
        // Killed on drop so a failed assertion doesn't leave the watcher running.
        let mut child = KillOnDrop(
            Command::new(ROJO_PATH)
                .arg("build")
                .arg(project_path)
                .arg("-o")
                .arg(output)
                .arg("--watch")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );

        // Raw bytes are kept and decoded when searched, since a chunk boundary
        // can split a multi-byte character.
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let reader = {
            let stderr = Arc::clone(&stderr);
            let mut pipe = child.0.stderr.take().unwrap();
            thread::spawn(move || {
                let mut buffer = [0; 1024];
                while let Ok(read) = pipe.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    stderr.lock().unwrap().extend_from_slice(&buffer[..read]);
                }
            })
        };

        Self {
            child,
            stderr,
            reader: Some(reader),
        }
    }

    fn stderr_contains(&self, needle: &str) -> bool {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).contains(needle)
    }

    fn is_running(&mut self) -> bool {
        self.child.0.try_wait().unwrap().is_none()
    }

    fn stop(mut self) {
        self.child.0.kill().unwrap();
        self.child.0.wait().unwrap();
        self.reader.take().unwrap().join().unwrap();
    }
}

fn output_contains(output: &Path, needle: &str) -> bool {
    fs::read_to_string(output)
        .map(|contents| contents.contains(needle))
        .unwrap_or(false)
}

/// A failing rebuild in watch mode (here: the root temporarily stops being a
/// DataModel while building a place) must be reported without stopping the
/// watcher, so that fixing the project produces a new build.
#[test]
fn watch_keeps_running_after_failed_rebuild() {
    let temp = tempdir().unwrap();
    let project_path = temp.path().join("default.project.json");
    let output = temp.path().join("watch.rbxlx");
    fs::write(&project_path, watch_project("DataModel", "FirstBuild")).unwrap();

    let mut watch = WatchBuild::spawn(&project_path, &output);
    wait_until("initial build", || output_contains(&output, "FirstBuild"));

    fs::write(&project_path, watch_project("Folder", "Broken")).unwrap();
    wait_until("rebuild error", || {
        watch.stderr_contains("Failed to rebuild project")
    });
    assert!(
        watch.is_running(),
        "watch mode exited after a failed rebuild"
    );
    assert!(
        output_contains(&output, "FirstBuild"),
        "failed rebuild replaced the last good output"
    );

    fs::write(&project_path, watch_project("DataModel", "SecondBuild")).unwrap();
    wait_until("rebuild after fix", || {
        output_contains(&output, "SecondBuild")
    });

    watch.stop();
}

/// Watch mode must also survive a failing first build, since the project can
/// be mid-edit when the command starts.
#[test]
fn watch_keeps_running_after_failed_initial_build() {
    let temp = tempdir().unwrap();
    let project_path = temp.path().join("default.project.json");
    let output = temp.path().join("watch.rbxlx");
    fs::write(&project_path, watch_project("Folder", "Broken")).unwrap();

    let mut watch = WatchBuild::spawn(&project_path, &output);
    wait_until("initial build error", || {
        watch.stderr_contains("Failed to build project")
    });
    assert!(
        watch.is_running(),
        "watch mode exited after a failed initial build"
    );
    assert!(!output.exists(), "failed initial build wrote an output");

    fs::write(&project_path, watch_project("DataModel", "FixedBuild")).unwrap();
    wait_until("build after fix", || output_contains(&output, "FixedBuild"));

    watch.stop();
}
