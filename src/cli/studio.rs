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
            StudioSubcommand::Logs { target, limit } => {
                (&target.session, "logs", json!({"limit":limit}), None)
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

fn local_address(address: &str) -> anyhow::Result<Url> {
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

fn read_response(response: reqwest::blocking::Response) -> anyhow::Result<Value> {
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
