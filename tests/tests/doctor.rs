use std::process::Command;

use tempfile::tempdir;

use crate::rojo_test::{io_util::ROJO_PATH, serve_util::run_serve_test};

fn doctor(args: &[&str]) -> (bool, serde_json::Value) {
    let plugins = tempdir().unwrap();
    doctor_with_plugins(args, plugins.path())
}

fn doctor_with_plugins(args: &[&str], plugins: &std::path::Path) -> (bool, serde_json::Value) {
    let output = Command::new(ROJO_PATH)
        .arg("doctor")
        .args(args)
        .arg("--plugins-dir")
        .arg(plugins)
        .arg("--json")
        .output()
        .unwrap();
    let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "doctor did not print JSON ({err}):\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.success(), report)
}

fn statuses(report: &serde_json::Value, category: &str) -> Vec<(String, String)> {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["category"] == category)
        .map(|check| {
            (
                check["code"].as_str().unwrap().to_owned(),
                check["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn doctor_reports_a_running_server() {
    run_serve_test("empty", |session, _| {
        let port = session.port().to_string();
        let project = session.path().to_str().unwrap().to_owned();
        let (success, report) = doctor(&[&project, "--port", &port]);
        assert!(success, "{report:#}");
        assert_eq!(report["schemaVersion"], 1);
        assert_eq!(report["ok"], true);
        let server = statuses(&report, "server");
        assert_eq!(
            server[0],
            ("server-current".to_owned(), "ok".to_owned()),
            "{server:?}"
        );
        assert!(!server.iter().any(|(code, _)| code == "other-project"));
    });
}

#[test]
fn doctor_exits_with_1_and_still_reports_when_the_project_is_missing() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("missing");
    // Nothing listens on port 1, so the server check does not depend on
    // whatever runs on the default port.
    let (success, report) = doctor(&[missing.to_str().unwrap(), "--port", "1"]);
    assert!(!success);
    assert_eq!(report["ok"], false);
    assert_eq!(
        statuses(&report, "project"),
        [("project-invalid".to_owned(), "error".to_owned())]
    );
    assert_eq!(
        statuses(&report, "server"),
        [("not-running".to_owned(), "info".to_owned())]
    );
}

#[test]
fn doctor_reports_a_corrupted_plugin_without_crashing() {
    let plugins = tempdir().unwrap();
    let status = Command::new(ROJO_PATH)
        .args(["plugin", "install", "--plugins-dir"])
        .arg(plugins.path())
        .output()
        .unwrap()
        .status;
    assert!(status.success());
    let path = plugins.path().join("RojoManagedPlugin.rbxm");

    // The installed plugin passes the untrusted-model check in the child
    // process that the real binary runs.
    let (_, report) = doctor_with_plugins(&["--port", "1"], plugins.path());
    assert_eq!(
        statuses(&report, "plugin"),
        [("plugin-current".to_owned(), "ok".to_owned())]
    );

    // A header that declares about four billion instances would make
    // rbx_binary try to reserve memory for all of them.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();
    let (_, report) = doctor_with_plugins(&["--port", "1"], plugins.path());
    assert_eq!(
        statuses(&report, "plugin"),
        [("plugin-unreadable".to_owned(), "warning".to_owned())]
    );
}
