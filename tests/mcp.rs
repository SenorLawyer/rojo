use std::{
    io::{BufRead, Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};

fn fixture() -> TempDir {
    let directory = tempdir().unwrap();
    fs_err::write(directory.path().join("wally.toml"), "[dependencies]\n").unwrap();
    fs_err::write(
        directory.path().join("default.project.json"),
        r#"{"name":"mcp-test","tree":{"$className":"Folder"}}"#,
    )
    .unwrap();
    directory
}

fn initialize(version: &str) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}})
}

fn initialized() -> Value {
    json!({"jsonrpc":"2.0","method":"notifications/initialized"})
}

fn exchange(directory: &std::path::Path, input: &[u8]) -> Vec<Value> {
    // Lock and backup state outside Git goes to a per-user folder, which a
    // test must not touch.
    let state = directory.join(".test-state");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rojo"))
        .args(["mcp", "--project"])
        .arg(directory)
        .env("LOCALAPPDATA", &state)
        .env("XDG_STATE_HOME", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("stdout must contain only JSON-RPC messages"))
        .collect()
}

/// Tool calls run concurrently, so responses are matched by ID, not position.
fn reply(replies: &[Value], id: i64) -> &Value {
    replies
        .iter()
        .find(|reply| reply["id"] == id)
        .unwrap_or_else(|| panic!("No reply with id {id}: {replies:?}"))
}

fn exchange_messages(directory: &std::path::Path, messages: &[Value]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut input, message).unwrap();
        input.push(b'\n');
    }
    exchange(directory, &input)
}

#[test]
fn stdio_handshake_catalog_native_call_and_error_recovery() {
    let directory = fixture();
    let replies = exchange_messages(
        directory.path(),
        &[
            json!({"jsonrpc":"2.0","id":0,"method":"tools/list"}),
            initialize("2025-06-18"),
            initialized(),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"packages_doctor","arguments":{}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"studio_status","arguments":{}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"studio_status","arguments":{"session":"00000000-0000-0000-0000-000000000001"}}}),
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"shell","arguments":{"command":"whoami"}}}),
            json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"packages_doctor","arguments":{"project":"elsewhere"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/unknown"}),
            json!({"jsonrpc":"2.0","id":8,"method":"ping"}),
        ],
    );
    assert_eq!(replies.len(), 9);
    assert_eq!(reply(&replies, 0)["error"]["code"], -32000);
    assert_eq!(
        reply(&replies, 1)["result"]["protocolVersion"],
        "2025-06-18"
    );
    let names: Vec<_> = reply(&replies, 2)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 15);
    assert!(names.contains(&"studio_test") && names.contains(&"run_tests"));
    assert!(names.contains(&"studio_verify") && names.contains(&"studio_backup"));
    assert!(names.contains(&"studio_lock_acquire") && names.contains(&"studio_lock_status"));
    assert!(names.contains(&"studio_export") && names.contains(&"workspace_list"));
    assert!(!names.iter().any(|name| name.contains("publish")
        || name.contains("import")
        || name.contains("install")));
    assert_eq!(reply(&replies, 3)["result"]["isError"], false);
    assert_eq!(
        reply(&replies, 3)["result"]["structuredContent"]["schemaVersion"],
        1
    );
    assert_eq!(reply(&replies, 4)["error"]["code"], -32602);
    let studio = &reply(&replies, 5)["result"];
    assert_eq!(studio["isError"], true);
    assert!(studio["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("--token-file"));
    // Studio-sourced text is labelled as untrusted for the agent.
    assert!(studio["content"][1]["text"]
        .as_str()
        .unwrap()
        .contains("untrusted"));
    assert_eq!(
        reply(&replies, 3)["result"]["content"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(reply(&replies, 6)["error"]["code"], -32602);
    assert_eq!(reply(&replies, 7)["error"]["code"], -32602);
    assert_eq!(reply(&replies, 8)["result"], json!({}));
}

#[test]
fn older_protocol_uses_text_content_and_unknown_versions_negotiate_latest() {
    let directory = fixture();
    let replies = exchange_messages(
        directory.path(),
        &[
            initialize("2024-11-05"),
            initialized(),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"packages_doctor"}}),
        ],
    );
    assert_eq!(
        reply(&replies, 1)["result"]["protocolVersion"],
        "2024-11-05"
    );
    assert!(reply(&replies, 2)["result"]
        .get("structuredContent")
        .is_none());
    let text = reply(&replies, 2)["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap()["schemaVersion"],
        1
    );
    let replies = exchange_messages(directory.path(), &[initialize("2030-01-01")]);
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
}

/// The listing is the decided split with Roblox's Studio MCP server: tools it
/// duplicates are left out, but stay callable for agents that already use them.
#[test]
fn listing_leaves_out_studio_mcp_duplicates_that_stay_callable() {
    let directory = fixture();
    let session = "00000000-0000-0000-0000-000000000001";
    let replies = exchange_messages(
        directory.path(),
        &[
            initialize("2025-06-18"),
            initialized(),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"studio_tree","arguments":{"session":session,"depth":1}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"studio_properties","arguments":{"session":session,"instance":"game/Workspace","properties":["Name"]}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"studio_tree","arguments":{"session":session,"depth":7}}}),
        ],
    );
    let instructions = reply(&replies, 1)["result"]["instructions"]
        .as_str()
        .unwrap();
    assert!(instructions.contains("open_cloud_invoke"));
    assert!(instructions.contains("studio_verify"));
    let tools = reply(&replies, 2)["result"]["tools"].as_array().unwrap();
    let mut names: Vec<_> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "checkpoint_list",
            "packages_doctor",
            "run_tests",
            "studio_backup",
            "studio_backup_list",
            "studio_export",
            "studio_lock_acquire",
            "studio_lock_release",
            "studio_lock_status",
            "studio_logs",
            "studio_sessions",
            "studio_status",
            "studio_test",
            "studio_verify",
            "workspace_list",
        ]
    );
    let description = |name: &str| {
        tools.iter().find(|tool| tool["name"] == name).unwrap()["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    // Tools that overlap with Roblox's point agents at its equivalents.
    assert!(description("studio_test").contains("start_stop_play"));
    assert!(description("studio_logs").contains("get_console_output"));
    assert!(description("studio_sessions").contains("list_roblox_studios"));
    // Unlisted tools still reach the native command (which fails here for
    // want of a token) and still validate their arguments.
    for id in [3, 4] {
        let call = &reply(&replies, id)["result"];
        assert_eq!(call["isError"], true, "{call}");
        assert!(call["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("--token-file"));
    }
    assert_eq!(reply(&replies, 5)["error"]["code"], -32602);
}

#[test]
fn native_test_tool_requires_a_session_and_bounded_seconds() {
    let directory = fixture();
    let replies = exchange_messages(
        directory.path(),
        &[
            initialize("2025-06-18"),
            initialized(),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"studio_test","arguments":{"session":"00000000-0000-0000-0000-000000000001","seconds":6}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"studio_test","arguments":{"session":"--help","seconds":1}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"studio_test","arguments":{"session":"00000000-0000-0000-0000-000000000001","seconds":1}}}),
        ],
    );
    let test = reply(&replies, 2)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "studio_test")
        .unwrap();
    assert_eq!(test["annotations"]["readOnlyHint"], false);
    // Existing and synced scripts run in game context and can reach services.
    assert_eq!(test["annotations"]["destructiveHint"], true);
    assert_eq!(test["annotations"]["openWorldHint"], true);
    assert_eq!(reply(&replies, 3)["error"]["code"], -32602);
    assert_eq!(reply(&replies, 4)["error"]["code"], -32602);
    assert_eq!(reply(&replies, 5)["result"]["isError"], true);
    assert!(reply(&replies, 5)["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("--token-file"));
}

#[test]
fn oversized_or_malformed_input_does_not_corrupt_the_next_message() {
    let directory = fixture();
    let mut input = vec![b'x'; 65 * 1024];
    input
        .extend_from_slice(b"\nnot-json\n[]\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");
    let replies = exchange(directory.path(), &input);
    assert_eq!(replies.len(), 4);
    assert_eq!(replies[0]["error"]["code"], -32600);
    assert_eq!(replies[1]["error"]["code"], -32700);
    assert_eq!(replies[2]["error"]["code"], -32600);
    assert_eq!(replies[3]["id"], 2);
    assert_eq!(replies[3]["result"], json!({}));
}

#[test]
fn blank_lines_are_ignored() {
    let directory = fixture();
    let replies = exchange(
        directory.path(),
        b"\n  \r\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n\n",
    );
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["id"], 3);
}

#[test]
fn native_output_limit_returns_a_tool_error_and_keeps_stdio_usable() {
    let directory = fixture();
    fs_err::create_dir(directory.path().join("Packages")).unwrap();
    let nodes: serde_json::Map<_, _> = (0..3000)
        .map(|index| {
            (
                format!("PackageMapping{index}"),
                json!({"$path":"Packages"}),
            )
        })
        .collect();
    fs_err::write(directory.path().join("default.project.json"), serde_json::to_vec(&json!({"name":"large-report","tree":{"$className":"DataModel","ReplicatedStorage":Value::Object(nodes)}})).unwrap()).unwrap();
    let replies = exchange_messages(
        directory.path(),
        &[
            initialize("2025-06-18"),
            initialized(),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"packages_doctor"}}),
            json!({"jsonrpc":"2.0","id":3,"method":"ping"}),
        ],
    );
    assert_eq!(reply(&replies, 2)["result"]["isError"], true);
    assert!(reply(&replies, 2)["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("256 KiB"));
    assert_eq!(reply(&replies, 3)["result"], json!({}));
}

/// Accepts connections and records request lines without ever responding,
/// standing in for a Studio bridge whose command never completes.
fn unresponsive_bridge() -> (String, Arc<Mutex<Vec<String>>>) {
    recording_bridge(true)
}

/// Records request lines, then keeps each connection open without answering
/// or, when `hold` is false, closes it so the request fails.
fn recording_bridge(hold: bool) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    thread::spawn(move || {
        let mut open = Vec::new();
        for mut stream in listener.incoming().flatten() {
            let mut head = Vec::new();
            let mut byte = [0; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
            }
            let line = String::from_utf8_lossy(&head)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            recorded.lock().unwrap().push(line);
            if hold {
                open.push(stream);
            }
        }
    });
    (address, seen)
}

#[test]
fn ping_is_answered_during_a_slow_call_and_cancellation_reaches_the_bridge() {
    let directory = fixture();
    let token = directory.path().join("token");
    fs_err::write(&token, "0123456789abcdef0123456789abcdef").unwrap();
    let (address, seen) = unresponsive_bridge();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rojo"))
        .args(["mcp", "--project"])
        .arg(directory.path())
        .arg(format!("--address={address}"))
        .arg("--token-file")
        .arg(&token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut send = |message: Value| {
        serde_json::to_writer(&mut input, &message).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
    };
    let mut receive = || {
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    send(initialize("2025-06-18"));
    assert_eq!(receive()["id"], 1);
    send(initialized());
    let started = Instant::now();
    send(
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"studio_status","arguments":{"session":"00000000-0000-0000-0000-000000000001"}}}),
    );
    send(json!({"jsonrpc":"2.0","id":3,"method":"ping"}));
    assert_eq!(receive()["id"], 3);
    assert!(started.elapsed() < Duration::from_secs(10));
    // Let the native command reach the bridge before cancelling it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("/api/studio/command"))
    {
        assert!(
            Instant::now() < deadline,
            "The tool call never reached the bridge"
        );
        thread::sleep(Duration::from_millis(20));
    }
    send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}));
    drop(input);
    let mut rest = String::new();
    output.read_to_string(&mut rest).unwrap();
    assert!(child.wait().unwrap().success());
    // A cancelled request receives no response.
    assert!(rest.trim().is_empty(), "{rest}");
    assert!(started.elapsed() < Duration::from_secs(20));
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|line| line.starts_with("POST /api/studio/cancel")),
        "{seen:?}"
    );
}

#[test]
fn failed_studio_calls_are_cancelled_at_the_bridge() {
    let directory = fixture();
    let token = directory.path().join("token");
    fs_err::write(&token, "0123456789abcdef0123456789abcdef").unwrap();
    // The connection drops, so the command may or may not have been queued.
    let (address, seen) = recording_bridge(false);
    let mut child = Command::new(env!("CARGO_BIN_EXE_rojo"))
        .args(["mcp", "--project"])
        .arg(directory.path())
        .arg(format!("--address={address}"))
        .arg("--token-file")
        .arg(&token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        initialize("2025-06-18"),
        initialized(),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"studio_status","arguments":{"session":"00000000-0000-0000-0000-000000000001"}}}),
    ] {
        serde_json::to_writer(&mut input, &message).unwrap();
        input.write_all(b"\n").unwrap();
    }
    drop(input);
    let mut output = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert!(child.wait().unwrap().success());
    let replies: Vec<Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(reply(&replies, 2)["result"]["isError"], true);
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|line| line.starts_with("POST /api/studio/cancel")),
        "{seen:?}"
    );
}

#[test]
fn lock_and_backup_list_tools_work_without_a_bridge_token() {
    let directory = fixture();
    let call = |id: i64, name: &str, arguments: Value| {
        let replies = exchange_messages(
            directory.path(),
            &[
                initialize("2025-06-18"),
                initialized(),
                json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
            ],
        );
        reply(&replies, id)["result"].clone()
    };
    let granted = call(
        2,
        "studio_lock_acquire",
        json!({"lock":"main","owner":"agent-a"}),
    );
    assert_eq!(granted["isError"], false, "{granted}");
    assert_eq!(granted["structuredContent"]["granted"], true);
    // Calls run concurrently within one session, so the busy check uses its own.
    let busy = call(
        3,
        "studio_lock_acquire",
        json!({"lock":"main","owner":"agent-b","exclusive":true}),
    );
    assert_eq!(busy["isError"], false, "{busy}");
    assert_eq!(busy["structuredContent"]["granted"], false);
    let status = call(4, "studio_lock_status", json!({}));
    assert_eq!(status["isError"], false, "{status}");
    assert_eq!(
        status["structuredContent"]["locks"][0]["holders"][0]["owner"],
        "agent-a"
    );
    let released = call(
        5,
        "studio_lock_release",
        json!({"lock":"main","owner":"agent-a"}),
    );
    assert_eq!(
        released["structuredContent"]["released"], true,
        "{released}"
    );
    let backups = call(6, "studio_backup_list", json!({}));
    assert_eq!(backups["isError"], false, "{backups}");
    assert_eq!(backups["structuredContent"]["backups"], json!([]));
}
