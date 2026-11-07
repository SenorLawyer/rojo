use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use insta::assert_snapshot;
use rbx_dom_weak::{
    types::{Attributes, Ref, Variant},
    ustr, InstanceBuilder, WeakDom,
};
use tempfile::tempdir;

use crate::rojo_test::io_util::{
    copy_recursive, get_working_dir_path, KillOnDrop, BUILD_TESTS_PATH, ROJO_PATH,
};

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
    content_properties,
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
        Self::spawn_with(project_path, output, &[])
    }

    fn spawn_with(project_path: &Path, output: &Path, extra: &[&std::ffi::OsStr]) -> Self {
        // Killed on drop so a failed assertion doesn't leave the watcher running.
        let mut child = KillOnDrop(
            Command::new(ROJO_PATH)
                .arg("build")
                .arg(project_path)
                .arg("-o")
                .arg(output)
                .arg("--watch")
                .args(extra)
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

/// Builds the base place for the `into_place` tests: a map, a lighting tweak,
/// extra ReplicatedStorage content, a `Shared` folder the project replaces
/// and refs that cross between kept, paired and replaced instances.
fn into_place_base() -> WeakDom {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = dom.root_ref();

    let workspace = dom.insert(
        root,
        InstanceBuilder::new("Workspace")
            .with_property("Gravity", 196.2f32)
            .with_property("FallenPartsDestroyHeight", -1000.0f32),
    );
    let map = dom.insert(workspace, InstanceBuilder::new("Model").with_name("Map"));
    dom.insert(map, InstanceBuilder::new("Part").with_name("Baseplate"));
    dom.insert(
        map,
        InstanceBuilder::new("Part")
            .with_name("Spawn")
            .with_property("Attributes", Attributes::new().with("Rojo_Id", "BaseSpawn")),
    );
    let map_ref = dom.insert(
        workspace,
        InstanceBuilder::new("ObjectValue").with_name("MapRef"),
    );
    let config_ref = dom.insert(
        workspace,
        InstanceBuilder::new("ObjectValue").with_name("ConfigRef"),
    );
    let old_ref = dom.insert(
        workspace,
        InstanceBuilder::new("ObjectValue").with_name("OldRef"),
    );
    dom.insert(
        workspace,
        InstanceBuilder::new("Terrain").with_name("Terrain"),
    );

    dom.insert(
        root,
        InstanceBuilder::new("Lighting").with_property("Brightness", 3.0f32),
    );
    // Not a service, so it follows the root's $ignoreUnknownInstances.
    dom.insert(
        root,
        InstanceBuilder::new("Folder").with_name("LooseFolder"),
    );

    let storage = dom.insert(root, InstanceBuilder::new("ReplicatedStorage"));
    let shared = dom.insert(
        storage,
        InstanceBuilder::new("Folder")
            .with_name("Shared")
            // The project's $id for Shared must replace this ID.
            .with_property(
                "Attributes",
                Attributes::new()
                    .with("Owner", "map-team")
                    .with("Rojo_Id", "OldShared"),
            ),
    );
    // The project's Link.rbxmx has no value, which must clear this one.
    let link = dom.insert(
        shared,
        InstanceBuilder::new("ObjectValue").with_name("Link"),
    );
    let config = dom.insert(
        shared,
        InstanceBuilder::new("StringValue")
            .with_name("Config")
            .with_property("Value", "base value")
            .with_property("Attributes", Attributes::new().with("Note", "from base")),
    );
    let old_module = dom.insert(
        shared,
        InstanceBuilder::new("ModuleScript")
            .with_name("OldModule")
            .with_property("Source", "return 'old'"),
    );
    let assets = dom.insert(
        storage,
        InstanceBuilder::new("Folder").with_name("BaseAssets"),
    );
    dom.insert(assets, InstanceBuilder::new("Model").with_name("Tree"));
    dom.insert(storage, InstanceBuilder::new("Model").with_name("Remotes"));
    let pointers = dom.insert(
        storage,
        InstanceBuilder::new("Folder").with_name("Pointers"),
    );
    dom.insert(
        pointers,
        InstanceBuilder::new("ObjectValue").with_name("ToShared"),
    );
    let base_pointer = dom.insert(
        pointers,
        InstanceBuilder::new("ObjectValue").with_name("BasePointer"),
    );

    let scripts = dom.insert(root, InstanceBuilder::new("ServerScriptService"));
    dom.insert(
        scripts,
        InstanceBuilder::new("Script")
            .with_name("LegacyScript")
            .with_property("Source", "print('legacy')"),
    );
    dom.insert(
        scripts,
        InstanceBuilder::new("Script")
            .with_name("Main")
            .with_property("Source", "print('main from base')"),
    );

    for (holder, target) in [
        (map_ref, map),
        (config_ref, config),
        (old_ref, old_module),
        (base_pointer, map),
        (link, map),
    ] {
        dom.get_by_ref_mut(holder)
            .unwrap()
            .properties
            .insert(ustr("Value"), Variant::Ref(target));
    }

    dom
}

fn write_place(dom: &WeakDom, path: &Path) {
    let file = fs::File::create(path).unwrap();
    let children = dom.root().children();
    if path.extension().unwrap() == "rbxl" {
        rbx_binary::to_writer(file, dom, children).unwrap();
    } else {
        rbx_xml::to_writer_default(file, dom, children).unwrap();
    }
}

fn read_place(path: &Path) -> WeakDom {
    let file = fs::File::open(path).unwrap();
    if path.extension().unwrap() == "rbxl" {
        rbx_binary::from_reader(file).unwrap()
    } else {
        rbx_xml::from_reader_default(file).unwrap()
    }
}

fn instance_path(dom: &WeakDom, id: Ref) -> String {
    let mut names = Vec::new();
    let mut current = id;
    while current != dom.root_ref() {
        let instance = dom.get_by_ref(current).unwrap();
        names.push(instance.name.as_str());
        current = instance.parent();
    }
    names.reverse();
    names.join("/")
}

/// Describes the parts of a place the `into_place` tests check, in a form
/// that is the same for binary and XML files: binary files carry every
/// property of a class, so only the properties the tests are about are
/// listed, and refs are shown as the path of their target.
fn describe_place(dom: &WeakDom) -> String {
    const SHOWN: &[&str] = &[
        "Attributes",
        "Brightness",
        "FallenPartsDestroyHeight",
        "Gravity",
        "Source",
        "Value",
    ];

    fn describe(dom: &WeakDom, id: Ref, depth: usize, out: &mut String) {
        let instance = dom.get_by_ref(id).unwrap();
        let indent = "  ".repeat(depth);
        out.push_str(&format!("{indent}{} ({})\n", instance.name, instance.class));
        let mut properties: Vec<_> = instance
            .properties
            .iter()
            .filter(|(name, _)| SHOWN.contains(&name.as_str()))
            .collect();
        properties.sort_by_key(|(name, _)| name.as_str());
        for (name, value) in properties {
            let value = match value {
                Variant::Ref(target) if target.is_none() => "nil".to_owned(),
                Variant::Ref(target) if dom.get_by_ref(*target).is_none() => {
                    "<dangling>".to_owned()
                }
                Variant::Ref(target) => format!("-> {}", instance_path(dom, *target)),
                Variant::String(value) => format!("{value:?}"),
                Variant::Float32(value) => value.to_string(),
                Variant::Attributes(attributes) => {
                    if attributes.is_empty() {
                        continue;
                    }
                    let mut entries: Vec<_> = attributes
                        .iter()
                        .map(|(key, value)| match value {
                            Variant::String(value) => format!("{key}={value:?}"),
                            // Attributes read from a file hold their strings
                            // as BinaryString.
                            Variant::BinaryString(value) => {
                                format!("{key}={:?}", String::from_utf8_lossy(value.as_ref()))
                            }
                            other => format!("{key}={other:?}"),
                        })
                        .collect();
                    entries.sort();
                    entries.join(", ")
                }
                other => format!("{other:?}"),
            };
            out.push_str(&format!("{indent}  .{name} = {value}\n"));
        }
        for &child in instance.children() {
            describe(dom, child, depth + 1, out);
        }
    }

    let mut out = String::new();
    for &child in dom.root().children() {
        describe(dom, child, 0, &mut out);
    }
    out
}

fn build_into(base: &Path, output: &Path) -> std::process::Output {
    build_project_into(
        &Path::new(BUILD_TESTS_PATH).join("into_place"),
        base,
        output,
    )
}

fn build_project_into(project: &Path, base: &Path, output: &Path) -> std::process::Output {
    Command::new(ROJO_PATH)
        .current_dir(get_working_dir_path())
        .arg("build")
        .arg(project)
        .arg("--into")
        .arg(base)
        .arg("-o")
        .arg(output)
        .env("RUST_LOG", "error")
        .output()
        .expect("Couldn't start Rojo")
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "Rojo did not exit successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `--into` keeps what only the base place has (the map, a lighting tweak,
/// a ReplicatedStorage sibling, service properties the project does not set),
/// replaces the subtree a `$path` folder owns and resolves refs across both,
/// for XML and binary base and output files alike.
#[test]
fn build_into_place_keeps_base_content() {
    let temp = tempdir().unwrap();
    let base = into_place_base();

    let mut descriptions = Vec::new();
    for (base_name, output_name) in [
        ("base.rbxlx", "out.rbxlx"),
        ("base.rbxl", "out.rbxl"),
        ("base2.rbxl", "out2.rbxlx"),
        ("base2.rbxlx", "out2.rbxl"),
    ] {
        let base_path = temp.path().join(base_name);
        let output_path = temp.path().join(output_name);
        write_place(&base, &base_path);
        let before = fs::read(&base_path).unwrap();

        assert_success(&build_into(&base_path, &output_path));
        assert_eq!(
            fs::read(&base_path).unwrap(),
            before,
            "the base place was modified"
        );

        descriptions.push((output_name, describe_place(&read_place(&output_path))));
    }

    for (name, description) in &descriptions[1..] {
        pretty_assertions::assert_eq!(
            &descriptions[0].1,
            description,
            "{} differs from out.rbxlx",
            name
        );
    }

    let xml = fs::read_to_string(temp.path().join("out.rbxlx")).unwrap();
    // A ref to a removed base instance must be written as nil rather than as
    // a referent that names no item in the file.
    let referents: HashSet<&str> = xml
        .split("referent=\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap())
        .collect();
    for value in xml.split("<Ref name=\"").skip(1) {
        let target = value.split('>').nth(1).unwrap().split('<').next().unwrap();
        assert!(
            target == "null" || referents.contains(target),
            "dangling ref {target} in output"
        );
    }
    let mut settings = insta::Settings::new();
    settings.set_snapshot_path(
        Path::new(BUILD_TESTS_PATH)
            .parent()
            .unwrap()
            .join("build-test-snapshots"),
    );
    settings.bind(|| {
        assert_snapshot!("into_place", descriptions[0].1);
        assert_snapshot!("into_place_xml", xml);
    });
}

/// Applying the project onto the place it is written to goes through the
/// same staged write as any build, and applying it again changes nothing.
#[test]
fn build_into_place_in_place() {
    let temp = tempdir().unwrap();
    let place = temp.path().join("place.rbxl");
    write_place(&into_place_base(), &place);

    assert_success(&build_into(&place, &place));
    let first = describe_place(&read_place(&place));
    assert!(first.contains("LegacyScript (Script)"), "{first}");
    assert!(first.contains("Util (ModuleScript)"), "{first}");
    assert!(!first.contains("OldModule"), "{first}");

    assert_success(&build_into(&place, &place));
    pretty_assertions::assert_eq!(first, describe_place(&read_place(&place)));
}

#[test]
fn build_into_refuses_non_place_inputs_and_outputs() {
    let temp = tempdir().unwrap();
    let base = temp.path().join("base.rbxl");
    write_place(&into_place_base(), &base);
    let stderr =
        |output: &std::process::Output| String::from_utf8_lossy(&output.stderr).into_owned();

    // A model project cannot be applied onto a place, and the output is left
    // as it was.
    let output = temp.path().join("existing.rbxl");
    fs::write(&output, "keep").unwrap();
    let result = Command::new(ROJO_PATH)
        .current_dir(get_working_dir_path())
        .arg("build")
        .arg(Path::new(BUILD_TESTS_PATH).join("module_init"))
        .arg("--into")
        .arg(&base)
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains("as a place"),
        "{}",
        stderr(&result)
    );
    assert_eq!(fs::read(&output).unwrap(), b"keep");

    // The output must be a place.
    let result = build_into(&base, &temp.path().join("out.rbxm"));
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains("must end in .rbxl or .rbxlx"),
        "{}",
        stderr(&result)
    );

    // So must the base.
    let model = temp.path().join("base.rbxm");
    fs::write(&model, "not a place").unwrap();
    let result = build_into(&model, &temp.path().join("out.rbxl"));
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains("must be a place file"),
        "{}",
        stderr(&result)
    );
    assert!(!temp.path().join("out.rbxl").exists());

    // A watched build cannot keep applying the project onto its own output.
    let result = Command::new(ROJO_PATH)
        .current_dir(get_working_dir_path())
        .arg("build")
        .arg(Path::new(BUILD_TESTS_PATH).join("into_place"))
        .arg("--into")
        .arg(&base)
        .arg("-o")
        .arg(&base)
        .arg("--watch")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains("--watch cannot write over"),
        "{}",
        stderr(&result)
    );
}

/// A Workspace built from a `$path` folder keeps the base's map and Terrain
/// when the folder's `init.meta.json` says `ignoreUnknownInstances`, and
/// loses them without it. The root of this project does not ignore unknown
/// instances, so a top-level folder that is not a service is removed while
/// the services the project does not mention stay.
#[test]
fn build_into_workspace_path_with_meta_file() {
    let temp = tempdir().unwrap();
    let base = temp.path().join("base.rbxl");
    write_place(&into_place_base(), &base);

    let fixture = Path::new(BUILD_TESTS_PATH).join("into_place_workspace");
    let project = temp.path().join("project");
    copy_recursive(&fixture, &project).unwrap();

    let output = temp.path().join("kept.rbxl");
    assert_success(&build_project_into(&project, &base, &output));
    let kept = describe_place(&read_place(&output));

    fs::remove_file(project.join("src/workspace/init.meta.json")).unwrap();
    let output = temp.path().join("replaced.rbxl");
    assert_success(&build_project_into(&project, &base, &output));
    let replaced = describe_place(&read_place(&output));

    let mut settings = insta::Settings::new();
    settings.set_snapshot_path(
        Path::new(BUILD_TESTS_PATH)
            .parent()
            .unwrap()
            .join("build-test-snapshots"),
    );
    settings.bind(|| {
        assert_snapshot!("into_place_workspace_meta", kept);
        assert_snapshot!("into_place_workspace_no_meta", replaced);
    });

    for description in [&kept, &replaced] {
        assert!(description.contains("Spawner (Script)"), "{description}");
        assert!(description.contains("Lighting (Lighting)"), "{description}");
        assert!(
            description.contains("ServerScriptService (ServerScriptService)"),
            "{description}"
        );
        assert!(!description.contains("LooseFolder"), "{description}");
    }
    assert!(kept.contains("Map (Model)"), "{kept}");
    assert!(kept.contains("Terrain (Terrain)"), "{kept}");
    assert!(!replaced.contains("Map (Model)"), "{replaced}");
    assert!(!replaced.contains("Terrain (Terrain)"), "{replaced}");
}

/// A `Rojo_Id` that a kept base instance shares with a project ID would leave
/// both unresolvable, so the build fails before writing.
#[test]
fn build_into_refuses_base_ids_that_collide_with_project_ids() {
    let temp = tempdir().unwrap();
    let mut dom = into_place_base();
    let lighting = dom.root().children()[1];
    assert_eq!(dom.get_by_ref(lighting).unwrap().class, "Lighting");
    dom.insert(
        lighting,
        InstanceBuilder::new("Folder")
            .with_name("Sky")
            .with_property("Attributes", Attributes::new().with("Rojo_Id", "Remotes")),
    );
    let base = temp.path().join("base.rbxlx");
    write_place(&dom, &base);

    let output = temp.path().join("out.rbxlx");
    let result = build_into(&base, &output);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    assert!(stderr.contains("\"Remotes\""), "{stderr}");
    assert!(stderr.contains("Lighting/Sky"), "{stderr}");
    assert!(stderr.contains("ReplicatedStorage/Remotes"), "{stderr}");
    assert!(!output.exists());
}

/// `--watch --into` rebuilds when only the base place changes, and recovers
/// once a base that could not be read is saved again.
#[test]
fn watch_into_rebuilds_when_the_base_changes() {
    let temp = tempdir().unwrap();
    let base = temp.path().join("base.rbxlx");
    let output = temp.path().join("out.rbxlx");
    let mut dom = into_place_base();
    write_place(&dom, &base);

    let project = Path::new(BUILD_TESTS_PATH).join("into_place");
    let mut watch = WatchBuild::spawn_with(&project, &output, &["--into".as_ref(), base.as_ref()]);
    wait_until("initial build", || output_contains(&output, "Baseplate"));

    let workspace = dom.root().children()[0];
    dom.insert(
        workspace,
        InstanceBuilder::new("Model").with_name("SavedInStudio"),
    );
    write_place(&dom, &base);
    wait_until("rebuild after the base changed", || {
        output_contains(&output, "SavedInStudio")
    });

    fs::write(&base, "not a place").unwrap();
    wait_until("rebuild error", || {
        watch.stderr_contains("Failed to rebuild project")
    });
    assert!(watch.is_running(), "watch mode exited after a bad base");
    assert!(
        output_contains(&output, "SavedInStudio"),
        "a failed rebuild replaced the last good output"
    );

    dom.insert(
        workspace,
        InstanceBuilder::new("Model").with_name("Recovered"),
    );
    write_place(&dom, &base);
    wait_until("rebuild after the base was fixed", || {
        output_contains(&output, "Recovered")
    });

    watch.stop();
}
