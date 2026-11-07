use std::process::Command;

use tempfile::tempdir;

use crate::rojo_test::{io_util::ROJO_PATH, serve_util::run_serve_test};

fn doctor(args: &[&str]) -> (bool, serde_json::Value) {
    let plugins = tempdir().unwrap();
    let output = Command::new(ROJO_PATH)
        .arg("doctor")
        .args(args)
        .arg("--plugins-dir")
        .arg(plugins.path())
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
