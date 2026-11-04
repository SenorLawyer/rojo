//! `rojo test --cloud`: runs the specs headlessly with Open Cloud Luau
//! Execution instead of Studio.
//!
//! The project is built into a place in memory and saved, never published,
//! as a new version of the given place. The standalone spec runner then runs
//! against exactly that version, and its report is read the same way as the
//! Studio bridge's. With `--no-upload` nothing is saved and the runner runs
//! against the version Open Cloud picks when none is named, which Roblox
//! documents as the place's current published version. That tests what was
//! uploaded before, not the local files.

use std::{path::Path, time::Duration};

use anyhow::Context;
use memofs::Vfs;
use serde::Serialize;

use super::{standalone_runner_source, SpecOptions, Status, TestReport};
use crate::{
    open_cloud::{LogMessage, OpenCloud, OpenCloudError, PlaceFormat, TaskState, VersionType},
    serve_session::ServeSession,
};

/// Luau Execution stops a task after at most five minutes.
const MAX_TASK_SECONDS: u64 = 300;

/// Time the task gets beyond the specs' own timeout, for loading the runner,
/// finding the specs and requiring the framework. The runner's own timeout
/// fires first, so a slow spec still produces a report listing what failed.
const TASK_MARGIN_SECONDS: u64 = 30;

/// How long the Luau task may run for specs allowed `spec_seconds`.
pub(super) fn task_timeout(spec_seconds: u64) -> Duration {
    Duration::from_secs((spec_seconds + TASK_MARGIN_SECONDS).min(MAX_TASK_SECONDS))
}

/// Builds the project at `project` into a binary place file in memory and
/// returns the project's name with the bytes.
pub(super) fn build_place(project: &Path) -> anyhow::Result<(String, Vec<u8>)> {
    let session = ServeSession::new(Vfs::new_default_without_watch(), project)?;
    let name = session.project_name().to_owned();
    let bytes = {
        let tree = session.tree();
        let root = tree.get_instance(tree.get_root_id()).unwrap();
        anyhow::ensure!(
            root.class_name() == "DataModel",
            "rojo test --cloud needs a place project, but the root of '{name}' is a {}. \
             Set the project's root $className to DataModel.",
            root.class_name()
        );
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, tree.inner(), root.children())
            .context("Could not serialize the place")?;
        bytes
    };
    // Dropping a ServeSession can be slow and the process exits soon after,
    // as for `rojo build`.
    std::mem::forget(session);
    Ok((name, bytes))
}

/// Where and what to run.
pub(super) struct CloudRequest<'a> {
    pub universe_id: u64,
    pub place_id: u64,
    /// The place to save as a new version first, or `None` to run against
    /// the version Open Cloud picks (the current published one).
    pub place: Option<&'a [u8]>,
    pub options: &'a SpecOptions,
    pub task_timeout: Duration,
}

/// A finished cloud run: where it ran, its logs and its report.
#[derive(Debug)]
pub(super) struct CloudRun {
    pub universe_id: u64,
    pub place_id: u64,
    /// The version the task ran against, when Roblox named it.
    pub place_version: Option<u64>,
    pub uploaded: bool,
    pub task_path: Option<String>,
    pub logs: Vec<LogMessage>,
    pub logs_truncated: bool,
    pub report: TestReport,
}

/// Saves the place (when given), runs the specs and reads the report.
/// `progress` receives one line per step, for stderr.
///
/// Errors mean the run could not be started or followed: the place could not
/// be saved, or Open Cloud refused or failed a request. A task that ran and
/// failed, was cancelled or did not finish in time is a report instead.
pub(super) fn run(
    client: &OpenCloud,
    request: &CloudRequest,
    progress: &mut dyn FnMut(String),
) -> anyhow::Result<CloudRun> {
    let CloudRequest {
        universe_id,
        place_id,
        ..
    } = *request;
    let version = match request.place {
        Some(bytes) => {
            progress(format!(
                "Saving the built place ({} bytes) as a new version of place {place_id} in universe {universe_id}. \
                 It is saved, not published: players keep joining the current published version. \
                 Studio opens the latest saved version, though, so whoever opens this place in Studio next gets this build, \
                 and publishing from Studio would publish it.",
                bytes.len()
            ));
            let version = client
                .publish_place(
                    universe_id,
                    place_id,
                    bytes,
                    PlaceFormat::Binary,
                    VersionType::Saved,
                )
                .map_err(save_error)?;
            progress(format!("Saved place version {version}."));
            Some(version)
        }
        None => None,
    };
    match version {
        Some(version) => progress(format!(
            "Running the specs on place {place_id} version {version} with Open Cloud Luau Execution."
        )),
        None => progress(format!(
            "Running the specs on the current published version of place {place_id} with Open Cloud Luau Execution. \
             --no-upload tests what was uploaded to the place before, not the local files."
        )),
    }

    let script = standalone_runner_source(request.options);
    let mut run = CloudRun {
        universe_id,
        place_id,
        place_version: version,
        uploaded: version.is_some(),
        task_path: None,
        logs: Vec::new(),
        logs_truncated: false,
        report: TestReport::error("unreachable"),
    };
    let task = match client.run_luau(
        universe_id,
        place_id,
        version,
        &script,
        request.task_timeout,
    ) {
        Ok(task) => task,
        Err(OpenCloudError::TimedOut(message)) => {
            run.report = TestReport {
                status: Status::TimedOut,
                ..TestReport::error(format!(
                    "{message}. The Luau task may still be running; Roblox stops it after its {}-second timeout.",
                    request.task_timeout.as_secs()
                ))
            };
            return Ok(run);
        }
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context("Could not run the specs with Open Cloud Luau Execution"))
        }
    };
    if let Some(ran) = version_in_path(&task.path) {
        if version.is_some_and(|version| version != ran) {
            log::warn!("Asked to run version {version:?}, but the task ran on version {ran}");
        }
        run.place_version = Some(ran);
    }
    let ran_on = run.place_version.map_or(String::new(), |version| {
        format!(" on place version {version}")
    });
    progress(format!(
        "Luau task {}{ran_on} finished as {:?}.",
        task.path, task.state
    ));
    if let Some(error) = &task.logs_error {
        progress(format!(
            "Could not fetch all of the task's logs, so some are missing: {error}"
        ));
    }
    run.report = task_report(
        &task.state,
        &task.results,
        &task.error_code,
        &task.error_message,
    );
    run.task_path = Some(task.path);
    run.logs = task.logs;
    run.logs_truncated = task.logs_truncated;
    Ok(run)
}

/// The report for a finished task.
fn task_report(
    state: &TaskState,
    results: &[serde_json::Value],
    error_code: &Option<String>,
    error_message: &Option<String>,
) -> TestReport {
    let message = error_message.as_deref().unwrap_or("no message");
    match state {
        TaskState::Complete => match results.first() {
            Some(value) => TestReport::from_runner(value)
                .unwrap_or_else(|error| TestReport::error(format!("{error:#}"))),
            None => TestReport::error("The Luau task finished without returning a report"),
        },
        TaskState::Failed if error_code.as_deref() == Some("DEADLINE_EXCEEDED") => TestReport {
            status: Status::TimedOut,
            ..TestReport::error(format!(
                "The Luau task reached its time limit before the specs finished: {message}"
            ))
        },
        TaskState::Failed => TestReport::error(format!(
            "The Luau task failed ({}): {message}",
            error_code.as_deref().unwrap_or("no error code")
        )),
        TaskState::Cancelled => {
            TestReport::error(format!("The Luau task was cancelled: {message}"))
        }
    }
}

/// Explains a failed save. A 409 is what Roblox answers while the place is
/// open in a Team Create session.
fn save_error(error: OpenCloudError) -> anyhow::Error {
    let conflict =
        matches!(&error, OpenCloudError::Status { status, .. } if status.as_u16() == 409);
    let applied = error.may_have_applied();
    let mut context = String::from("Could not save the place as a new version");
    if conflict {
        context.push_str(
            ". Roblox refuses to save a place while it is open in Team Create; close it in Studio, or pass --no-upload to test the version already uploaded",
        );
    }
    if applied {
        context.push_str(". A version may have been saved anyway; check the place's version history before retrying");
    }
    anyhow::Error::new(error).context(context)
}

/// The version number in a task path such as
/// `universes/1/places/2/versions/7/luau-execution-sessions/s/tasks/t`.
fn version_in_path(path: &str) -> Option<u64> {
    let mut segments = path.split('/');
    segments.find(|segment| *segment == "versions")?;
    segments.next()?.parse().ok()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LogOutput<'a> {
    pub message: &'a str,
    pub kind: &'a str,
}

/// The `--json` output of a cloud run: the report with where it ran and the
/// task's logs.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CloudOutput<'a> {
    pub universe_id: u64,
    pub place_id: u64,
    pub place_version: Option<u64>,
    pub uploaded: bool,
    pub task_path: Option<&'a str>,
    pub logs: Vec<LogOutput<'a>>,
    pub logs_truncated: bool,
    #[serde(flatten)]
    pub report: &'a TestReport,
}

impl CloudRun {
    pub(super) fn output(&self) -> CloudOutput<'_> {
        CloudOutput {
            universe_id: self.universe_id,
            place_id: self.place_id,
            place_version: self.place_version,
            uploaded: self.uploaded,
            task_path: self.task_path.as_deref(),
            logs: self
                .logs
                .iter()
                .map(|log| LogOutput {
                    message: &log.message,
                    kind: &log.kind,
                })
                .collect(),
            logs_truncated: self.logs_truncated,
            report: &self.report,
        }
    }

    /// The human-readable output: errors the task logged, then the report.
    pub(super) fn render(&self) -> String {
        let mut output = String::new();
        let errors: Vec<_> = self
            .logs
            .iter()
            .filter(|log| log.kind.eq_ignore_ascii_case("ERROR"))
            .collect();
        if !errors.is_empty() {
            output.push_str("Errors logged by the Luau task:\n");
            for log in errors {
                for line in log.message.lines() {
                    output.push_str("    ");
                    output.push_str(line);
                    output.push('\n');
                }
            }
            output.push('\n');
        }
        output.push_str(&self.report.render());
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test::Framework;
    use serde_json::{json, Value};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
    };

    const KEY: &str = "fixture-key-cloud-0123456789";
    const TASK_PATH: &str = "universes/1/places/2/versions/7/luau-execution-sessions/s1/tasks/t1";

    #[derive(Debug, Clone)]
    struct Recorded {
        method: String,
        target: String,
        headers: String,
        body: Vec<u8>,
    }

    /// Serves the JSON replies in order on loopback, one connection each,
    /// and records the requests.
    fn serve(replies: Vec<(&'static str, String)>) -> (String, Arc<Mutex<Vec<Recorded>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&recorded);
        std::thread::spawn(move || {
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut data = Vec::new();
                let mut chunk = [0; 8192];
                let head_end = loop {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0, "client closed the connection early");
                    data.extend_from_slice(&chunk[..n]);
                    if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while data.len() < head_end + length {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0, "client closed the connection early");
                    data.extend_from_slice(&chunk[..n]);
                }
                let mut request_line = head.lines().next().unwrap().split(' ');
                log.lock().unwrap().push(Recorded {
                    method: request_line.next().unwrap().to_owned(),
                    target: request_line.next().unwrap().to_owned(),
                    headers: head.to_ascii_lowercase(),
                    body: data[head_end..head_end + length].to_vec(),
                });
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (format!("http://{address}"), recorded)
    }

    fn ok(body: Value) -> (&'static str, String) {
        ("200 OK", body.to_string())
    }

    fn client(base: &str) -> OpenCloud {
        OpenCloud::with_base_url(KEY, base)
            .unwrap()
            .with_fast_timing()
    }

    fn options() -> SpecOptions {
        SpecOptions {
            roots: vec!["game/ReplicatedStorage/Shared".into()],
            framework: Framework::Testez,
            module: None,
            filter: Some("Math".into()),
            timeout_seconds: 60,
        }
    }

    fn report_value(status: &str, failed: u64) -> Value {
        json!({
            "framework": "testez", "status": status, "passed": 2, "failed": failed,
            "skipped": 0, "modules": 1,
            "failures": if failed > 0 {
                json!([{"path": ["Math", "adds"], "messages": ["expected 3"]}])
            } else {
                json!({})
            },
            "loadErrors": {}, "durationSeconds": 0.5,
        })
    }

    fn logs(messages: Value) -> (&'static str, String) {
        ok(json!({"luauExecutionSessionTaskLogs": [{"structuredMessages": messages}]}))
    }

    fn execute(
        base: &str,
        place: Option<&[u8]>,
        task_timeout: Duration,
    ) -> (anyhow::Result<CloudRun>, Vec<String>) {
        let options = options();
        let mut lines = Vec::new();
        let result = run(
            &client(base),
            &CloudRequest {
                universe_id: 1,
                place_id: 2,
                place,
                options: &options,
                task_timeout,
            },
            &mut |line| lines.push(line),
        );
        (result, lines)
    }

    #[test]
    fn uploads_a_saved_version_and_runs_against_it() {
        let (base, recorded) = serve(vec![
            ok(json!({"versionNumber": 7})),
            ok(json!({"path": TASK_PATH, "state": "QUEUED"})),
            ok(json!({"path": TASK_PATH, "state": "PROCESSING"})),
            ok(json!({"path": TASK_PATH, "state": "COMPLETE",
                "output": {"results": [report_value("failed", 1)]}})),
            logs(json!([
                {"message": "loading specs", "messageType": "OUTPUT"},
                {"message": "Workspace.Thing:3: oops", "messageType": "ERROR"},
            ])),
        ]);
        let (result, lines) = execute(&base, Some(b"<roblox!place"), Duration::from_secs(90));
        let run = result.unwrap();

        let requests = recorded.lock().unwrap().clone();
        assert_eq!(requests.len(), 5);
        // Saved, never Published.
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].target,
            "/universes/v1/1/places/2/versions?versionType=Saved"
        );
        assert_eq!(requests[0].body, b"<roblox!place");
        assert!(requests[0].headers.contains("x-api-key: fixture-key-cloud"));
        // The task runs on exactly the version that was saved.
        assert_eq!(
            requests[1].target,
            "/cloud/v2/universes/1/places/2/versions/7/luau-execution-session-tasks"
        );
        let task: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(task["timeout"], "90s");
        let script = task["script"].as_str().unwrap();
        assert!(script.contains("function SpecRunner.runWithTimeout"));
        assert!(script.contains(&options().to_args().to_string()));
        assert_eq!(requests[2].target, format!("/cloud/v2/{TASK_PATH}"));
        assert!(requests[4]
            .target
            .starts_with(&format!("/cloud/v2/{TASK_PATH}/logs?")));

        assert_eq!(run.place_version, Some(7));
        assert!(run.uploaded);
        assert_eq!(run.task_path.as_deref(), Some(TASK_PATH));
        assert_eq!(run.report.status, Status::Failed);
        assert_eq!(run.report.exit_code(), 1);
        assert!(lines.iter().any(|line| line.contains("not published")));
        // Saved versions are what Studio opens, which the warning says.
        assert!(lines
            .iter()
            .any(|line| line.contains("publishing from Studio would publish it")));
        assert!(lines.iter().any(|line| line == "Saved place version 7."));
        assert!(lines.iter().any(|line| line.contains("place 2 version 7")));
        assert!(lines.iter().all(|line| !line.contains(KEY)));

        let rendered = run.render();
        assert!(
            rendered.starts_with("Errors logged by the Luau task:\n    Workspace.Thing:3: oops\n\nFAIL Math > adds\n"),
            "{rendered}"
        );
        assert!(!rendered.contains("loading specs"));

        let output = serde_json::to_value(run.output()).unwrap();
        assert_eq!(output["placeVersion"], 7);
        assert_eq!(output["uploaded"], true);
        assert_eq!(output["status"], "failed");
        assert_eq!(
            output["logs"][0],
            json!({"message": "loading specs", "kind": "OUTPUT"})
        );
        assert_eq!(output["logs"][1]["kind"], "ERROR");
        assert_eq!(output["logsTruncated"], false);
    }

    #[test]
    fn a_passing_report_exits_with_0() {
        let (base, _) = serve(vec![
            ok(json!({"versionNumber": 3})),
            ok(json!({"path": TASK_PATH, "state": "COMPLETE",
                "output": {"results": [report_value("passed", 0)]}})),
            logs(json!([])),
        ]);
        let run = execute(&base, Some(b"place"), Duration::from_secs(60))
            .0
            .unwrap();
        assert_eq!(run.report.status, Status::Passed);
        assert_eq!(run.report.exit_code(), 0);
        assert!(run.render().starts_with("PASSED: 2 passed"));
    }

    #[test]
    fn no_upload_runs_against_the_version_open_cloud_picks() {
        let (base, recorded) = serve(vec![
            ok(json!({"path": TASK_PATH, "state": "COMPLETE",
                "output": {"results": [report_value("passed", 0)]}})),
            logs(json!([])),
        ]);
        let (result, lines) = execute(&base, None, Duration::from_secs(60));
        let run = result.unwrap();
        let requests = recorded.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].target,
            "/cloud/v2/universes/1/places/2/luau-execution-session-tasks"
        );
        assert!(!run.uploaded);
        // The version comes from the task's path.
        assert_eq!(run.place_version, Some(7));
        assert!(lines
            .iter()
            .any(|line| line.contains("not the local files")));
    }

    #[test]
    fn a_failed_task_is_an_error_report_with_its_logs() {
        let (base, _) = serve(vec![
            ok(json!({"path": TASK_PATH, "state": "FAILED",
                "error": {"code": "SCRIPT_ERROR", "message": "Script:1: boom"}})),
            logs(json!([{"message": "Script:1: boom", "messageType": "ERROR"}])),
        ]);
        let run = execute(&base, None, Duration::from_secs(60)).0.unwrap();
        assert_eq!(run.report.status, Status::Error);
        assert_eq!(run.report.exit_code(), 1);
        assert_eq!(
            run.report.error.as_deref(),
            Some("The Luau task failed (SCRIPT_ERROR): Script:1: boom")
        );
        assert!(run
            .render()
            .contains("Errors logged by the Luau task:\n    Script:1: boom\n"));
    }

    #[test]
    fn deadlines_and_cancellations_are_reported() {
        let (base, _) = serve(vec![
            ok(json!({"path": TASK_PATH, "state": "FAILED",
                "error": {"code": "DEADLINE_EXCEEDED", "message": "took too long"}})),
            logs(json!([])),
            ok(json!({"path": TASK_PATH, "state": "CANCELLED"})),
            logs(json!([])),
        ]);
        let run = execute(&base, None, Duration::from_secs(60)).0.unwrap();
        assert_eq!(run.report.status, Status::TimedOut);
        assert_eq!(run.report.exit_code(), 1);
        let run = execute(&base, None, Duration::from_secs(60)).0.unwrap();
        assert_eq!(run.report.status, Status::Error);
        assert!(run.report.error.unwrap().contains("cancelled"));
    }

    #[test]
    fn a_task_that_never_finishes_times_out() {
        let mut replies = vec![ok(json!({"path": TASK_PATH, "state": "PROCESSING"}))];
        replies.extend((0..100).map(|_| ok(json!({"path": TASK_PATH, "state": "PROCESSING"}))));
        let (base, _) = serve(replies);
        let run = execute(&base, None, Duration::from_millis(200)).0.unwrap();
        assert_eq!(run.report.status, Status::TimedOut);
        assert_eq!(run.report.exit_code(), 1);
        assert!(run.report.render().starts_with("Test run timed out: "));
    }

    #[test]
    fn a_complete_task_without_a_report_is_an_error() {
        let (base, _) = serve(vec![
            ok(json!({"path": TASK_PATH, "state": "COMPLETE", "output": {"results": []}})),
            logs(json!([])),
            ok(json!({"path": TASK_PATH, "state": "COMPLETE", "output": {"results": [5]}})),
            logs(json!([])),
        ]);
        let run = execute(&base, None, Duration::from_secs(60)).0.unwrap();
        assert_eq!(run.report.status, Status::Error);
        let run = execute(&base, None, Duration::from_secs(60)).0.unwrap();
        assert_eq!(
            run.report.error.as_deref(),
            Some("The spec runner returned an unrecognized report")
        );
    }

    #[test]
    fn a_refused_save_stops_before_any_task() {
        let (base, recorded) = serve(vec![(
            "409 Conflict",
            "Server is busy and unable to process your upload request".to_owned(),
        )]);
        let error = execute(&base, Some(b"place"), Duration::from_secs(60))
            .0
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Team Create"), "{message}");
        assert!(message.contains("--no-upload"), "{message}");
        assert!(!message.contains(KEY));
        assert_eq!(recorded.lock().unwrap().len(), 1);
    }

    #[test]
    fn task_timeouts_leave_room_for_the_runner_within_the_limit() {
        assert_eq!(task_timeout(60), Duration::from_secs(90));
        assert_eq!(task_timeout(300), Duration::from_secs(300));
        assert_eq!(task_timeout(1), Duration::from_secs(31));
    }

    #[test]
    fn versions_are_read_from_task_paths() {
        assert_eq!(version_in_path(TASK_PATH), Some(7));
        assert_eq!(version_in_path("universes/1/places/2/tasks/t"), None);
    }

    fn project(dir: &Path, class: &str) -> std::path::PathBuf {
        let path = dir.join("default.project.json");
        std::fs::write(
            &path,
            json!({"name": "cloud", "tree": {"$className": class,
            "ReplicatedStorage": if class == "DataModel" {
                json!({"$className": "ReplicatedStorage"})
            } else {
                json!({"$className": "Folder"})
            }}})
            .to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn only_place_projects_are_built() {
        let dir = tempfile::tempdir().unwrap();
        let (name, bytes) = build_place(&project(dir.path(), "DataModel")).unwrap();
        assert_eq!(name, "cloud");
        assert!(bytes.starts_with(b"<roblox!"));
        let error = build_place(&project(dir.path(), "Folder")).unwrap_err();
        assert!(
            error.to_string().contains("needs a place project"),
            "{error}"
        );
    }
}
