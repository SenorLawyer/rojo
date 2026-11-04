//! `rojo test` and the MCP `run_tests` tool against a stand-in Studio bridge,
//! checking exit codes, output and what is sent to the bridge.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread,
};

use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};

const SESSION: &str = "00000000-0000-4000-8000-000000000001";

fn read_request(stream: &mut TcpStream) -> (String, Value) {
    let mut head = Vec::new();
    let mut byte = [0; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    let line = head.lines().next().unwrap_or_default().to_owned();
    (line, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Answers the session listing with `sessions` and every command with
/// `status` and `body`, recording each command it receives.
fn bridge(sessions: Value, status: u16, body: Value) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let commands = Arc::new(Mutex::new(Vec::new()));
    let recorded = commands.clone();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let (line, request) = read_request(&mut stream);
            let (status, response) = if line.starts_with("GET /api/studio/sessions") {
                (200, json!({"protocolVersion":1,"sessions":sessions}))
            } else {
                recorded.lock().unwrap().push(request);
                (status, body.clone())
            };
            let response = response.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
        }
    });
    (address, commands)
}

fn report(status: &str, failed: u64) -> Value {
    json!({
        "sessionId": SESSION,
        "result": {"ok": true, "value": {"mode": "Run", "report": {
            "framework": "testez",
            "status": status,
            "passed": 2,
            "failed": failed,
            "skipped": 0,
            "modules": 1,
            "failures": if failed > 0 {
                json!([{"path": ["Shared", "Math", "subtracts"], "messages": ["expected 1, got 2"]}])
            } else {
                json!({})
            },
            "loadErrors": {},
            "durationSeconds": 0.5,
        }}},
    })
}

fn token() -> TempDir {
    let directory = tempdir().unwrap();
    let path = directory.path().join("token");
    fs_err::write(&path, "0123456789abcdef0123456789abcdef").unwrap();
    // Rojo warns on stderr about token files other users can read, which
    // tests that expect a quiet stderr would otherwise see on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs_err::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    directory
}

fn rojo_test(address: &str, token: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rojo"))
        .arg("test")
        .arg(format!("--address={address}"))
        .arg("--token-file")
        .arg(token.join("token"))
        .args(extra)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn one_session() -> Value {
    json!([{"sessionId": SESSION, "placeName": "Game", "placeId": 1}])
}

#[test]
fn passing_runs_exit_0_and_send_only_options() {
    let directory = token();
    let (address, commands) = bridge(one_session(), 200, report("passed", 0));
    let output = rojo_test(
        &address,
        directory.path(),
        &[
            "--root",
            "game/ReplicatedStorage/Shared",
            "--filter",
            "Math",
            "--timeout",
            "120",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.starts_with("PASSED: 2 passed, 0 failed, 0 skipped"),
        "{stdout}"
    );
    let commands = commands.lock().unwrap();
    assert_eq!(commands.len(), 1);
    // The only session was chosen, and the options went as data.
    assert_eq!(commands[0]["sessionId"], SESSION);
    assert_eq!(commands[0]["action"], "spec");
    assert_eq!(
        commands[0]["args"],
        json!({"framework":"auto","timeoutSeconds":120,"roots":["game/ReplicatedStorage/Shared"],"filter":"Math"})
    );
}

#[test]
fn failing_runs_exit_1_with_a_json_report() {
    let directory = token();
    let (address, _) = bridge(one_session(), 200, report("failed", 1));
    let output = rojo_test(&address, directory.path(), &["--json"]);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "failed");
    assert_eq!(value["sessionId"], SESSION);
    assert_eq!(
        value["failures"][0]["path"],
        json!(["Shared", "Math", "subtracts"])
    );
    // A failed run is reported once, on stdout, not again as an error log.
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn plugin_errors_and_studio_timeouts_exit_1() {
    let directory = token();
    for (status, body) in [
        (
            200,
            json!({"result":{"ok":false,"error":"Could not find a ModuleScript named TestEZ"}}),
        ),
        (
            504,
            json!({"error":"Studio did not complete the command within 90 seconds."}),
        ),
    ] {
        let (address, _) = bridge(one_session(), status, body);
        let output = rojo_test(&address, directory.path(), &["--json"]);
        assert_eq!(output.status.code(), Some(1));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["status"], "error");
        assert!(value["error"].as_str().unwrap().len() > 10, "{value}");
    }
}

#[test]
fn usage_and_connection_problems_exit_2() {
    let directory = token();
    // Several sessions and no --session.
    let (address, commands) = bridge(
        json!([
            {"sessionId": SESSION, "placeName": "One", "placeId": 1},
            {"sessionId": "00000000-0000-4000-8000-000000000002", "placeName": "Two", "placeId": 2},
        ]),
        200,
        report("passed", 0),
    );
    let output = rojo_test(&address, directory.path(), &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--session"));
    assert!(commands.lock().unwrap().is_empty());

    // The bridge refused the command.
    let (address, _) = bridge(
        one_session(),
        401,
        json!({"error":"Invalid Studio control token"}),
    );
    assert_eq!(
        rojo_test(&address, directory.path(), &[]).status.code(),
        Some(2)
    );

    // Nothing is listening.
    let unused = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", unused.local_addr().unwrap());
    drop(unused);
    assert_eq!(
        rojo_test(&address, directory.path(), &[]).status.code(),
        Some(2)
    );

    // Out-of-range timeout.
    assert_eq!(
        rojo_test(&address, directory.path(), &["--timeout", "301"])
            .status
            .code(),
        Some(2)
    );
}

/// Calls the MCP `run_tests` tool once against a bridge answering every
/// command with `body`, and returns the tool result and the commands sent.
fn mcp_run_tests(body: Value) -> (Value, Vec<Value>) {
    let directory = token();
    fs_err::write(
        directory.path().join("default.project.json"),
        r#"{"name":"mcp-test","tree":{"$className":"Folder"}}"#,
    )
    .unwrap();
    let (address, commands) = bridge(one_session(), 200, body);
    let mut child = Command::new(env!("CARGO_BIN_EXE_rojo"))
        .args(["mcp", "--project"])
        .arg(directory.path())
        .arg(format!("--address={address}"))
        .arg("--token-file")
        .arg(directory.path().join("token"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run_tests","arguments":{"session":SESSION,"roots":["game/ReplicatedStorage/Shared"],"timeout":5}}}),
    ] {
        serde_json::to_writer(&mut input, &message).unwrap();
        input.write_all(b"\n").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    let replies: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let reply = replies
        .iter()
        .find(|reply| reply["id"] == 2)
        .expect("run_tests reply");
    let commands = commands.lock().unwrap().clone();
    (reply["result"].clone(), commands)
}

#[test]
fn mcp_run_tests_returns_failed_runs_as_results() {
    let (result, commands) = mcp_run_tests(report("failed", 1));
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["structuredContent"]["status"], "failed");
    assert_eq!(result["structuredContent"]["failed"], 1);
    // Test failures and messages come from the place's scripts.
    assert!(result["content"][1]["text"]
        .as_str()
        .unwrap()
        .contains("untrusted"));
    assert_eq!(commands[0]["action"], "spec");
    assert_eq!(commands[0]["args"]["timeoutSeconds"], 5);
    assert_eq!(commands[0]["sessionId"], SESSION);
}

#[test]
fn mcp_run_tests_cuts_a_large_report_instead_of_failing() {
    // 250 failures with long tracebacks: within the runner's own bounds, but
    // larger than the 256 KiB the adapter reads from a command.
    let traceback = "ReplicatedStorage.Shared.Util:12 function helper\n".repeat(20);
    let failures: Vec<Value> = (0..250)
        .map(|index| {
            json!({
                "path": ["Shared", "Math", format!("test {index}")],
                "messages": [format!("ReplicatedStorage.Shared.Math.spec:{index}: Expected value 3, got 2
{traceback}")],
            })
        })
        .collect();
    let mut body = report("failed", 250);
    body["result"]["value"]["report"]["failures"] = json!(failures);
    assert!(body.to_string().len() > 256 * 1024);

    let (result, _) = mcp_run_tests(body);
    assert_eq!(result["isError"], false, "{}", result["content"][0]);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.len() <= 256 * 1024, "{}", text.len());
    let report: Value = serde_json::from_str(text).unwrap();
    assert_eq!(report, result["structuredContent"]);
    assert_eq!(report["status"], "failed");
    assert_eq!(report["failed"], 250);
    let listed = report["failures"].as_array().unwrap().len() as u64;
    assert!(listed > 100, "{listed}");
    assert_eq!(listed + report["omittedFailures"].as_u64().unwrap(), 250);
}

#[test]
fn invalid_options_exit_2_without_reaching_studio() {
    let directory = token();
    let (address, commands) = bridge(one_session(), 200, report("passed", 0));
    let roots: Vec<String> = (0..33)
        .map(|index| format!("--root=game/R{index}"))
        .collect();
    let roots: Vec<&str> = roots.iter().map(String::as_str).collect();
    let long_filter = format!("--filter={}", "a".repeat(257));
    for (extra, expected) in [
        (
            &["--root", "ReplicatedStorage/Shared"][..],
            "--root must be a game/",
        ),
        // The runner would reject the empty name only once Studio had it.
        (&["--root=game/"][..], "--root must be a game/"),
        (&roots[..], "at most 32 times"),
        (&[long_filter.as_str()][..], "--filter must be at most 256"),
        (&["--module="][..], "--module must be a game/"),
    ] {
        let output = rojo_test(&address, directory.path(), extra);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
    }
    assert!(commands.lock().unwrap().is_empty());
}
