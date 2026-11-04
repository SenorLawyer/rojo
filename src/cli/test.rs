//! `rojo test`: runs a place's TestEZ or Jest-Lua specs in Roblox Studio
//! through the local Studio bridge and reports the results.
//!
//! The runner itself is the plugin's `SpecRunner` module. Callers only pass
//! options as data. [`SpecOptions`], [`TestReport`] and
//! [`standalone_runner_source`] are independent of the bridge, so `--cloud`
//! (see [`cloud`]) runs the same runner with Open Cloud Luau Execution.

mod cloud;

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use clap::{ArgEnum, Parser};
use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::{json, Value};

use super::{
    studio::{local_address, read_response},
    ExitStatus, UsageError,
};
use crate::{
    open_cloud::{OpenCloud, DEFAULT_API_KEY_VAR},
    web::studio::{max_command_request, read_token_file, studio_deadline, MAX_SPEC_SECONDS},
};

/// The runner module the Studio plugin copies into Run mode. It is plain Luau
/// with no dependencies, so it also runs as a standalone script.
const SPEC_RUNNER: &str = include_str!("../../plugin/src/SpecRunner.lua");

/// Run the place's *.spec modules in Studio's Run mode, or with --cloud in
/// Open Cloud Luau Execution, and report the results.
///
/// Studio mode requires `rojo serve --studio-token-file` and a connected
/// Studio. Exits with 0 when every test passed, 1 when a test failed or the
/// run could not finish, and 2 when the command could not reach Studio or
/// Open Cloud or was used incorrectly.
#[derive(Debug, Parser)]
pub struct TestCommand {
    /// Run headlessly with Open Cloud Luau Execution instead of Studio: build
    /// the project, save it as a new (unpublished) version of --place-id and
    /// run the specs against that version.
    #[clap(long, conflicts_with_all = &["token-file", "session", "request-id"])]
    pub cloud: bool,
    /// With --cloud: the place project to build. Defaults to the current
    /// directory.
    #[clap(
        value_name = "PROJECT",
        requires = "cloud",
        conflicts_with = "no-upload"
    )]
    pub project: Option<PathBuf>,
    /// With --cloud: the universe (experience) that holds the place.
    #[clap(long, requires = "cloud")]
    pub universe_id: Option<u64>,
    /// With --cloud: the place to save the build to and run the specs in.
    /// Use a place set aside for testing.
    #[clap(long, requires = "cloud")]
    pub place_id: Option<u64>,
    /// With --cloud: the environment variable that holds the Open Cloud API
    /// key. Defaults to ROBLOX_API_KEY.
    #[clap(long, value_name = "VAR", requires = "cloud")]
    pub api_key_env: Option<String>,
    /// With --cloud: save nothing and run against the place's current
    /// published version. This tests what was uploaded before, not the local
    /// files.
    #[clap(long, requires = "cloud")]
    pub no_upload: bool,
    /// Local rojo serve address. Remote control is intentionally unsupported.
    #[clap(long, default_value = "http://127.0.0.1:34872")]
    pub address: String,
    /// Secret token file passed to rojo serve --studio-token-file.
    #[clap(long)]
    pub token_file: Option<PathBuf>,
    /// Studio session ID from `rojo studio sessions`. Optional when exactly
    /// one session is connected.
    #[clap(long)]
    pub session: Option<String>,
    /// Instance path to search for *.spec modules, such as
    /// game/ReplicatedStorage/Shared. Repeat for several roots. Defaults to the
    /// services that hold code.
    #[clap(long = "root", value_name = "PATH")]
    pub roots: Vec<String>,
    /// Run only tests whose full name matches this pattern (a Lua pattern for
    /// TestEZ, a regular expression for Jest).
    #[clap(long)]
    pub filter: Option<String>,
    /// Test framework. `auto` uses TestEZ when the place has it, else Jest.
    #[clap(long, arg_enum, default_value = "auto")]
    pub framework: Framework,
    /// game/... path to the framework's ModuleScript. Found by name when omitted.
    #[clap(long, value_name = "PATH")]
    pub module: Option<String>,
    /// Seconds the specs may run, from 1 to 300.
    #[clap(long, default_value = "60")]
    pub timeout: u64,
    /// Print the report as JSON.
    #[clap(long)]
    pub json: bool,
    /// Assign a UUID to the command, so another process can cancel it with
    /// `rojo studio cancel` before Studio starts it.
    #[clap(long)]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, ArgEnum, PartialEq, Eq)]
pub enum Framework {
    Auto,
    Testez,
    Jest,
}

impl Framework {
    fn name(self) -> &'static str {
        match self {
            Framework::Auto => "auto",
            Framework::Testez => "testez",
            Framework::Jest => "jest",
        }
    }
}

/// What to run, in the shape the runner reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpecOptions {
    pub roots: Vec<String>,
    pub framework: Framework,
    pub module: Option<String>,
    pub filter: Option<String>,
    pub timeout_seconds: u64,
}

impl SpecOptions {
    /// Arguments for the runner's `normalizeOptions`. Unset options are left
    /// out so the runner applies its defaults.
    pub(crate) fn to_args(&self) -> Value {
        let mut args = json!({
            "framework": self.framework.name(),
            "timeoutSeconds": self.timeout_seconds,
        });
        if !self.roots.is_empty() {
            args["roots"] = json!(self.roots);
        }
        if let Some(module) = &self.module {
            args["module"] = json!(module);
        }
        if let Some(filter) = &self.filter {
            args["filter"] = json!(filter);
        }
        args
    }
}

/// Luau source that runs the specs in the DataModel it executes in and
/// returns the report, for runners without the plugin, such as Open Cloud
/// Luau Execution. Options are embedded as JSON data, not as code.
pub(crate) fn standalone_runner_source(options: &SpecOptions) -> String {
    let data = options.to_args().to_string();
    // A long bracket string cannot contain its own closing bracket, so pick a
    // level that the data does not contain.
    let mut level = String::new();
    while data.contains(&format!("]{level}]")) {
        level.push('=');
    }
    format!(
        "local SpecRunner = (function()\n{SPEC_RUNNER}\nend)()\n\
         local options = game:GetService(\"HttpService\"):JSONDecode([{level}[{data}]{level}])\n\
         return SpecRunner.runWithTimeout(options)\n"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Status {
    Passed,
    Failed,
    Error,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Failure {
    /// Phrases from the outermost describe block to the test.
    pub path: Vec<String>,
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LoadError {
    pub module: String,
    pub message: String,
}

/// The runner's report, checked and normalized.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TestReport {
    pub status: Status,
    pub framework: Option<String>,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub modules: u64,
    pub failures: Vec<Failure>,
    pub load_errors: Vec<LoadError>,
    pub omitted_failures: u64,
    pub omitted_load_errors: u64,
    pub truncated_messages: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_seconds: f64,
}

/// Luau encodes an empty table as `[]` or `{}`, so both mean an empty list.
fn list(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn count(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}

fn string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl TestReport {
    /// A run that produced no report from the runner.
    pub(crate) fn error(message: impl Into<String>) -> Self {
        TestReport {
            status: Status::Error,
            framework: None,
            passed: 0,
            failed: 0,
            skipped: 0,
            modules: 0,
            failures: Vec::new(),
            load_errors: Vec::new(),
            omitted_failures: 0,
            omitted_load_errors: 0,
            truncated_messages: 0,
            error: Some(message.into()),
            duration_seconds: 0.0,
        }
    }

    /// Reads the table the runner returned. Missing counts are zero, but an
    /// unknown status is an error, so a report that is not one fails the run
    /// instead of passing it.
    pub(crate) fn from_runner(value: &Value) -> anyhow::Result<Self> {
        let status = match value["status"].as_str() {
            Some("passed") => Status::Passed,
            Some("failed") => Status::Failed,
            Some("error") => Status::Error,
            Some("timedOut") => Status::TimedOut,
            _ => anyhow::bail!("The spec runner returned an unrecognized report"),
        };
        let failures = list(&value["failures"])
            .iter()
            .map(|failure| Failure {
                path: list(&failure["path"]).iter().map(string).collect(),
                messages: list(&failure["messages"]).iter().map(string).collect(),
            })
            .collect();
        let load_errors = list(&value["loadErrors"])
            .iter()
            .map(|error| LoadError {
                module: string(&error["module"]),
                message: string(&error["message"]),
            })
            .collect();
        let mut report = TestReport {
            status,
            framework: value["framework"].as_str().map(str::to_owned),
            passed: count(value, "passed"),
            failed: count(value, "failed"),
            skipped: count(value, "skipped"),
            modules: count(value, "modules"),
            failures,
            load_errors,
            omitted_failures: count(value, "omittedFailures"),
            omitted_load_errors: count(value, "omittedLoadErrors"),
            truncated_messages: count(value, "truncatedMessages"),
            error: value["error"].as_str().map(str::to_owned),
            duration_seconds: value["durationSeconds"].as_f64().unwrap_or(0.0),
        };
        // A report claiming success alongside failures is treated as failed.
        if report.status == Status::Passed
            && (report.failed > 0
                || !report.load_errors.is_empty()
                || report.omitted_load_errors > 0)
        {
            report.status = Status::Failed;
        }
        Ok(report)
    }

    pub(crate) fn exit_code(&self) -> i32 {
        if self.status == Status::Passed {
            0
        } else {
            1
        }
    }

    /// The human-readable report: each failure, then a summary line.
    pub(crate) fn render(&self) -> String {
        let mut output = String::new();
        let indent = |text: &str| -> String {
            text.lines()
                .map(|line| format!("    {line}\n"))
                .collect::<String>()
        };
        for error in &self.load_errors {
            let _ = writeln!(output, "LOAD ERROR {}", error.module);
            output.push_str(&indent(&error.message));
        }
        if self.omitted_load_errors > 0 {
            let _ = writeln!(
                output,
                "... {} more modules failed to load",
                self.omitted_load_errors
            );
        }
        for failure in &self.failures {
            let _ = writeln!(output, "FAIL {}", failure.path.join(" > "));
            for message in &failure.messages {
                output.push_str(&indent(message));
            }
        }
        if self.omitted_failures > 0 {
            let _ = writeln!(
                output,
                "... {} more failures not shown",
                self.omitted_failures
            );
        }
        if self.truncated_messages > 0 {
            let _ = writeln!(
                output,
                "({} messages were shortened or left out to bound the report)",
                self.truncated_messages
            );
        }
        if !output.is_empty() {
            output.push('\n');
        }
        match self.status {
            Status::Error => {
                let _ = writeln!(
                    output,
                    "Test run failed: {}",
                    self.error.as_deref().unwrap_or("unknown error")
                );
            }
            Status::TimedOut => {
                let _ = writeln!(
                    output,
                    "Test run timed out: {}",
                    self.error.as_deref().unwrap_or("the specs did not finish")
                );
            }
            Status::Passed | Status::Failed => {
                let _ = write!(
                    output,
                    "{}: {} passed, {} failed, {} skipped",
                    if self.status == Status::Passed {
                        "PASSED"
                    } else {
                        "FAILED"
                    },
                    self.passed,
                    self.failed,
                    self.skipped
                );
                if !self.load_errors.is_empty() || self.omitted_load_errors > 0 {
                    let _ = write!(
                        output,
                        ", {} modules failed to load",
                        self.load_errors.len() as u64 + self.omitted_load_errors
                    );
                }
                let _ = write!(output, " in {:.2}s", self.duration_seconds);
                if let Some(framework) = &self.framework {
                    let _ = write!(output, " ({framework}, {} spec modules)", self.modules);
                }
                output.push('\n');
            }
        }
        output
    }
}

/// Picks the session to test: the one requested, or the only one connected.
fn choose_session(listing: &Value, requested: Option<&str>) -> anyhow::Result<String> {
    if let Some(requested) = requested {
        return Ok(requested.to_owned());
    }
    let sessions = list(&listing["sessions"]);
    match sessions {
        [] => anyhow::bail!(
            "No Studio session is connected to this server. Connect the Rojo plugin, or check --address."
        ),
        [only] => only["sessionId"]
            .as_str()
            .map(str::to_owned)
            .context("The Studio bridge listed a session without an ID"),
        several => {
            let mut message = String::from(
                "Several Studio sessions are connected. Choose one with --session:\n",
            );
            for session in several {
                let _ = writeln!(
                    message,
                    "  {}  {} (place {})",
                    string(&session["sessionId"]),
                    string(&session["placeName"]),
                    string(&session["placeId"])
                );
            }
            anyhow::bail!(message.trim_end().to_owned())
        }
    }
}

/// What the bridge answered to a spec command.
#[derive(Debug, PartialEq)]
enum Answer {
    /// The plugin's result value.
    Value(Value),
    /// The run did not complete: the plugin reported an error, Studio timed
    /// out or disconnected, or the command was cancelled.
    RunFailed(String),
}

/// Statuses that mean the run itself did not complete, rather than that the
/// command could not be delivered.
fn run_failure_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 410 | 503 | 504)
}

fn read_answer(response: reqwest::blocking::Response) -> anyhow::Result<Answer> {
    let status = response.status();
    match read_response(response) {
        Ok(value) => {
            let result = &value["result"];
            if result["ok"] == json!(true) {
                Ok(Answer::Value(result["value"].clone()))
            } else {
                Ok(Answer::RunFailed(
                    result["error"]
                        .as_str()
                        .unwrap_or("Invalid plugin response")
                        .to_owned(),
                ))
            }
        }
        Err(error) if run_failure_status(status) => Ok(Answer::RunFailed(format!("{error:#}"))),
        Err(error) => Err(error),
    }
}

/// The client timeout for a run: longer than the server's own bound on the
/// command, so the server's answer arrives before the client gives up.
pub(crate) fn client_timeout(timeout_seconds: u64) -> Duration {
    let deadline = studio_deadline("spec", &json!({ "timeoutSeconds": timeout_seconds }))
        .unwrap_or(Duration::from_secs(MAX_SPEC_SECONDS));
    max_command_request(deadline) + Duration::from_secs(5)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Output<'a> {
    session_id: &'a str,
    request_id: &'a str,
    #[serde(flatten)]
    report: &'a TestReport,
}

impl TestCommand {
    pub fn run(self) -> anyhow::Result<()> {
        if self.cloud {
            return self.run_cloud();
        }
        let (session, request_id, report) = self.execute().map_err(UsageError)?;
        if self.json {
            let output = Output {
                session_id: &session,
                request_id: &request_id,
                report: &report,
            };
            println!("{}", serde_json::to_string(&output)?);
        } else {
            print!("{}", report.render());
        }
        match report.exit_code() {
            0 => Ok(()),
            code => Err(ExitStatus(code).into()),
        }
    }

    fn spec_options(&self) -> SpecOptions {
        SpecOptions {
            roots: self.roots.clone(),
            framework: self.framework,
            module: self.module.clone(),
            filter: self.filter.clone(),
            timeout_seconds: self.timeout,
        }
    }

    fn check_timeout(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=MAX_SPEC_SECONDS).contains(&self.timeout),
            "--timeout must be from 1 to {MAX_SPEC_SECONDS} seconds"
        );
        Ok(())
    }

    /// The universe and place for --cloud. Both are required, so a test run
    /// never picks a place on its own.
    fn cloud_target(&self) -> anyhow::Result<(u64, u64)> {
        match (self.universe_id, self.place_id) {
            (Some(universe), Some(place)) if universe > 0 && place > 0 => Ok((universe, place)),
            (Some(_), Some(_)) => anyhow::bail!("--universe-id and --place-id must be positive"),
            _ => anyhow::bail!(
                "--cloud needs both --universe-id and --place-id. Use a place set aside for testing:                  the build is saved to it as a new version."
            ),
        }
    }

    fn run_cloud(self) -> anyhow::Result<()> {
        let run = self.execute_cloud().map_err(UsageError)?;
        if self.json {
            println!("{}", serde_json::to_string(&run.output())?);
        } else {
            print!("{}", run.render());
        }
        match run.report.exit_code() {
            0 => Ok(()),
            code => Err(ExitStatus(code).into()),
        }
    }

    /// Everything up to the cloud run's report. As for Studio, errors are
    /// usage or connection problems.
    fn execute_cloud(&self) -> anyhow::Result<cloud::CloudRun> {
        self.check_timeout()?;
        let (universe_id, place_id) = self.cloud_target()?;
        let client =
            OpenCloud::from_env(self.api_key_env.as_deref().unwrap_or(DEFAULT_API_KEY_VAR))?;
        let place = if self.no_upload {
            None
        } else {
            let project = super::resolve_path(self.project.as_deref().unwrap_or(Path::new("")))?;
            let (name, bytes) = cloud::build_place(&project)?;
            eprintln!("Built project '{name}'.");
            Some(bytes)
        };
        let options = self.spec_options();
        cloud::run(
            &client,
            &cloud::CloudRequest {
                universe_id,
                place_id,
                place: place.as_deref(),
                options: &options,
                task_timeout: cloud::task_timeout(self.timeout),
            },
            &mut |line| eprintln!("{line}"),
        )
    }

    /// Everything up to the report. Errors here are usage or connection
    /// problems; a run that fails is a report.
    fn execute(&self) -> anyhow::Result<(String, String, TestReport)> {
        self.check_timeout()?;
        if let Some(id) = &self.request_id {
            uuid::Uuid::parse_str(id).context("--request-id must be a UUID")?;
        }
        let address = local_address(&self.address)?;
        let token_file = self.token_file.as_ref().context(
            "Pass --token-file with the secret file used by rojo serve --studio-token-file",
        )?;
        let token = read_token_file(token_file)?;
        let client = Client::builder()
            .timeout(client_timeout(self.timeout))
            .no_proxy()
            .build()?;
        let unreachable = "Could not reach local Studio bridge. Start rojo serve with --studio-token-file and connect Studio.";

        let session = if let Some(session) = &self.session {
            session.clone()
        } else {
            let response = client
                .get(address.join("api/studio/sessions")?)
                .header("x-rojo-studio-token", &token)
                .send()
                .context(unreachable)?;
            choose_session(&read_response(response)?, None)?
        };
        let options = self.spec_options();
        let request_id = self
            .request_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let response = client
            .post(address.join("api/studio/command")?)
            .header("x-rojo-studio-token", &token)
            .json(&json!({
                "sessionId": session,
                "action": "spec",
                "args": options.to_args(),
                "requestId": request_id,
            }))
            .send()
            .context(unreachable)?;
        let report = match read_answer(response)? {
            Answer::Value(value) => TestReport::from_runner(&value["report"])
                .unwrap_or_else(|error| TestReport::error(format!("{error:#}"))),
            Answer::RunFailed(message) => TestReport::error(message),
        };
        Ok((session, request_id, report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::studio::DEFAULT_SPEC_SECONDS;

    fn failing_report() -> Value {
        json!({
            "framework": "testez",
            "status": "failed",
            "passed": 3,
            "failed": 1,
            "skipped": 2,
            "modules": 4,
            "failures": [{"path": ["ReplicatedStorage", "Math", "subtracts"], "messages": ["expected 1\ngot 2"]}],
            "loadErrors": [],
            "omittedFailures": 0,
            "omittedLoadErrors": 0,
            "truncatedMessages": 0,
            "durationSeconds": 1.5,
        })
    }

    #[test]
    fn failing_reports_list_each_failure_and_exit_with_1() {
        let report = TestReport::from_runner(&failing_report()).unwrap();
        assert_eq!(report.status, Status::Failed);
        assert_eq!(report.exit_code(), 1);
        assert_eq!(
            report.render(),
            "FAIL ReplicatedStorage > Math > subtracts\n    expected 1\n    got 2\n\n\
             FAILED: 3 passed, 1 failed, 2 skipped in 1.50s (testez, 4 spec modules)\n"
        );
    }

    #[test]
    fn passing_reports_exit_with_0() {
        let report = TestReport::from_runner(&json!({
            "framework": "testez", "status": "passed", "passed": 2, "modules": 1,
            // Luau encodes empty lists as objects or arrays.
            "failures": {}, "loadErrors": [], "durationSeconds": 0.25,
        }))
        .unwrap();
        assert_eq!(report.exit_code(), 0);
        assert!(report.failures.is_empty());
        assert_eq!(
            report.render(),
            "PASSED: 2 passed, 0 failed, 0 skipped in 0.25s (testez, 1 spec modules)\n"
        );
    }

    #[test]
    fn load_errors_and_bounds_are_reported() {
        let mut value = failing_report();
        value["loadErrors"] =
            json!([{"module": "game/ReplicatedStorage/Broken.spec", "message": "syntax error"}]);
        value["omittedFailures"] = json!(7);
        value["truncatedMessages"] = json!(2);
        let rendered = TestReport::from_runner(&value).unwrap().render();
        assert!(rendered
            .starts_with("LOAD ERROR game/ReplicatedStorage/Broken.spec\n    syntax error\nFAIL "));
        assert!(rendered.contains("... 7 more failures not shown\n"));
        assert!(rendered.contains("(2 messages were shortened"));
        assert!(rendered.contains(", 1 modules failed to load in"));
    }

    #[test]
    fn a_report_cannot_pass_with_failures() {
        let mut value = failing_report();
        value["status"] = json!("passed");
        assert_eq!(
            TestReport::from_runner(&value).unwrap().status,
            Status::Failed
        );
        value["status"] = json!("ok");
        assert!(TestReport::from_runner(&value).is_err());
        assert!(TestReport::from_runner(&Value::Null).is_err());
    }

    #[test]
    fn errors_and_timeouts_exit_with_1() {
        let report = TestReport::from_runner(&json!({
            "status": "timedOut", "error": "Specs did not finish within 5 seconds",
        }))
        .unwrap();
        assert_eq!(report.exit_code(), 1);
        assert_eq!(
            report.render(),
            "Test run timed out: Specs did not finish within 5 seconds\n"
        );
        let report = TestReport::error("Could not find a ModuleScript named TestEZ");
        assert_eq!(report.exit_code(), 1);
        assert_eq!(
            report.render(),
            "Test run failed: Could not find a ModuleScript named TestEZ\n"
        );
    }

    #[test]
    fn json_output_carries_the_report_fields() {
        let report = TestReport::from_runner(&failing_report()).unwrap();
        let output = serde_json::to_value(Output {
            session_id: "session",
            request_id: "request",
            report: &report,
        })
        .unwrap();
        assert_eq!(output["sessionId"], "session");
        assert_eq!(output["status"], "failed");
        assert_eq!(output["failures"][0]["path"][2], "subtracts");
        assert_eq!(output["loadErrors"], json!([]));
        assert!(output.get("error").is_none());
        let timed_out = serde_json::to_value(TestReport {
            status: Status::TimedOut,
            ..TestReport::error("late")
        })
        .unwrap();
        assert_eq!(timed_out["status"], "timedOut");
    }

    #[test]
    fn the_only_session_is_chosen_and_several_are_listed() {
        let one = json!({"sessions": [{"sessionId": "a", "placeName": "Place", "placeId": 1}]});
        assert_eq!(choose_session(&one, None).unwrap(), "a");
        assert_eq!(choose_session(&one, Some("b")).unwrap(), "b");
        let none = json!({"sessions": []});
        assert!(choose_session(&none, None)
            .unwrap_err()
            .to_string()
            .contains("No Studio session"));
        let two = json!({"sessions": [
            {"sessionId": "a", "placeName": "First", "placeId": 1},
            {"sessionId": "b", "placeName": "Second", "placeId": 2},
        ]});
        let error = choose_session(&two, None).unwrap_err().to_string();
        assert!(error.contains("--session"), "{error}");
        assert!(error.contains("a  First (place 1)"), "{error}");
        assert!(error.contains("b  Second (place 2)"), "{error}");
    }

    #[test]
    fn options_leave_defaults_to_the_runner() {
        let mut options = SpecOptions {
            roots: Vec::new(),
            framework: Framework::Auto,
            module: None,
            filter: None,
            timeout_seconds: DEFAULT_SPEC_SECONDS,
        };
        assert_eq!(
            options.to_args(),
            json!({"framework":"auto","timeoutSeconds":60})
        );
        options.roots = vec!["game/ReplicatedStorage/Shared".into()];
        options.framework = Framework::Testez;
        options.module = Some("game/ReplicatedStorage/Packages/TestEZ".into());
        options.filter = Some("Math".into());
        assert_eq!(
            options.to_args(),
            json!({
                "framework": "testez",
                "timeoutSeconds": 60,
                "roots": ["game/ReplicatedStorage/Shared"],
                "module": "game/ReplicatedStorage/Packages/TestEZ",
                "filter": "Math",
            })
        );
    }

    #[test]
    fn standalone_source_embeds_the_runner_and_options_as_data() {
        let options = SpecOptions {
            roots: vec!["game/ReplicatedStorage/a]]b".into()],
            framework: Framework::Auto,
            module: None,
            filter: Some("]=]".into()),
            timeout_seconds: 5,
        };
        let source = standalone_runner_source(&options);
        assert!(source.starts_with("local SpecRunner = (function()\n"));
        assert!(source.contains("function SpecRunner.runWithTimeout"));
        // The data contains "]]" and "]=]", so the string needs level 2.
        let data = options.to_args().to_string();
        assert!(source.contains(&format!("JSONDecode([==[{data}]==])")));
        assert!(source.ends_with("return SpecRunner.runWithTimeout(options)\n"));
    }

    #[test]
    fn run_failures_are_told_apart_from_delivery_failures() {
        for status in [410, 503, 504] {
            assert!(run_failure_status(
                reqwest::StatusCode::from_u16(status).unwrap()
            ));
        }
        for status in [400, 401, 404, 409, 429] {
            assert!(!run_failure_status(
                reqwest::StatusCode::from_u16(status).unwrap()
            ));
        }
    }

    #[test]
    fn the_client_outlasts_the_servers_bound_on_a_run() {
        use crate::web::studio::MAX_COMMAND_REQUEST;
        // A 300-second run: 330 s for Studio plus the 45 s of other waits.
        assert_eq!(client_timeout(300), Duration::from_secs(380));
        assert!(client_timeout(1) > MAX_COMMAND_REQUEST);
    }

    #[test]
    fn usage_errors_exit_with_2_and_reported_failures_quietly() {
        use crate::cli::exit_status;
        assert_eq!(
            exit_status(&UsageError(anyhow::anyhow!("no session")).into()),
            (2, true)
        );
        assert_eq!(exit_status(&ExitStatus(1).into()), (1, false));
        assert_eq!(exit_status(&anyhow::anyhow!("other")), (1, true));
    }

    #[test]
    fn the_test_subcommand_parses() {
        use crate::cli::{Options, Subcommand};
        let options = Options::try_parse_from([
            "rojo",
            "test",
            "--root",
            "game/ReplicatedStorage/A",
            "--root",
            "game/ServerScriptService/B",
            "--framework",
            "testez",
            "--timeout",
            "120",
            "--json",
        ])
        .unwrap();
        let Subcommand::Test(command) = options.subcommand else {
            panic!("expected the test subcommand");
        };
        assert_eq!(command.roots.len(), 2);
        assert_eq!(command.framework, Framework::Testez);
        assert_eq!(command.timeout, 120);
        assert!(command.json && command.session.is_none());
    }

    fn parse_test(args: &[&str]) -> Result<TestCommand, clap::Error> {
        use crate::cli::{Options, Subcommand};
        let options = Options::try_parse_from(["rojo", "test"].iter().chain(args))?;
        let Subcommand::Test(command) = options.subcommand else {
            panic!("expected the test subcommand");
        };
        Ok(command)
    }

    #[test]
    fn cloud_options_parse_and_need_cloud() {
        let command = parse_test(&[
            "--cloud",
            "places/test.project.json",
            "--universe-id",
            "1",
            "--place-id",
            "2",
            "--api-key-env",
            "MY_KEY",
        ])
        .unwrap();
        assert!(command.cloud && !command.no_upload);
        assert_eq!(
            command.project,
            Some(PathBuf::from("places/test.project.json"))
        );
        assert_eq!(command.cloud_target().unwrap(), (1, 2));
        assert_eq!(command.api_key_env.as_deref(), Some("MY_KEY"));

        // Cloud options without --cloud, and Studio options with it, are refused.
        assert!(parse_test(&["--place-id", "2"]).is_err());
        assert!(parse_test(&["--no-upload"]).is_err());
        assert!(parse_test(&["some.project.json"]).is_err());
        assert!(parse_test(&["--cloud", "--token-file", "token"]).is_err());
        assert!(parse_test(&["--cloud", "--session", "a"]).is_err());
        // --no-upload builds nothing, so a project would be ignored.
        assert!(parse_test(&["--cloud", "--no-upload", "x.project.json"]).is_err());
        assert!(parse_test(&["--cloud", "--no-upload"]).unwrap().no_upload);
    }

    #[test]
    fn cloud_runs_need_both_ids() {
        for args in [
            &["--cloud"][..],
            &["--cloud", "--universe-id", "1"],
            &["--cloud", "--place-id", "2"],
        ] {
            let error = parse_test(args).unwrap().cloud_target().unwrap_err();
            assert!(
                error.to_string().contains("--universe-id and --place-id"),
                "{error}"
            );
        }
        let error = parse_test(&["--cloud", "--universe-id", "0", "--place-id", "2"])
            .unwrap()
            .cloud_target()
            .unwrap_err();
        assert!(error.to_string().contains("positive"));
    }

    #[test]
    fn a_cloud_run_without_ids_stops_before_reading_the_key() {
        // The variable is never set, so reaching it would give a different error.
        let mut command = parse_test(&["--cloud", "--place-id", "2"]).unwrap();
        command.api_key_env = Some("ROJO_TEST_UNSET_KEY_VARIABLE".into());
        let error = command.execute_cloud().unwrap_err().to_string();
        assert!(error.contains("--universe-id"), "{error}");
        command.universe_id = Some(1);
        let error = command.execute_cloud().unwrap_err().to_string();
        assert!(error.contains("ROJO_TEST_UNSET_KEY_VARIABLE"), "{error}");
    }
}
