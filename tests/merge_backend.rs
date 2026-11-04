//! Real-engine tests. Set ROJO_TEST_RBX_MERGE to the pinned executable and run
//! `cargo test --locked --test merge_backend -- --ignored`.

use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use rbx_dom_weak::{
    types::{Attributes, Ref, UniqueId, Variant, Vector3},
    ustr, InstanceBuilder, WeakDom,
};

fn engine() -> PathBuf {
    PathBuf::from(
        std::env::var_os("ROJO_TEST_RBX_MERGE")
            .expect("Set ROJO_TEST_RBX_MERGE to the pinned rbx-merge executable"),
    )
}

fn rojo(engine: &Path, directory: &Path, arguments: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rojo"))
        .arg("merge")
        .arg("--engine")
        .arg(engine)
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", directory.join("global-config"))
        .output()
        .unwrap()
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn model() -> WeakDom {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = dom.insert(
        dom.root_ref(),
        InstanceBuilder::new("Folder")
            .with_name("Root")
            .with_property("UniqueId", UniqueId::new(1, 1, 1)),
    );
    let left = dom.insert(
        root,
        InstanceBuilder::new("Folder")
            .with_name("Left")
            .with_property("UniqueId", UniqueId::new(2, 1, 1)),
    );
    for (index, name) in [(3, "Right"), (4, "Third")] {
        dom.insert(
            root,
            InstanceBuilder::new("Folder")
                .with_name(name)
                .with_property("UniqueId", UniqueId::new(index, 1, 1)),
        );
    }
    let mut attributes = Attributes::new();
    attributes.insert("KeepMe".into(), Variant::Bool(true));
    let part = dom.insert(
        left,
        InstanceBuilder::new("Part")
            .with_name("Target")
            .with_property("UniqueId", UniqueId::new(5, 1, 1))
            .with_property("Anchored", false)
            .with_property("Transparency", 0.0_f32)
            .with_property("Size", Vector3::new(2.0, 3.0, 4.0))
            .with_property("Attributes", attributes),
    );
    dom.insert(
        root,
        InstanceBuilder::new("Part")
            .with_name("OtherTarget")
            .with_property("UniqueId", UniqueId::new(6, 1, 1)),
    );
    dom.insert(
        root,
        InstanceBuilder::new("ObjectValue")
            .with_name("Pointer")
            .with_property("UniqueId", UniqueId::new(7, 1, 1))
            .with_property("Value", part),
    );
    dom.insert(
        root,
        InstanceBuilder::new("ModuleScript")
            .with_name("Code")
            .with_property("UniqueId", UniqueId::new(8, 1, 1))
            .with_property("Source", "-- keep UTF-8 café\nreturn 42\n"),
    );
    dom
}

fn find(dom: &WeakDom, name: &str) -> Ref {
    dom.descendants()
        .find(|instance| instance.name == name)
        .unwrap()
        .referent()
}

fn edit(dom: &mut WeakDom, name: &str, property: &str, value: impl Into<Variant>) {
    let id = find(dom, name);
    dom.get_by_ref_mut(id)
        .unwrap()
        .properties
        .insert(ustr(property), value.into());
}

fn encode(dom: &WeakDom, extension: &str) -> Vec<u8> {
    let mut output = Vec::new();
    if matches!(extension, "rbxm" | "rbxl") {
        rbx_binary::to_writer(&mut output, dom, dom.root().children()).unwrap();
    } else {
        rbx_xml::to_writer(
            &mut output,
            dom,
            dom.root().children(),
            rbx_xml::EncodeOptions::new()
                .property_behavior(rbx_xml::EncodePropertyBehavior::WriteUnknown),
        )
        .unwrap();
    }
    output
}

fn decode(path: &Path) -> WeakDom {
    let bytes = fs::read(path).unwrap();
    if matches!(path.extension().unwrap().to_str().unwrap(), "rbxm" | "rbxl") {
        rbx_binary::from_reader(Cursor::new(bytes)).unwrap()
    } else {
        rbx_xml::from_reader(
            Cursor::new(bytes),
            rbx_xml::DecodeOptions::new()
                .property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown),
        )
        .unwrap()
    }
}

fn write_inputs(
    directory: &Path,
    extension: &str,
    ours: &WeakDom,
    theirs: &WeakDom,
) -> [PathBuf; 4] {
    let paths =
        ["base", "ours", "theirs", "out"].map(|name| directory.join(format!("{name}.{extension}")));
    for (path, dom) in paths.iter().zip([&model(), ours, theirs]) {
        fs::write(path, encode(dom, extension)).unwrap();
    }
    paths
}

fn run_merge(
    engine: &Path,
    dir: &Path,
    paths: &[PathBuf; 4],
    extra: &[&std::ffi::OsStr],
) -> Output {
    let mut arguments: Vec<&std::ffi::OsStr> = vec![
        "run".as_ref(),
        "--base".as_ref(),
        paths[0].as_os_str(),
        "--ours".as_ref(),
        paths[1].as_os_str(),
        "--theirs".as_ref(),
        paths[2].as_os_str(),
        "--out".as_ref(),
        paths[3].as_os_str(),
    ];
    arguments.extend_from_slice(extra);
    rojo(engine, dir, &arguments)
}

#[test]
#[ignore = "requires the pinned external engine in ROJO_TEST_RBX_MERGE"]
fn independent_properties_merge_and_preserve_instances_and_references_in_all_formats() {
    let engine = engine();
    for extension in ["rbxm", "rbxl", "rbxmx", "rbxlx"] {
        let dir = tempfile::tempdir().unwrap();
        let mut ours = model();
        let mut theirs = model();
        edit(&mut ours, "Target", "Anchored", true);
        edit(&mut theirs, "Target", "Transparency", 0.25_f32);
        if matches!(extension, "rbxmx" | "rbxlx") {
            edit(
                &mut ours,
                "Root",
                "FutureProperty",
                "preserve unknown XML data",
            );
            edit(
                &mut theirs,
                "Root",
                "FutureProperty",
                "preserve unknown XML data",
            );
        }
        let mut paths = write_inputs(dir.path(), extension, &ours, &theirs);
        if extension == "rbxmx" {
            paths[3] = paths[1].clone();
        }
        success(run_merge(&engine, dir.path(), &paths, &[]));
        let merged = decode(&paths[3]);
        if matches!(extension, "rbxmx" | "rbxlx") {
            assert_eq!(
                merged.get_by_ref(find(&merged, "Root")).unwrap().properties
                    [&ustr("FutureProperty")],
                Variant::String("preserve unknown XML data".into())
            );
        }
        let target = merged.get_by_ref(find(&merged, "Target")).unwrap();
        assert_eq!(target.properties[&ustr("Anchored")], Variant::Bool(true));
        assert_eq!(
            target.properties[&ustr("Transparency")],
            Variant::Float32(0.25)
        );
        assert_eq!(
            target.properties[&ustr("Size")],
            Variant::Vector3(Vector3::new(2.0, 3.0, 4.0))
        );
        assert_eq!(merged.descendants().count(), model().descendants().count());
        let pointer = merged.get_by_ref(find(&merged, "Pointer")).unwrap();
        assert_eq!(
            pointer.properties[&ustr("Value")],
            Variant::Ref(target.referent())
        );
        let code = merged.get_by_ref(find(&merged, "Code")).unwrap();
        assert_eq!(
            code.properties[&ustr("Source")],
            Variant::String("-- keep UTF-8 café\nreturn 42\n".into())
        );
        assert!(
            matches!(&target.properties[&ustr("Attributes")], Variant::Attributes(values) if values.get("KeepMe") == Some(&Variant::Bool(true)))
        );
    }
}

#[test]
#[ignore = "requires the pinned external engine in ROJO_TEST_RBX_MERGE"]
fn property_conflict_preserves_output_and_can_be_explicitly_resolved() {
    let engine = engine();
    let dir = tempfile::tempdir().unwrap();
    let mut ours = model();
    let mut theirs = model();
    edit(&mut ours, "Target", "Transparency", 0.25_f32);
    edit(&mut theirs, "Target", "Transparency", 0.75_f32);
    let paths = write_inputs(dir.path(), "rbxmx", &ours, &theirs);
    fs::write(&paths[3], b"preserve existing destination bytes").unwrap();
    let stash = dir.path().join("new conflicts");
    let result = run_merge(
        &engine,
        dir.path(),
        &paths,
        &["--stash-dir".as_ref(), stash.as_os_str()],
    );
    assert!(!result.status.success());
    assert_eq!(
        fs::read(&paths[3]).unwrap(),
        b"preserve existing destination bytes"
    );
    let report_path = stash.join("conflicts.txt");
    let report = fs::read_to_string(&report_path).unwrap();
    assert!(report.contains("kind = PropertyValue"));
    fs::write(
        &report_path,
        report.replace("resolution = unresolved", "resolution = theirs"),
    )
    .unwrap();
    success(rojo(
        &engine,
        dir.path(),
        &[
            "resolve".as_ref(),
            "--stash-dir".as_ref(),
            stash.as_os_str(),
            "--out".as_ref(),
            paths[3].as_os_str(),
        ],
    ));
    let merged = decode(&paths[3]);
    assert_eq!(
        merged
            .get_by_ref(find(&merged, "Target"))
            .unwrap()
            .properties[&ustr("Transparency")],
        Variant::Float32(0.75)
    );
}

#[test]
#[ignore = "requires the pinned external engine in ROJO_TEST_RBX_MERGE"]
fn divergent_moves_delete_modify_and_deleted_reference_targets_conflict() {
    let engine = engine();
    for kind in ["ParentMove", "DeleteModify", "RefTarget"] {
        let dir = tempfile::tempdir().unwrap();
        let mut ours = model();
        let mut theirs = model();
        match kind {
            "ParentMove" => {
                let target = find(&ours, "Target");
                let parent = find(&ours, "Right");
                ours.transfer_within(target, parent);
                let target = find(&theirs, "Target");
                let parent = find(&theirs, "Third");
                theirs.transfer_within(target, parent);
            }
            "DeleteModify" => {
                let target = find(&ours, "Target");
                ours.destroy(target);
                edit(&mut theirs, "Target", "Anchored", true);
            }
            "RefTarget" => {
                let other = find(&ours, "OtherTarget");
                edit(&mut ours, "Pointer", "Value", other);
                let other = find(&theirs, "OtherTarget");
                theirs.destroy(other);
            }
            _ => unreachable!(),
        }
        let paths = write_inputs(dir.path(), "rbxmx", &ours, &theirs);
        let report = dir.path().join("conflicts.txt");
        let result = run_merge(
            &engine,
            dir.path(),
            &paths,
            &["--conflicts-out".as_ref(), report.as_os_str()],
        );
        assert!(!result.status.success(), "{kind} should conflict");
        assert!(!paths[3].exists());
        assert!(fs::read_to_string(report)
            .unwrap()
            .contains(&format!("kind = {kind}")));
    }
}

fn git(directory: &Path, arguments: &[&std::ffi::OsStr]) -> Output {
    Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", directory.join("global-config"))
        .env("GIT_AUTHOR_NAME", "Rojo merge test")
        .env("GIT_COMMITTER_NAME", "Rojo merge test")
        .env("GIT_AUTHOR_EMAIL", "rojo-test@example.invalid")
        .env("GIT_COMMITTER_EMAIL", "rojo-test@example.invalid")
        .output()
        .unwrap()
}

#[test]
#[ignore = "requires the pinned external engine and Git on PATH"]
fn git_driver_handles_worktrees_and_quoted_paths_without_global_changes() {
    let engine = engine();
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("clone path ' $ spaced");
    let worktree = scratch.path().join("linked worktree");
    fs::create_dir(&root).unwrap();
    let tools = scratch.path().join("tool path ' $ spaced");
    fs::create_dir(&tools).unwrap();
    let copied_engine = tools.join(engine.file_name().unwrap());
    fs::copy(&engine, &copied_engine).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&copied_engine, fs::Permissions::from_mode(0o755)).unwrap();
    }
    success(git(
        &root,
        &["init".as_ref(), "--initial-branch=main".as_ref()],
    ));
    fs::write(root.join("global-config"), "# untouched global sentinel\n").unwrap();
    let asset = "model ' $name; asset.rbxmx";
    fs::write(root.join(asset), encode(&model(), "rbxmx")).unwrap();
    success(git(&root, &["add".as_ref(), "--".as_ref(), asset.as_ref()]));
    success(git(
        &root,
        &[
            "commit".as_ref(),
            "-m".as_ref(),
            "Create fixture (AI-assisted)".as_ref(),
        ],
    ));
    success(git(
        &root,
        &[
            "worktree".as_ref(),
            "add".as_ref(),
            "-b".as_ref(),
            "theirs".as_ref(),
            worktree.as_os_str(),
        ],
    ));
    fs::write(
        worktree.join("global-config"),
        "# untouched global sentinel\n",
    )
    .unwrap();
    success(rojo(
        &copied_engine,
        &worktree,
        &["install".as_ref(), "--write-gitattributes".as_ref()],
    ));
    success(rojo(&copied_engine, &root, &["doctor".as_ref()]));
    for directory in [&root, &worktree] {
        let attrs = success(git(
            directory,
            &[
                "check-attr".as_ref(),
                "merge".as_ref(),
                "--".as_ref(),
                asset.as_ref(),
            ],
        ));
        assert!(String::from_utf8_lossy(&attrs.stdout).contains("rbxdom"));
        assert_eq!(
            fs::read_to_string(directory.join("global-config")).unwrap(),
            "# untouched global sentinel\n"
        );
    }
    let mut ours = model();
    edit(&mut ours, "Target", "Anchored", true);
    fs::write(root.join(asset), encode(&ours, "rbxmx")).unwrap();
    success(git(&root, &["add".as_ref(), "--".as_ref(), asset.as_ref()]));
    success(git(
        &root,
        &[
            "commit".as_ref(),
            "-m".as_ref(),
            "Edit ours (AI-assisted)".as_ref(),
        ],
    ));
    let mut theirs = model();
    edit(&mut theirs, "Target", "Transparency", 0.25_f32);
    fs::write(worktree.join(asset), encode(&theirs, "rbxmx")).unwrap();
    success(git(
        &worktree,
        &["add".as_ref(), "--".as_ref(), asset.as_ref()],
    ));
    success(git(
        &worktree,
        &[
            "commit".as_ref(),
            "-m".as_ref(),
            "Edit theirs (AI-assisted)".as_ref(),
        ],
    ));
    success(git(
        &root,
        &[
            "merge".as_ref(),
            "theirs".as_ref(),
            "-m".as_ref(),
            "Merge fixture (AI-assisted)".as_ref(),
        ],
    ));
    let merged = decode(&root.join(asset));
    let target = merged.get_by_ref(find(&merged, "Target")).unwrap();
    assert_eq!(target.properties[&ustr("Anchored")], Variant::Bool(true));
    assert_eq!(
        target.properties[&ustr("Transparency")],
        Variant::Float32(0.25)
    );
    success(rojo(&copied_engine, &root, &["uninstall".as_ref()]));
    let attrs = success(git(
        &root,
        &[
            "check-attr".as_ref(),
            "merge".as_ref(),
            "--".as_ref(),
            asset.as_ref(),
        ],
    ));
    assert!(!String::from_utf8_lossy(&attrs.stdout).contains("rbxdom"));
}
