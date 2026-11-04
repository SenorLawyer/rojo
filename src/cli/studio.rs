use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context};
use clap::Parser;
use data_encoding::BASE64;
use reqwest::{blocking::Client, Url};
use serde_json::{json, Value};

use crate::web::{
    model_check,
    studio::{read_token_file, IMPORT_CHECK_LIMIT, MAX_COMMAND_REQUEST, MAX_MODEL_BYTES},
};

/// Checks an untrusted model and writes the result for the parent process.
#[derive(Debug, Parser)]
pub struct CheckUntrustedModelCommand {
    #[clap(long)]
    pub allow_scripts: bool,
    /// Write the model back out as a binary model.
    #[clap(long)]
    pub encode: bool,
}

impl CheckUntrustedModelCommand {
    pub fn run(self) -> anyhow::Result<()> {
        model_check::run_child(self.allow_scripts, self.encode)
    }
}

/// Inspect and exchange native models with an explicitly targeted Studio session.
#[derive(Debug, Parser)]
pub struct StudioCommand {
    /// Local rojo serve address. Remote control is intentionally unsupported.
    #[clap(long, global = true, default_value = "http://127.0.0.1:34872")]
    pub address: String,
    /// Secret token file passed to rojo serve --studio-token-file.
    #[clap(long, global = true)]
    pub token_file: Option<PathBuf>,
    /// Emit compact JSON for automation.
    #[clap(long, global = true)]
    pub json: bool,
    /// Assign a UUID before issuing a command, so another process can cancel it.
    #[clap(long, global = true)]
    pub request_id: Option<String>,
    #[clap(subcommand)]
    pub command: StudioSubcommand,
}

#[derive(Debug, Parser)]
pub struct Target {
    /// Exact session ID from `rojo studio sessions`.
    #[clap(long)]
    pub session: String,
}

#[derive(Debug, Parser)]
pub enum StudioSubcommand {
    /// Create a random authentication token without printing its contents.
    Token {
        #[clap(long)]
        output: PathBuf,
    },
    /// List connected Studio sessions and their project/workspace identities.
    Sessions,
    /// Cancel a queued or pending command by its caller-assigned request ID.
    Cancel {
        #[clap(flatten)]
        target: Target,
        #[clap(long)]
        request: String,
    },
    Status(Target),
    Capabilities(Target),
    Selection(Target),
    /// Run a short native Studio simulation, then return to Edit mode.
    Test {
        #[clap(flatten)]
        target: Target,
        #[clap(long, default_value = "1")]
        seconds: u8,
    },
    Tree {
        #[clap(flatten)]
        target: Target,
        #[clap(long, default_value = "game")]
        instance: String,
        #[clap(long, default_value = "3")]
        depth: u8,
        #[clap(long, default_value = "500")]
        limit: u16,
    },
    Properties {
        #[clap(flatten)]
        target: Target,
        #[clap(long)]
        instance: String,
        #[clap(long = "property", value_delimiter = ',', required = true)]
        properties: Vec<String>,
    },
    Logs {
        #[clap(flatten)]
        target: Target,
        #[clap(long, default_value = "100")]
        limit: u16,
        /// Return only entries whose sequence number is greater than this,
        /// with the sequence to pass next and how many entries were dropped.
        #[clap(long)]
        after: Option<u64>,
        /// Keep printing new entries every second until interrupted. With
        /// --json, prints one JSON object per entry and line.
        #[clap(long)]
        follow: bool,
    },
    /// Export native SerializationService bytes. Services and DataModel are unsupported.
    Export {
        #[clap(flatten)]
        target: Target,
        #[clap(long)]
        instance: String,
        #[clap(long, short)]
        output: PathBuf,
    },
    /// Insert a validated model into Studio. This does not save or publish a place.
    Import {
        #[clap(flatten)]
        target: Target,
        #[clap(long)]
        parent: String,
        #[clap(long)]
        input: PathBuf,
        /// Permit script content. Imported Script/LocalScript instances remain disabled.
        #[clap(long)]
        allow_scripts: bool,
    },
}

impl StudioCommand {
    pub fn run(self) -> anyhow::Result<()> {
        if let StudioSubcommand::Token { output } = &self.command {
            create_token(output)?;
            return print_json(&json!({"tokenFile":output,"created":true}), self.json);
        }
        let address = local_address(&self.address)?;
        let token_file = self.token_file.as_ref().context(
            "Pass --token-file with the secret file used by rojo serve --studio-token-file",
        )?;
        let token = read_token_file(token_file)?;
        // Outlast the server's own bound on a command, so a slow command (for
        // example while Git is busy) reports the server's answer, not a
        // timeout that leaves the command running in Studio.
        let client = Client::builder()
            .timeout(MAX_COMMAND_REQUEST + Duration::from_secs(5))
            .no_proxy()
            .build()?;
        if matches!(self.command, StudioSubcommand::Sessions) {
            let result = request(&client, &address, token.trim(), None)?;
            return print_json(&result, self.json);
        }
        if let StudioSubcommand::Cancel { target, request } = &self.command {
            let response = client
                .post(address.join("api/studio/cancel")?)
                .header("x-rojo-studio-token", token.trim())
                .json(&json!({"sessionId":target.session,"requestId":request}))
                .send()?;
            return print_json(&read_response(response)?, self.json);
        }
        if let StudioSubcommand::Logs {
            target,
            limit,
            after,
            follow: true,
        } = &self.command
        {
            let bridge = Bridge {
                client: &client,
                address: &address,
                token: token.trim(),
                session: &target.session,
            };
            return follow_logs(&bridge, *limit, *after, self.json);
        }
        let (session, action, args, export_path) = match &self.command {
            StudioSubcommand::Status(target) | StudioSubcommand::Capabilities(target) => {
                (&target.session, "status", json!({}), None)
            }
            StudioSubcommand::Selection(target) => (&target.session, "selection", json!({}), None),
            StudioSubcommand::Test { target, seconds } => {
                anyhow::ensure!(
                    (1..=5).contains(seconds),
                    "Native test duration must be 1-5 seconds"
                );
                (&target.session, "test", json!({"seconds":seconds}), None)
            }
            StudioSubcommand::Tree {
                target,
                instance,
                depth,
                limit,
            } => (
                &target.session,
                "tree",
                json!({"instance":instance,"depth":depth,"limit":limit}),
                None,
            ),
            StudioSubcommand::Properties {
                target,
                instance,
                properties,
            } => (
                &target.session,
                "properties",
                json!({"instance":instance,"properties":properties}),
                None,
            ),
            StudioSubcommand::Logs {
                target,
                limit,
                after,
                ..
            } => {
                let mut args = json!({"limit":limit});
                if let Some(after) = after {
                    args["after"] = json!(after);
                }
                (&target.session, "logs", args, None)
            }
            StudioSubcommand::Export {
                target,
                instance,
                output,
            } => {
                anyhow::ensure!(
                    output.extension().and_then(|extension| extension.to_str()) == Some("rbxm"),
                    "Native Studio export requires an .rbxm output path"
                );
                (
                    &target.session,
                    "export",
                    json!({"instance":instance}),
                    Some(output),
                )
            }
            StudioSubcommand::Import {
                target,
                parent,
                input,
                allow_scripts,
            } => {
                let bytes = read_model(input, *allow_scripts)?;
                (
                    &target.session,
                    "import",
                    json!({"parent":parent,"model":BASE64.encode(&bytes),"allowScripts":allow_scripts}),
                    None,
                )
            }
            _ => unreachable!(),
        };
        let mut result = request(
            &client,
            &address,
            token.trim(),
            Some(
                json!({"sessionId":session,"action":action,"args":args,"requestId":self.request_id.clone().unwrap_or_else(||uuid::Uuid::new_v4().to_string())}),
            ),
        )?;
        let response = &result["result"];
        if response.get("ok") != Some(&json!(true)) {
            bail!(
                "Studio command failed: {}",
                response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Invalid plugin response")
            );
        }
        if let StudioSubcommand::Logs { after, .. } = &self.command {
            check_logs_answer(&response["value"], *after)?;
        }
        if let Some(path) = export_path {
            let bytes = BASE64.decode(
                response["value"]["model"]
                    .as_str()
                    .context("Studio did not return native model bytes")?
                    .as_bytes(),
            )?;
            anyhow::ensure!(
                bytes.len() <= 5 * 1024 * 1024,
                "Export exceeds the 5MiB model limit"
            );
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .with_context(|| {
                    format!(
                        "Could not create {}. Existing files are never overwritten.",
                        path.display()
                    )
                })?;
            file.write_all(&bytes)?;
            result["result"]["value"]["model"] = Value::Null;
            result["result"]["value"]["output"] = json!(path);
            result["result"]["value"]["bytes"] = json!(bytes.len());
        }
        print_json(&result, self.json)
    }
}

/// The most entries one `logs` command returns; the plugin's buffer size.
const MAX_LOG_ENTRIES: u64 = 200;
const FOLLOW_INTERVAL: Duration = Duration::from_secs(1);
/// Consecutive failed polls before `--follow` gives up, so a brief hiccup
/// such as Studio entering Play mode does not end it.
const MAX_FOLLOW_FAILURES: u32 = 5;
const UNNUMBERED_LOGS: &str = "The connected Rojo plugin does not number its log entries, so it \
     cannot follow logs. Update the plugin, or run `rojo studio logs` without --follow or --after.";

/// A Studio session to send bridge commands to.
struct Bridge<'a> {
    client: &'a Client,
    address: &'a Url,
    token: &'a str,
    session: &'a str,
}

impl Bridge<'_> {
    /// Runs one command and returns the value the plugin answered with.
    fn command(&self, action: &str, args: Value) -> anyhow::Result<Value> {
        let mut result = request(
            self.client,
            self.address,
            self.token,
            Some(json!({
                "sessionId": self.session,
                "action": action,
                "args": args,
                "requestId": uuid::Uuid::new_v4().to_string(),
            })),
        )?;
        let response = &mut result["result"];
        if response.get("ok") != Some(&json!(true)) {
            bail!(
                "Studio command failed: {}",
                response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Invalid plugin response")
            );
        }
        Ok(response["value"].take())
    }
}

/// One answer to a `logs` command sent with `after`.
#[derive(Debug, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogPage {
    #[serde(default)]
    entries: Vec<Value>,
    latest_sequence: u64,
    next_after: Option<u64>,
    #[serde(default)]
    dropped: u64,
    #[serde(default)]
    more: bool,
    #[serde(default)]
    restarted: bool,
}

impl LogPage {
    fn parse(mut value: Value) -> anyhow::Result<Self> {
        // Plugins from before follow mode ignore `after` and answer with the
        // plain list of entries.
        if value.is_array() {
            bail!(UNNUMBERED_LOGS);
        }
        // Luau encodes an empty table as `[]`, but accept `{}` too.
        if value
            .get("entries")
            .is_some_and(|entries| entries.as_object().is_some_and(serde_json::Map::is_empty))
        {
            value["entries"] = json!([]);
        }
        serde_json::from_value(value).context("Invalid log response from the Studio plugin")
    }

    fn next_after(&self) -> u64 {
        self.next_after.unwrap_or(self.latest_sequence)
    }
}

/// Plugins from before numbered logs ignore `after` and answer with every
/// entry they keep, which would pass for the entries after `after` without a
/// word. Fails like `--follow` does instead.
fn check_logs_answer(value: &Value, after: Option<u64>) -> anyhow::Result<()> {
    if after.is_some() && value.is_array() {
        bail!(UNNUMBERED_LOGS);
    }
    Ok(())
}

fn follow_logs(bridge: &Bridge, limit: u16, after: Option<u64>, json: bool) -> anyhow::Result<()> {
    let limit = u64::from(limit).clamp(1, MAX_LOG_ENTRIES);
    let mut after = match after {
        Some(after) => after,
        // Like `tail -f`, start with the newest `limit` entries.
        None => {
            let page = LogPage::parse(bridge.command("logs", json!({"after":0,"limit":1}))?)?;
            page.latest_sequence.saturating_sub(limit)
        }
    };

    let mut failures = 0;
    loop {
        let page = bridge
            .command("logs", json!({"after":after,"limit":limit}))
            .and_then(LogPage::parse);
        match page {
            Ok(page) => {
                failures = 0;
                for line in follow_lines(&page, json)? {
                    match line {
                        FollowLine::Out(line) => {
                            // Piping into a reader that stops early, such as
                            // `head`, ends following like Ctrl+C does.
                            match writeln!(std::io::stdout(), "{line}") {
                                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                                    return Ok(())
                                }
                                result => result?,
                            }
                        }
                        FollowLine::Err(line) => eprintln!("{line}"),
                    }
                }
                after = page.next_after();
                if page.more {
                    continue;
                }
            }
            // An old plugin will not start numbering entries on a retry.
            Err(error) if error.to_string() == UNNUMBERED_LOGS => return Err(error),
            Err(error) => {
                failures += 1;
                if failures >= MAX_FOLLOW_FAILURES {
                    return Err(error.context("Stopped following Studio logs"));
                }
                log::warn!("Could not read Studio logs, trying again: {error:#}");
            }
        }
        std::thread::sleep(FOLLOW_INTERVAL);
    }
}

#[derive(Debug, PartialEq)]
enum FollowLine {
    Out(String),
    Err(String),
}

/// Formats one page of followed logs. Entries go to stdout, so `--json`
/// output stays one entry per line; notices about missed entries go to stderr.
fn follow_lines(page: &LogPage, json: bool) -> anyhow::Result<Vec<FollowLine>> {
    let mut lines = Vec::new();
    if page.restarted {
        lines.push(FollowLine::Err(
            "Studio log numbering started over; showing every entry still kept".to_owned(),
        ));
    }
    if page.dropped > 0 {
        lines.push(FollowLine::Err(format!(
            "{} log entries were dropped from Studio's {MAX_LOG_ENTRIES}-entry buffer before they could be read",
            page.dropped
        )));
    }
    for entry in &page.entries {
        lines.push(FollowLine::Out(if json {
            serde_json::to_string(entry)?
        } else {
            let kind = entry["kind"].as_str().unwrap_or("Message");
            format!(
                "[{}] {}: {}",
                entry["sequence"],
                kind.strip_prefix("Message")
                    .filter(|kind| !kind.is_empty())
                    .unwrap_or(kind),
                entry["message"].as_str().unwrap_or_default()
            )
        }));
    }
    Ok(lines)
}

fn print_json(value: &Value, compact: bool) -> anyhow::Result<()> {
    println!(
        "{}",
        if compact {
            serde_json::to_string(value)?
        } else {
            serde_json::to_string_pretty(value)?
        }
    );
    Ok(())
}

pub(super) fn local_address(address: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(address)?;
    let host = url.host_str().context("Studio address must have a host")?;
    anyhow::ensure!(
        url.scheme() == "http"
            && (host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())),
        "Studio control only supports an HTTP loopback address"
    );
    anyhow::ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Studio address must not contain credentials, query parameters or fragments"
    );
    url.set_path("/");
    Ok(url)
}

fn request(
    client: &Client,
    base: &Url,
    token: &str,
    command: Option<Value>,
) -> anyhow::Result<Value> {
    let request = match command {
        Some(command) => client.post(base.join("api/studio/command")?).json(&command),
        None => client.get(base.join("api/studio/sessions")?),
    };
    let response = request.header("x-rojo-studio-token", token).send().context("Could not reach local Studio bridge. Start rojo serve with --studio-token-file and connect Studio.")?;
    read_response(response)
}

pub(super) fn read_response(response: reqwest::blocking::Response) -> anyhow::Result<Value> {
    let status = response.status();
    let mut bytes = Vec::new();
    response.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 8 * 1024 * 1024,
        "Studio response exceeds 8MiB"
    );
    parse_response(status, &bytes)
}

/// Checks the status before parsing, because errors from proxies or from
/// another server on the port are often not JSON.
fn parse_response(status: reqwest::StatusCode, bytes: &[u8]) -> anyhow::Result<Value> {
    let value = serde_json::from_slice::<Value>(bytes);
    if !status.is_success() {
        let message = match &value {
            Ok(value) => value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string()),
            Err(_) => {
                let text = String::from_utf8_lossy(bytes);
                let text = text.trim();
                if text.is_empty() {
                    "Unknown error".to_owned()
                } else {
                    text.chars().take(1024).collect()
                }
            }
        };
        bail!("Studio bridge returned {status}: {message}");
    }
    value.context("Invalid Studio bridge JSON response")
}

fn create_token(path: &Path) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).context("Could not create token file. Create its parent directory first; existing tokens are never overwritten.")?;
    // Restrict access before the secret is written, since Windows files
    // otherwise inherit the parent directory's ACL.
    #[cfg(windows)]
    restrict_to_current_user(path);
    write!(
        file,
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )?;
    Ok(())
}

/// Replaces inherited permissions with full control for the current user only.
/// Failure is reported but not fatal, matching a token in a private directory.
#[cfg(windows)]
fn restrict_to_current_user(path: &Path) {
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(user)) if !domain.is_empty() => format!("{domain}\\{user}"),
        (_, Ok(user)) => user,
        _ => {
            log::warn!(
                "Could not determine the current user to restrict {}",
                path.display()
            );
            return;
        }
    };
    let result = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:F"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !result.is_ok_and(|status| status.success()) {
        log::warn!(
            "Could not restrict {} to the current user; keep it in a private directory",
            path.display()
        );
    }
}

fn read_model(path: &Path, allow_scripts: bool) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        fs::metadata(path)?.len() <= MAX_MODEL_BYTES as u64,
        "Import exceeds the 5MiB model limit"
    );
    let bytes = fs::read(path)?;
    // The same isolated check the server runs, so a model that crashes the
    // decoder is reported as invalid instead of ending this command.
    model_check::check_untrusted_model(bytes, allow_scripts, true, IMPORT_CHECK_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_check_subcommand_parses() {
        use crate::cli::{Options, Subcommand};
        let options =
            Options::try_parse_from(["rojo", model_check::SUBCOMMAND, "--encode"]).unwrap();
        assert!(matches!(
            options.subcommand,
            Subcommand::CheckUntrustedModel(CheckUntrustedModelCommand {
                encode: true,
                allow_scripts: false
            })
        ));
    }

    #[test]
    fn logs_accepts_follow_and_after() {
        use crate::cli::{Options, Subcommand};
        let session = "00000000-0000-4000-8000-000000000000";
        let options = Options::try_parse_from([
            "rojo",
            "studio",
            "--json",
            "logs",
            "--session",
            session,
            "--follow",
            "--after",
            "7",
        ])
        .unwrap();
        let Subcommand::Studio(studio) = options.subcommand else {
            panic!("expected a studio command");
        };
        assert!(studio.json);
        assert!(matches!(
            studio.command,
            StudioSubcommand::Logs {
                limit: 100,
                after: Some(7),
                follow: true,
                ..
            }
        ));

        // Both are optional, so older invocations keep working.
        let options =
            Options::try_parse_from(["rojo", "studio", "logs", "--session", session]).unwrap();
        let Subcommand::Studio(studio) = options.subcommand else {
            panic!("expected a studio command");
        };
        assert!(matches!(
            studio.command,
            StudioSubcommand::Logs {
                after: None,
                follow: false,
                ..
            }
        ));
    }

    #[test]
    fn log_pages_parse_and_tell_the_next_sequence() {
        let page = LogPage::parse(json!({
            "entries": [{"sequence": 4, "kind": "MessageWarning", "message": "careful", "timestamp": 1}],
            "latestSequence": 9,
            "nextAfter": 4,
            "dropped": 2,
            "more": true,
            "restarted": false,
        }))
        .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.next_after(), 4);
        assert_eq!(page.dropped, 2);
        assert!(page.more);

        // Luau encodes an empty table as either kind of empty JSON value, and
        // optional fields may be missing.
        for entries in [json!([]), json!({})] {
            let page = LogPage::parse(json!({"entries": entries, "latestSequence": 3})).unwrap();
            assert!(page.entries.is_empty());
            assert_eq!(page.next_after(), 3);
            assert_eq!(page.dropped, 0);
            assert!(!page.more && !page.restarted);
        }
    }

    #[test]
    fn log_pages_from_plugins_without_sequences_explain_the_update() {
        let error = LogPage::parse(json!([{"message": "hi", "kind": "MessageOutput"}]))
            .unwrap_err()
            .to_string();
        assert_eq!(error, UNNUMBERED_LOGS);
        assert!(error.contains("Update the plugin"), "{error}");
    }

    #[test]
    fn logs_after_a_sequence_need_a_plugin_that_numbers_them() {
        let unnumbered = json!([{"message": "hi", "kind": "MessageOutput"}]);
        assert_eq!(
            check_logs_answer(&unnumbered, Some(4))
                .unwrap_err()
                .to_string(),
            UNNUMBERED_LOGS
        );

        // Without --after, an old plugin's list is the answer that was asked for.
        check_logs_answer(&unnumbered, None).unwrap();
        check_logs_answer(&json!({"entries": [], "latestSequence": 3}), Some(4)).unwrap();
    }

    #[test]
    fn followed_entries_print_one_per_line() {
        let page = LogPage {
            entries: vec![
                json!({"sequence": 5, "kind": "MessageOutput", "message": "hello", "timestamp": 1}),
                json!({"sequence": 6, "kind": "MessageError", "message": "line\nbreak", "timestamp": 2}),
            ],
            latest_sequence: 6,
            next_after: None,
            dropped: 3,
            more: false,
            restarted: true,
        };

        let text = follow_lines(&page, false).unwrap();
        assert_eq!(
            text,
            vec![
                FollowLine::Err(
                    "Studio log numbering started over; showing every entry still kept".into()
                ),
                FollowLine::Err(
                    "3 log entries were dropped from Studio's 200-entry buffer before they could be read"
                        .into()
                ),
                FollowLine::Out("[5] Output: hello".into()),
                FollowLine::Out("[6] Error: line\nbreak".into()),
            ]
        );

        let json_lines = follow_lines(&page, true).unwrap();
        let FollowLine::Out(line) = &json_lines[3] else {
            panic!("expected an entry line");
        };
        assert!(!line.contains('\n'), "{line}");
        assert_eq!(
            serde_json::from_str::<Value>(line).unwrap(),
            page.entries[1]
        );
    }

    #[test]
    fn rejects_remote_or_credentialed_addresses() {
        for address in [
            "https://localhost:34872",
            "http://example.com",
            "http://127.0.0.1.evil.test",
            "http://user:password@localhost",
        ] {
            assert!(local_address(address).is_err(), "{address}");
        }
        assert!(local_address("http://127.0.0.1:34872").is_ok());
    }
    #[test]
    fn error_statuses_are_reported_before_parsing_the_body() {
        let error = parse_response(reqwest::StatusCode::BAD_GATEWAY, b"upstream unavailable")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("502") && error.contains("upstream unavailable"),
            "{error}"
        );
        let error = parse_response(
            reqwest::StatusCode::CONFLICT,
            br#"{"error":"branch changed"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("409") && error.contains("branch changed"),
            "{error}"
        );
        assert!(parse_response(reqwest::StatusCode::OK, b"not json").is_err());
        assert_eq!(
            parse_response(reqwest::StatusCode::OK, br#"{"ok":true}"#).unwrap(),
            json!({"ok":true})
        );
    }

    #[test]
    fn read_model_shares_server_import_validation() {
        use rbx_dom_weak::{InstanceBuilder, WeakDom};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.rbxm");
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let folder = dom.insert(root, InstanceBuilder::new("Folder"));
        dom.insert(folder, InstanceBuilder::new("LocalScript"));
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, &[folder]).unwrap();
        fs::write(&path, bytes).unwrap();
        assert!(read_model(&path, false).is_err());
        assert!(read_model(&path, true).is_ok());
    }

    #[test]
    fn token_creation_does_not_overwrite_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        create_token(&path).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        assert_eq!(before.len(), 64);
        assert!(create_token(&path).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), before);
    }

    #[cfg(windows)]
    #[test]
    fn windows_token_files_do_not_inherit_directory_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        create_token(&path).unwrap();
        let output = std::process::Command::new("icacls")
            .arg(&path)
            .output()
            .unwrap();
        let acl = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success());
        // Inherited entries are marked "(I)"; none may remain.
        assert!(!acl.contains("(I)"), "{acl}");
        assert!(acl.contains("(F)"), "{acl}");
    }
}
