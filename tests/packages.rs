use std::process::Command;

use tempfile::tempdir;

#[test]
fn doctor_error_exit_keeps_json_report_machine_readable() {
    let directory = tempdir().unwrap();
    fs_err::write(directory.path().join("wally.toml"), "[broken").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rojo"))
        .args(["packages", "doctor"])
        .arg(directory.path())
        .arg("--json")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schemaVersion"], 1);
    assert_eq!(report["ok"], false);
    assert!(report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["code"] == "manifest-invalid"));
}

#[test]
fn wally_locked_mode_is_rejected_before_starting_a_manager() {
    let directory = tempdir().unwrap();
    fs_err::write(directory.path().join("wally.toml"), "[dependencies]\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rojo"))
        .args(["packages", "install"])
        .arg(directory.path())
        .args([
            "--manager",
            "wally",
            "--locked",
            "--executable",
            "nonexistent-wally-executable",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not provide a frozen/locked"));
    assert!(!directory.path().join("wally.lock").exists());
}
