use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    net::TcpListener,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn two_way_write_survives_a_git_index_lock_longer_than_five_seconds() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("default.project.json"),
        r#"{"name":"LockTest","tree":{"$path":"src"}}"#,
    )
    .unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    let source_path = dir.path().join("src/source.luau");
    fs::write(&source_path, "return 1").unwrap();
    fs::write(dir.path().join("token"), "0123456789abcdef0123456789abcdef").unwrap();
    assert!(Command::new("git")
        .arg("init")
        .arg(dir.path())
        .output()
        .unwrap()
        .status
        .success());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_rojo"))
            .arg("serve")
            .arg(dir.path())
            .args([
                "--address",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--studio-token-file",
            ])
            .arg(dir.path().join("token"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(15);
    let info: Value = loop {
        if let Ok(response) = client.get(format!("{base}/api/rojo")).send() {
            break rmp_serde::from_slice(&response.error_for_status().unwrap().bytes().unwrap())
                .unwrap();
        }
        assert!(Instant::now() < deadline, "Server did not start");
        assert!(server.0.try_wait().unwrap().is_none(), "Server exited");
        thread::sleep(Duration::from_millis(50));
    };
    let encoded_response = client
        .get(format!(
            "{base}/api/read/{}",
            info["rootInstanceId"].as_str().unwrap()
        ))
        .header("x-rojo-content-transfer", "base64")
        .send()
        .unwrap()
        .bytes()
        .unwrap();
    assert!(!encoded_response.contains(&0));
    let decoded_response = data_encoding::BASE64.decode(&encoded_response).unwrap();
    let read: Value = rmp_serde::from_slice(&decoded_response).unwrap();
    let source_id = read["instances"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, value)| value["Name"] == "source")
        .unwrap()
        .0;
    // Bad clients must get an error without applying a partial update or
    // preventing the subsequent valid Studio write from reaching disk.
    for payload in [
        vec![],
        vec![0xc0],
        vec![0xc1],
        vec![0x81, 0xa9],
        b"not messagepack".to_vec(),
    ] {
        for encoded in [false, true] {
            let mut request = client.post(format!("{base}/api/write"));
            let body = if encoded {
                request = request.header("x-rojo-content-transfer", "base64");
                data_encoding::BASE64.encode(&payload).into_bytes()
            } else {
                payload.clone()
            };
            assert_eq!(request.body(body).send().unwrap().status(), 400);
        }
    }
    assert_eq!(
        client
            .post(format!("{base}/api/write"))
            .header("x-rojo-content-transfer", "base64")
            .body("%%%invalid-base64%%%")
            .send()
            .unwrap()
            .status(),
        400
    );
    assert_eq!(fs::read_to_string(&source_path).unwrap(), "return 1");
    assert!(client
        .get(format!("{base}/api/rojo"))
        .send()
        .unwrap()
        .status()
        .is_success());
    let lock = dir.path().join(".git/index.lock");
    fs::write(&lock, "").unwrap();
    let body = rmp_serde::to_vec_named(&json!({
        "sessionId":info["sessionId"], "removed":[],
        "updated":[{"id":source_id, "changedProperties":{"Source":{"String":"return 2"}}}]
    }))
    .unwrap();
    let pending = thread::spawn(move || {
        client
            .post(format!("{base}/api/write"))
            .header("x-rojo-content-transfer", "base64")
            .body(data_encoding::BASE64.encode(&body))
            .send()
            .unwrap()
    });
    thread::sleep(Duration::from_millis(5500));
    assert!(
        !pending.is_finished(),
        "Git staging must hold the write instead of rejecting it"
    );
    assert_eq!(fs::read_to_string(&source_path).unwrap(), "return 1");
    fs::remove_file(lock).unwrap();
    assert!(pending.join().unwrap().status().is_success());
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_to_string(&source_path).unwrap() != "return 2" {
        assert!(
            Instant::now() < deadline,
            "Queued Studio source change was lost"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Rojo Test",
            "-c",
            "user.email=rojo@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn start_server(dir: &std::path::Path) -> (Server, String, reqwest::blocking::Client) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_rojo"))
            .arg("serve")
            .arg(dir)
            .args([
                "--address",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--studio-token-file",
            ])
            .arg(dir.join("token"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .no_proxy()
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(15);
    while client.get(format!("{base}/api/rojo")).send().is_err() {
        assert!(Instant::now() < deadline, "Server did not start");
        assert!(server.0.try_wait().unwrap().is_none(), "Server exited");
        thread::sleep(Duration::from_millis(50));
    }
    (server, base, client)
}

fn server_info(response: reqwest::blocking::Response) -> Value {
    let encoded = response.headers().get("x-rojo-content-transfer").is_some();
    let bytes = response.bytes().unwrap();
    let bytes = if encoded {
        data_encoding::BASE64.decode(&bytes).unwrap()
    } else {
        bytes.to_vec()
    };
    rmp_serde::from_slice(&bytes).unwrap()
}

#[test]
fn commits_keep_sync_alive_but_branch_switches_and_rebinding_hosts_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("default.project.json"),
        r#"{"name":"IdentityTest","tree":{"$path":"src"}}"#,
    )
    .unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/source.luau"), "return 1").unwrap();
    fs::write(dir.path().join("token"), "0123456789abcdef0123456789abcdef").unwrap();
    git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
    git(dir.path(), &["add", "default.project.json", "src"]);
    git(dir.path(), &["commit", "--quiet", "-m", "initial"]);
    let (_server, base, client) = start_server(dir.path());

    let info = server_info(client.get(format!("{base}/api/rojo")).send().unwrap());
    let pairing = &info["studioBridge"];
    assert!(pairing["pluginToken"].is_string());
    // The unauthenticated pairing response must not reveal local paths.
    assert!(
        !pairing.to_string().contains("IdentityTest")
            && pairing["identity"].get("workspacePath").is_none()
    );
    // A DNS rebinding page reaches the server under its own host name.
    let rebound = server_info(
        client
            .get(format!("{base}/api/rojo"))
            .header("host", "attacker.example:34872")
            .send()
            .unwrap(),
    );
    assert!(rebound.get("studioBridge").is_none(), "{rebound:?}");

    fs::write(dir.path().join("src/source.luau"), "return 2").unwrap();
    git(dir.path(), &["commit", "--quiet", "-am", "second"]);
    let response = client
        .get(format!("{base}/api/rojo"))
        .header("x-rojo-content-transfer", "base64")
        .send()
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());

    git(dir.path(), &["checkout", "--quiet", "-b", "other"]);
    let response = client
        .get(format!("{base}/api/rojo"))
        .header("x-rojo-content-transfer", "base64")
        .send()
        .unwrap();
    assert_eq!(response.status(), 409);
    // Errors honour the requested transfer encoding so Studio can decode them.
    assert_eq!(
        response.headers().get("x-rojo-content-transfer").unwrap(),
        "base64"
    );
    let error = server_info(response);
    assert_eq!(error["kind"], "BadRequest");
}

#[test]
fn every_response_to_a_base64_request_is_encoded() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("default.project.json"),
        r#"{"name":"EncodingTest","tree":{"$className":"Folder"}}"#,
    )
    .unwrap();
    fs::write(dir.path().join("token"), "0123456789abcdef0123456789abcdef").unwrap();
    let (_server, base, client) = start_server(dir.path());
    let decoded = |response: reqwest::blocking::Response| {
        assert_eq!(
            response.headers().get("x-rojo-content-transfer").unwrap(),
            "base64"
        );
        let status = response.status();
        let body = data_encoding::BASE64
            .decode(&response.bytes().unwrap())
            .unwrap();
        (status, body)
    };

    // A cross-origin rejection stays a generic 404 that does not identify
    // Rojo, so it is the one response left unencoded.
    let rejected = client
        .get(format!("{base}/api/rojo"))
        .header("origin", "http://attacker.example")
        .header("x-rojo-content-transfer", "base64")
        .send()
        .unwrap();
    assert_eq!(rejected.status(), 404);
    assert!(rejected.headers().get("x-rojo-content-transfer").is_none());

    let (status, _) = decoded(
        client
            .get(format!("{base}/api/missing"))
            .header("x-rojo-content-transfer", "base64")
            .send()
            .unwrap(),
    );
    assert_eq!(status, 404);

    // Studio bridge routes reply in JSON, encoded the same way.
    let (status, body) = decoded(
        client
            .get(format!("{base}/api/studio/sessions"))
            .header("x-rojo-studio-token", "0123456789abcdef0123456789abcdef")
            .header("x-rojo-content-transfer", "base64")
            .send()
            .unwrap(),
    );
    assert_eq!(status, 200);
    let sessions: Value = serde_json::from_slice(&body).unwrap();
    assert!(sessions["sessions"].as_array().unwrap().is_empty());
    let (status, body) = decoded(
        client
            .post(format!("{base}/api/studio/register"))
            .header("x-rojo-studio-token", "wrong-token")
            .header("x-rojo-content-transfer", "base64")
            .body(data_encoding::BASE64.encode(b"{}"))
            .send()
            .unwrap(),
    );
    assert_eq!(status, 401);
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert!(error["error"].is_string());
}

/// A binary model with one Folder whose only property is `raw_attributes`,
/// written by hand because rbx_binary cannot write malformed attributes.
fn folder_with_attributes(raw_attributes: &[u8]) -> Vec<u8> {
    fn string(out: &mut Vec<u8>, value: &[u8]) {
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
    }
    fn chunk(out: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(name);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(data);
    }
    let mut model = b"<roblox!\x89\xff\x0d\x0a\x1a\x0a\x00\x00".to_vec();
    model.extend_from_slice(&1u32.to_le_bytes());
    model.extend_from_slice(&1u32.to_le_bytes());
    model.extend_from_slice(&[0; 8]);
    let mut inst = 0u32.to_le_bytes().to_vec();
    string(&mut inst, b"Folder");
    inst.push(0);
    inst.extend_from_slice(&1u32.to_le_bytes());
    // Referent 0, interleaved and zigzag encoded.
    inst.extend_from_slice(&[0, 0, 0, 0]);
    chunk(&mut model, b"INST", &inst);
    let mut prop = 0u32.to_le_bytes().to_vec();
    string(&mut prop, b"AttributesSerialize");
    prop.push(0x01);
    string(&mut prop, raw_attributes);
    chunk(&mut model, b"PROP", &prop);
    // Instance 0's parent is -1 (none).
    let mut prnt = vec![0];
    prnt.extend_from_slice(&1u32.to_le_bytes());
    prnt.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
    chunk(&mut model, b"PRNT", &prnt);
    chunk(&mut model, b"END\0", b"</roblox>");
    model
}

/// One NumberSequence attribute declaring 2^32 - 1 keypoints in 13 bytes. The
/// attribute reader reserves memory for every keypoint before reading any.
const ATTRIBUTE_BOMB: &[u8] = b"\x01\x00\x00\x00\x00\x00\x00\x00\x17\xff\xff\xff\xff";

#[test]
fn models_that_crash_the_decoder_fail_only_their_own_check() {
    let check = |model: &[u8], encode: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rojo"));
        command.arg("check-untrusted-model");
        if encode {
            command.arg("--encode");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(model).unwrap();
        child.wait_with_output().unwrap()
    };
    let valid = folder_with_attributes(b"\x00\x00\x00\x00");
    let output = check(&valid, true);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.starts_with(b"<roblox!"));
    let output = check(&folder_with_attributes(ATTRIBUTE_BOMB), false);
    assert!(
        !output.status.success() && output.status.code() != Some(3),
        "{output:?}"
    );
    let output = check(
        b"<roblox version=\"4\"><Item class=\"Script\"></Item></roblox>",
        false,
    );
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("scripts"));
}

#[test]
fn a_model_that_crashes_the_decoder_does_not_stop_serve() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("default.project.json"),
        r#"{"name":"ImportTest","tree":{"$className":"Folder"}}"#,
    )
    .unwrap();
    let token = "0123456789abcdef0123456789abcdef";
    fs::write(dir.path().join("token"), token).unwrap();
    let (mut server, base, client) = start_server(dir.path());
    let pairing =
        server_info(client.get(format!("{base}/api/rojo")).send().unwrap())["studioBridge"].clone();
    let session = uuid::Uuid::new_v4().to_string();
    let registered = client
        .post(format!("{base}/api/studio/register"))
        .header(
            "x-rojo-studio-token",
            pairing["pluginToken"].as_str().unwrap(),
        )
        .json(&json!({"sessionId":session,"description":{"placeName":"test"},"identity":pairing["identity"]}))
        .send()
        .unwrap();
    assert!(registered.status().is_success(), "{registered:?}");
    for (model, expected) in [
        (folder_with_attributes(ATTRIBUTE_BOMB), "could not be decoded"),
        (
            b"<!DOCTYPE r [<!ENTITY o \"<Item class='Folder'>\">]><roblox version=\"4\">&o;</roblox>"
                .to_vec(),
            "document type",
        ),
    ] {
        let response = client
            .post(format!("{base}/api/studio/command"))
            .header("x-rojo-studio-token", token)
            .json(&json!({"sessionId":session,"action":"import","args":{"parent":"game/Workspace","model":data_encoding::BASE64.encode(&model)}}))
            .send()
            .unwrap();
        assert_eq!(response.status(), 400);
        let body: Value = response.json().unwrap();
        assert!(
            body["error"].as_str().unwrap().contains(expected),
            "{body}"
        );
    }
    assert!(server.0.try_wait().unwrap().is_none(), "Server exited");
    assert!(client
        .get(format!("{base}/api/rojo"))
        .send()
        .unwrap()
        .status()
        .is_success());
}
