//! The Studio plugin sends local requests over HTTP streams, which cut binary
//! bodies at NUL bytes, so it opts into base64 bodies with the
//! `x-rojo-content-transfer` header. These tests check that `rojo serve`
//! honours that header for every response the plugin can receive.

use serde_json::Value;
use std::{
    fs,
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

fn start_server(dir: &std::path::Path) -> (Server, String, reqwest::blocking::Client) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_rojo"))
            .arg("serve")
            .arg(dir)
            .args(["--address", "127.0.0.1", "--port", &port.to_string()])
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

fn decoded(response: reqwest::blocking::Response) -> (reqwest::StatusCode, Vec<u8>) {
    assert_eq!(
        response.headers().get("x-rojo-content-transfer").unwrap(),
        "base64"
    );
    let status = response.status();
    let body = data_encoding::BASE64
        .decode(&response.bytes().unwrap())
        .unwrap();
    (status, body)
}

#[test]
fn every_response_to_a_base64_request_is_encoded() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("default.project.json"),
        r#"{"name":"EncodingTest","tree":{"$className":"Folder"}}"#,
    )
    .unwrap();
    let (_server, base, client) = start_server(dir.path());

    let (status, body) = decoded(
        client
            .get(format!("{base}/api/rojo"))
            .header("x-rojo-content-transfer", "base64")
            .send()
            .unwrap(),
    );
    assert_eq!(status, 200);
    let info: Value = rmp_serde::from_slice(&body).unwrap();
    assert_eq!(info["projectName"], "EncodingTest");

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

    // Errors are encoded too, because the plugin decodes the body before it
    // looks at the status.
    let (status, body) = decoded(
        client
            .get(format!("{base}/api/missing"))
            .header("x-rojo-content-transfer", "base64")
            .send()
            .unwrap(),
    );
    assert_eq!(status, 404);
    let error: Value = rmp_serde::from_slice(&body).unwrap();
    assert_eq!(error["kind"], "NotFound");

    let (status, body) = decoded(
        client
            .post(format!("{base}/api/write"))
            .header("x-rojo-content-transfer", "base64")
            .body("not base64!")
            .send()
            .unwrap(),
    );
    assert_eq!(status, 400);
    let error: Value = rmp_serde::from_slice(&body).unwrap();
    assert_eq!(error["kind"], "BadRequest");

    // Requests without the header keep the plain MessagePack wire format.
    let plain = client.get(format!("{base}/api/rojo")).send().unwrap();
    assert!(plain.headers().get("x-rojo-content-transfer").is_none());
    let info: Value = rmp_serde::from_slice(&plain.bytes().unwrap()).unwrap();
    assert_eq!(info["projectName"], "EncodingTest");
}
