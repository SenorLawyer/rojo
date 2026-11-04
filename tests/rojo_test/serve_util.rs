use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::{self, JoinHandle},
    time::Duration,
};

use hyper_tungstenite::tungstenite::{connect, Message};
use rbx_dom_weak::types::Ref;

use serde::{Deserialize, Serialize};
use tempfile::{tempdir, TempDir};

use librojo::{
    web_api::{
        ReadResponse, SerializeRequest, SerializeResponse, ServerInfoResponse, SocketPacket,
        SocketPacketType,
    },
    SessionId,
};
use rojo_insta_ext::RedactionMap;

use crate::rojo_test::io_util::{
    copy_recursive, get_working_dir_path, KillOnDrop, ROJO_PATH, SERVE_TESTS_PATH,
};

/// Convenience method to run a `rojo serve` test.
///
/// Test projects should be defined in the `serve-tests` folder; their filename
/// should be given as the first parameter.
///
/// The passed in callback is where the actual test body should go. Setup and
/// cleanup happens automatically.
pub fn run_serve_test(test_name: &str, callback: impl FnOnce(TestServeSession, RedactionMap)) {
    let _ = env_logger::try_init();

    let mut redactions = RedactionMap::default();

    let mut session = TestServeSession::new(test_name);
    let info = session.wait_to_come_online();

    redactions.intern(info.session_id);
    redactions.intern(info.root_instance_id);

    let mut settings = insta::Settings::new();

    let snapshot_path = Path::new(SERVE_TESTS_PATH)
        .parent()
        .unwrap()
        .join("serve-test-snapshots");

    settings.set_snapshot_path(snapshot_path);
    settings.set_sort_maps(true);
    settings.add_redaction(".serverVersion", "[server-version]");
    settings.bind(move || callback(session, redactions));
}

/// Represents a running Rojo serve session running in a temporary directory.
pub struct TestServeSession {
    // Drop order is important here: we want the process to be killed before the
    // directory it's operating on is destroyed.
    rojo_process: KillOnDrop,
    /// Collects what Rojo writes to stderr, so a startup failure can tell a
    /// port collision apart from a crash.
    rojo_stderr: Option<JoinHandle<String>>,
    _dir: TempDir,

    port: usize,
    project_path: PathBuf,
}

impl TestServeSession {
    pub fn new(name: &str) -> Self {
        let working_dir = get_working_dir_path();

        let source_path = Path::new(SERVE_TESTS_PATH).join(name);
        let dir = tempdir().expect("Couldn't create temporary directory");
        let project_path = dir
            .path()
            .canonicalize()
            .expect("Couldn't canonicalize temporary directory path")
            .join(name);

        let source_is_file = fs::metadata(&source_path).unwrap().is_file();

        if source_is_file {
            fs::copy(&source_path, &project_path).expect("couldn't copy project file");
        } else {
            fs::create_dir(&project_path).expect("Couldn't create temporary project subdirectory");

            copy_recursive(&source_path, &project_path)
                .expect("Couldn't copy project to temporary directory");
        };

        // This is an ugly workaround for FSEvents sometimes reporting events
        // for the above copy operations, similar to this Stack Overflow question:
        // https://stackoverflow.com/questions/47679298/howto-avoid-receiving-old-events-in-fseventstream-callback-fsevents-framework-o
        // We'll hope that 100ms is enough for FSEvents to get whatever it is
        // out of its system.
        // TODO: find a better way to avoid processing these spurious events.
        #[cfg(target_os = "macos")]
        std::thread::sleep(Duration::from_millis(100));

        let port = get_port_number();
        let (rojo_process, rojo_stderr) = spawn_rojo_serve(&working_dir, &project_path, port);

        TestServeSession {
            rojo_process,
            rojo_stderr: Some(rojo_stderr),
            _dir: dir,
            port,
            project_path,
        }
    }

    pub fn path(&self) -> &Path {
        &self.project_path
    }

    pub fn port(&self) -> usize {
        self.port
    }

    /// Waits for the `rojo serve` server to come online with expontential
    /// backoff.
    pub fn wait_to_come_online(&mut self) -> ServerInfoResponse {
        const BASE_DURATION_MS: f32 = 30.0;
        const EXP_BACKOFF_FACTOR: f32 = 1.3;
        const MAX_TRIES: u32 = 5;
        const MAX_RESTARTS: u32 = 3;

        let mut restarts = 0;
        let mut i = 0;
        while i < MAX_TRIES {
            i += 1;
            match self.rojo_process.0.try_wait() {
                Ok(Some(status)) => {
                    // The process has exited, so its stderr is complete.
                    let stderr = self
                        .rojo_stderr
                        .take()
                        .and_then(|reader| reader.join().ok())
                        .unwrap_or_default();
                    // Test ports come from a range that the OS also hands out
                    // to outgoing connections, so something else can briefly
                    // hold one. Rojo exits when it cannot bind, so try another.
                    // Any other exit is a real failure, such as a panic, and
                    // restarting would hide it.
                    if restarts >= MAX_RESTARTS || !is_bind_failure(&stderr) {
                        panic!(
                            "Rojo process exited with status {} during startup:\n{}",
                            status, stderr
                        );
                    }
                    log::warn!("Rojo could not bind port {}, restarting", self.port);
                    restarts += 1;
                    i = 0;
                    self.port = get_port_number();
                    let (rojo_process, rojo_stderr) =
                        spawn_rojo_serve(&get_working_dir_path(), &self.project_path, self.port);
                    self.rojo_process = rojo_process;
                    self.rojo_stderr = Some(rojo_stderr);
                    continue;
                }
                Ok(None) => { /* The process is still running, as expected */ }
                Err(err) => panic!("Failed to wait on Rojo process: {}", err),
            }

            let info = match self.get_api_rojo() {
                Ok(info) => info,
                Err(err) => {
                    let retry_time_ms = BASE_DURATION_MS * (i as f32).powf(EXP_BACKOFF_FACTOR);
                    let retry_time = Duration::from_millis(retry_time_ms as u64);

                    log::info!("Server error, retrying in {:?}: {}", retry_time, err);
                    thread::sleep(retry_time);
                    continue;
                }
            };

            log::info!("Got session info: {:?}", info);

            return info;
        }

        panic!("Rojo server did not respond after {} tries.", MAX_TRIES);
    }

    pub fn get_api_rojo(&self) -> Result<ServerInfoResponse, reqwest::Error> {
        let url = format!("http://localhost:{}/api/rojo", self.port);
        let body = reqwest::blocking::get(url)?.bytes()?;

        Ok(deserialize_msgpack(&body).expect("Server returned malformed response"))
    }

    pub fn get_api_read(&self, id: Ref) -> Result<ReadResponse<'_>, reqwest::Error> {
        let url = format!("http://localhost:{}/api/read/{}", self.port, id);
        let body = reqwest::blocking::get(url)?.bytes()?;

        Ok(deserialize_msgpack(&body).expect("Server returned malformed response"))
    }

    pub fn get_api_socket_packet(
        &self,
        packet_type: SocketPacketType,
        cursor: u32,
    ) -> Result<SocketPacket<'static>, Box<dyn std::error::Error>> {
        let url = format!("ws://localhost:{}/api/socket/{}", self.port, cursor);

        let (mut socket, _response) = connect(url)?;

        // Wait for messages with a timeout
        let timeout = Duration::from_secs(10);
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err("Timeout waiting for packet from WebSocket".into());
            }

            match socket.read() {
                Ok(Message::Binary(binary)) => {
                    let packet: SocketPacket = deserialize_msgpack(&binary)?;
                    if packet.packet_type != packet_type {
                        continue;
                    }

                    // Close the WebSocket connection now that we got what we were waiting for
                    let _ = socket.close(None);
                    return Ok(packet);
                }
                Ok(Message::Close(_)) => {
                    return Err("WebSocket closed before receiving messages".into());
                }
                Ok(_) => {
                    // Ignore other message types (ping, pong, text)
                    continue;
                }
                Err(hyper_tungstenite::tungstenite::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    // No data available yet, sleep a bit and try again
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
    }

    pub fn post_api_serialize(
        &self,
        ids: &[Ref],
        session_id: SessionId,
    ) -> Result<reqwest::blocking::Response, reqwest::Error> {
        let client = reqwest::blocking::Client::new();
        let url = format!("http://localhost:{}/api/serialize", self.port);
        let body = serialize_msgpack(&SerializeRequest {
            session_id,
            ids: ids.to_vec(),
        })
        .unwrap();

        client.post(url).body(body).send()
    }

    /// Sends a GET to `/api/rojo` with the given extra request headers and
    /// returns the full response. Used to exercise the Host/Origin allowlist that
    /// guards against DNS rebinding, including asserting that a rejection reveals
    /// nothing about the server.
    pub fn api_rojo_response_with_headers(
        &self,
        headers: &[(&str, &str)],
    ) -> reqwest::blocking::Response {
        let client = reqwest::blocking::Client::new();
        let url = format!("http://localhost:{}/api/rojo", self.port);

        let mut request = client.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }

        request.send().expect("Failed to send request")
    }

    /// Sends a POST to `/api/open/<id>` and returns the response status code.
    /// Used to verify that the local-only gate on `/api/open` admits loopback
    /// peers (the test harness always connects over loopback).
    pub fn api_open_status(&self, id: &str) -> reqwest::StatusCode {
        let client = reqwest::blocking::Client::new();
        let url = format!("http://localhost:{}/api/open/{}", self.port, id);

        client
            .post(url)
            .send()
            .expect("Failed to send request")
            .status()
    }
}

fn serialize_msgpack<T: Serialize>(value: T) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let mut serialized = Vec::new();
    let mut serializer = rmp_serde::Serializer::new(&mut serialized)
        .with_human_readable()
        .with_struct_map();

    value.serialize(&mut serializer)?;

    Ok(serialized)
}

pub fn deserialize_msgpack<'a, T: Deserialize<'a>>(
    input: &'a [u8],
) -> Result<T, rmp_serde::decode::Error> {
    let mut deserializer = rmp_serde::Deserializer::new(input).with_human_readable();

    T::deserialize(&mut deserializer)
}

/// Starts `rojo serve`, and returns it along with a thread that passes its
/// stderr through and returns what it read once Rojo exits.
fn spawn_rojo_serve(
    working_dir: &Path,
    project_path: &Path,
    port: usize,
) -> (KillOnDrop, JoinHandle<String>) {
    // Only the start of the output is needed to recognize a startup failure.
    const MAX_CAPTURED_BYTES: usize = 64 * 1024;

    let port_string = port.to_string();
    let mut rojo_process = Command::new(ROJO_PATH)
        .args([
            "serve",
            project_path.to_str().unwrap(),
            "--port",
            port_string.as_str(),
        ])
        .current_dir(working_dir)
        .stderr(Stdio::piped())
        .spawn()
        .expect("Couldn't start Rojo");
    let stderr = rojo_process.stderr.take().unwrap();
    // The pipe is drained for the life of the process, since Rojo would block
    // once it filled up.
    let reader = thread::spawn(move || {
        let mut captured = String::new();
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        while matches!(reader.read_until(b'\n', &mut line), Ok(read) if read > 0) {
            let text = String::from_utf8_lossy(&line);
            eprint!("{}", text);
            if captured.len() < MAX_CAPTURED_BYTES {
                captured.push_str(&text);
            }
            line.clear();
        }
        captured
    });
    (KillOnDrop(rojo_process), reader)
}

/// Whether Rojo's stderr says that it exited because it could not bind its
/// port.
fn is_bind_failure(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    [
        "could not start the rojo server",
        "address already in use",
        "addrinuse",
        // WSAEADDRINUSE, and WSAEACCES for ports Windows reserved.
        "os error 10048",
        "os error 10013",
    ]
    .iter()
    .any(|marker| stderr.contains(marker))
}

/// Probably-okay way to generate random enough port numbers for running the
/// Rojo live server.
///
/// If this method ends up having problems, we should add an option for Rojo to
/// use a random port chosen by the operating system and figure out a good way
/// to get that port back to the test CLI.
fn get_port_number() -> usize {
    static NEXT_PORT_NUMBER: AtomicUsize = AtomicUsize::new(35103);

    NEXT_PORT_NUMBER.fetch_add(1, Ordering::SeqCst)
}

/// Takes a SerializeResponse and creates an XML model out of the response.
///
/// Since the provided structure intentionally includes unredacted referents,
/// some post-processing is done to ensure they don't show up in the model.
pub fn serialize_to_xml_model(response: &SerializeResponse, redactions: &RedactionMap) -> String {
    let mut dom = rbx_binary::from_reader(response.model_contents.as_slice()).unwrap();
    // This makes me realize that maybe we need a `descendants_mut` iter.
    let ref_list: Vec<Ref> = dom.descendants().map(|inst| inst.referent()).collect();
    for referent in ref_list {
        let inst = dom.get_by_ref_mut(referent).unwrap();
        if let Some(id) = redactions.get_id_for_value(&inst.name) {
            inst.name = format!("id-{id}");
        }
    }

    let mut data = Vec::new();
    rbx_xml::to_writer_default(&mut data, &dom, dom.root().children()).unwrap();
    String::from_utf8(data).expect("rbx_xml should never produce invalid utf-8")
}

#[test]
fn only_bind_failures_restart_rojo() {
    assert!(is_bind_failure(
        "Error: Could not start the Rojo server on 127.0.0.1:35103.\n\
         Caused by: error creating server listener: Only one usage of each socket address \
         (protocol/network address/port) is normally permitted. (os error 10048)"
    ));
    assert!(is_bind_failure(
        "error creating server listener: Address already in use (os error 98)"
    ));
    assert!(!is_bind_failure(
        "thread 'main' panicked at src/serve_session.rs:99:1:\ncalled `Option::unwrap()` on a `None` value"
    ));
    assert!(!is_bind_failure(""));
}
