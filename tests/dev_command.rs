use std::{
    fs,
    io::Read,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

struct Dev(Child);

impl Drop for Dev {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Points Rojo's per-user state folder at `state`, so the test never touches
/// the real one.
fn isolate<'a>(command: &'a mut Command, state: &Path) -> &'a mut Command {
    command
        .env("LOCALAPPDATA", state)
        .env("XDG_STATE_HOME", state)
        .env("HOME", state)
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|file| file == name) {
            return Some(path);
        }
    }
    None
}

fn wait_for(what: &str, dev: &mut Dev, output: &Mutex<String>, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(
            dev.0.try_wait().unwrap().is_none(),
            "rojo dev exited while waiting for {what}:\n{}",
            output.lock().unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for {what}:\n{}",
            output.lock().unwrap()
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn dev_serves_keeps_the_sourcemap_current_and_is_found_by_rojo_studio() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let state = dir.path().join("state");
    let plugins = dir.path().join("plugins");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        project.join("default.project.json"),
        r#"{"name":"DevTest","servePlaceIds":[0],"tree":{"$path":"src"}}"#,
    )
    .unwrap();
    fs::write(project.join("src/main.luau"), "return 1").unwrap();

    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    let mut command = Command::new(env!("CARGO_BIN_EXE_rojo"));
    command
        .arg("dev")
        .arg(&project)
        .args(["--port", &port.to_string(), "--plugins-dir"])
        .arg(&plugins)
        .current_dir(&project)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate(&mut command, &state);
    let mut dev = Dev(command.spawn().unwrap());

    let output = Arc::new(Mutex::new(String::new()));
    for stream in [
        Box::new(dev.0.stdout.take().unwrap()) as Box<dyn Read + Send>,
        Box::new(dev.0.stderr.take().unwrap()),
    ] {
        let output = Arc::clone(&output);
        thread::spawn(move || {
            let mut stream = stream;
            let mut buffer = [0; 4096];
            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                output
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buffer[..read]));
            }
        });
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}/api/rojo");
    wait_for("the server", &mut dev, &output, || {
        client
            .get(&url)
            .send()
            .is_ok_and(|response| response.status().is_success())
    });

    let sourcemap_path = project.join("sourcemap.json");
    let sourcemap = || fs::read_to_string(&sourcemap_path).unwrap_or_default();
    assert!(sourcemap().contains("\"main\""), "{}", sourcemap());
    wait_for("the status block", &mut dev, &output, || {
        output.lock().unwrap().contains("Press Ctrl+C to stop.")
    });
    {
        let text = output.lock().unwrap();
        assert!(
            text.contains(&format!("port {port}, from --port")),
            "{text}"
        );
        assert!(text.contains("servePlaceIds: 0"), "{text}");
        assert!(text.contains("Studio plugin: not installed"), "{text}");
        assert!(text.contains("rojo studio --address"), "{text}");
    }

    // A new script changes the sourcemap, which rojo dev rewrites.
    fs::write(project.join("src/added.luau"), "return 2").unwrap();
    wait_for(
        "the sourcemap to list the new script",
        &mut dev,
        &output,
        || sourcemap().contains("\"added\""),
    );
    assert!(
        fs::read_dir(&project).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")),
        "a temporary sourcemap was left behind"
    );

    // The token and state file are outside the project, and `rojo studio`
    // finds the server through them without flags.
    let token = find_file(&state, "studio-token").expect("no token was created");
    let dev_state = find_file(&state, "dev.json").expect("no dev.json was written");
    assert!(!token.starts_with(&project) && !dev_state.starts_with(&project));
    let mut studio = Command::new(env!("CARGO_BIN_EXE_rojo"));
    studio
        .args(["studio", "--json", "sessions"])
        .current_dir(project.join("src"))
        .stdin(Stdio::null());
    isolate(&mut studio, &state);
    let sessions = studio.output().unwrap();
    assert!(
        sessions.status.success(),
        "rojo studio sessions failed: {}",
        String::from_utf8_lossy(&sessions.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&sessions.stderr).contains("Ignoring"),
        "the live record was rejected: {}",
        String::from_utf8_lossy(&sessions.stderr)
    );
    let sessions: serde_json::Value = serde_json::from_slice(&sessions.stdout).unwrap();
    assert!(sessions["sessions"].is_array(), "{sessions}");
    assert!(
        sessions["identity"]["workspacePath"]
            .as_str()
            .is_some_and(|path| path.ends_with("project")),
        "{sessions}"
    );

    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&dev_state).unwrap()).unwrap();

    stop(&mut dev, &output, &dev_state);
    assert!(
        client.get(&url).send().is_err(),
        "the server still answers after rojo dev stopped"
    );

    // A record left by a rojo dev that died, whose port another server with
    // the shared token has since taken (here a plain rojo serve of another
    // project), must not send flagless commands to that server. The pid is
    // set to a live process so the check has to ask the server.
    let mut stale = record;
    stale["pid"] = serde_json::json!(std::process::id());
    fs::write(&dev_state, stale.to_string()).unwrap();
    let other = dir.path().join("other");
    fs::create_dir_all(other.join("src")).unwrap();
    fs::write(
        other.join("default.project.json"),
        r#"{"name":"Impostor","tree":{"$path":"src"}}"#,
    )
    .unwrap();
    let mut impostor = Dev(Command::new(env!("CARGO_BIN_EXE_rojo"))
        .arg("serve")
        .arg(&other)
        .args(["--port", &port.to_string(), "--studio-token-file"])
        .arg(&token)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap());
    wait_for("the other server", &mut impostor, &output, || {
        client
            .get(&url)
            .send()
            .is_ok_and(|response| response.status().is_success())
    });
    let mut studio = Command::new(env!("CARGO_BIN_EXE_rojo"));
    studio
        .args(["studio", "--json", "sessions"])
        .current_dir(&project)
        .stdin(Stdio::null());
    isolate(&mut studio, &state);
    let result = studio.output().unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    // It fails closed: no fallback to the default port, which may be another
    // worktree's server, and nothing from the other project's server.
    assert_eq!(result.status.code(), Some(2), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("could not be confirmed")
            && stderr.contains("not the one rojo dev recorded")
            && stderr.contains("--address and --token-file"),
        "{stderr}"
    );
    assert!(stdout.trim().is_empty(), "{stdout}");
}

/// Unix gets the Ctrl+C path: SIGINT must stop the server, remove the state
/// file and exit with 0. Windows cannot send a console Ctrl+C to one child
/// process without new dependencies, so there the process is killed.
#[cfg(unix)]
fn stop(dev: &mut Dev, output: &Mutex<String>, dev_state: &Path) {
    let status = Command::new("kill")
        .args(["-INT", &dev.0.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    let exit = loop {
        if let Some(exit) = dev.0.try_wait().unwrap() {
            break exit;
        }
        assert!(Instant::now() < deadline, "rojo dev did not stop on SIGINT");
        thread::sleep(Duration::from_millis(50));
    };
    // The reader threads may still be draining the pipes.
    thread::sleep(Duration::from_millis(200));
    let text = output.lock().unwrap();
    assert!(exit.success(), "{exit:?}\n{text}");
    assert!(text.contains("rojo dev stopped."), "{text}");
    assert!(!dev_state.exists(), "dev.json was left behind");
}

#[cfg(not(unix))]
fn stop(dev: &mut Dev, _output: &Mutex<String>, _dev_state: &Path) {
    dev.0.kill().unwrap();
    dev.0.wait().unwrap();
}

/// Without its record, commands run here without flags would go to the
/// default port with the shared token, so rojo dev does not start.
#[test]
fn dev_does_not_start_when_it_cannot_record_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let state = dir.path().join("state");
    let plugins = dir.path().join("plugins");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        project.join("default.project.json"),
        r#"{"name":"DevTest","servePlaceIds":[0],"tree":{"$path":"src"}}"#,
    )
    .unwrap();
    fs::write(project.join("src/main.luau"), "return 1").unwrap();

    // The project is not in Git, so its record goes in its own state folder,
    // where a folder named dev.json stops it from being written.
    let canonical = dunce::canonicalize(&project).unwrap();
    let id = blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex();
    let base = if cfg!(windows) {
        state.join("Rojo")
    } else {
        state.join("rojo")
    };
    fs::create_dir_all(base.join("projects").join(&id[..16]).join("dev.json")).unwrap();

    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_rojo"));
    command
        .arg("dev")
        .arg(&project)
        .args(["--port", &port.to_string(), "--plugins-dir"])
        .arg(&plugins)
        .current_dir(&project)
        .stdin(Stdio::null());
    isolate(&mut command, &state);
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("could not record this server"), "{stderr}");
    assert!(!stdout.contains("rojo dev is serving"), "{stdout}");
}
