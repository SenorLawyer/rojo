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

#[path = "studio_backup.rs"]
pub(crate) mod backup;

use std::io::IsTerminal;

use super::studio_lock::LockCommand;
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
    /// Check that every script Rojo syncs has the same source in Studio as in
    /// its file. Exits with 1 when something differs.
    Verify {
        #[clap(flatten)]
        target: Target,
        /// Set scripts that differ back to their files' contents.
        #[clap(long)]
        fix: bool,
        /// With --fix, also empty scripts whose emptying the plugin held back
        /// because their file became empty.
        #[clap(long, requires = "fix")]
        include_held: bool,
        /// With --fix, do not save the Studio copies of the scripts it
        /// overwrites first. Without this, --fix changes nothing when that
        /// backup cannot be saved.
        #[clap(long, requires = "fix")]
        no_backup: bool,
        /// List every script that differs, not just the first 200 of each kind
        /// (up to 10,000).
        #[clap(long, conflicts_with = "limit")]
        all: bool,
        /// List at most this many scripts of each kind (1 to 10,000).
        #[clap(long)]
        limit: Option<usize>,
        /// Bytes of JSON the lists may use together (1,024 to 4 MiB).
        #[clap(long)]
        max_bytes: Option<usize>,
    },
    /// Save copies of Studio instances outside the place, and put them back.
    Backup {
        /// The project, a project file or its folder, whose backups to use:
        /// the folder `rojo serve` saves its automatic backups in. Defaults
        /// to the current directory's.
        #[clap(long, global = true)]
        project: Option<PathBuf>,
        #[clap(subcommand)]
        command: BackupCommand,
    },
    /// Advisory leases that agents take before using a shared Studio window,
    /// for example one exclusive play test at a time. Needs no --token-file
    /// or rojo serve.
    Lock(LockCommand),
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

#[derive(Debug, Parser)]
pub enum BackupCommand {
    /// Export an instance to the project's backup folder.
    Create {
        #[clap(flatten)]
        target: Target,
        /// Instance ID or game/... path, such as game/Workspace/Map.
        #[clap(long)]
        instance: String,
        /// Short name for the backup. Defaults to the instance's name.
        #[clap(long)]
        name: Option<String>,
        /// What the backup is for, shown by `backup list`.
        #[clap(long)]
        note: Option<String>,
    },
    /// List backups, newest first.
    List {
        /// List at most this many, the newest.
        #[clap(long)]
        limit: Option<usize>,
        /// With --json, list at most this many bytes of records.
        #[clap(long)]
        max_bytes: Option<usize>,
        /// With --json, leave out each backup's list of roots and give only
        /// `rootCount`.
        #[clap(long)]
        no_roots: bool,
    },
    /// Insert a backup into Studio under a parent instance.
    Restore {
        #[clap(flatten)]
        target: Target,
        /// Backup ID from `backup list`.
        backup: String,
        /// Instance ID or game/... path to insert the backup under.
        #[clap(long)]
        parent: String,
        /// Permit script content. Restored Script/LocalScript instances remain disabled.
        #[clap(long)]
        allow_scripts: bool,
    },
    /// Delete a backup.
    Remove {
        /// Backup ID from `backup list`.
        backup: String,
    },
}

impl StudioCommand {
    pub fn run(self) -> anyhow::Result<()> {
        // Locks are files, so they work without the bridge or its token, for
        // Studios that are not connected to rojo serve too.
        if let StudioSubcommand::Lock(lock) = self.command {
            return lock.run(self.json);
        }
        // Listing and removing backups only reads local files, so they work
        // without a running server.
        if let StudioSubcommand::Backup { command, project } = &self.command {
            match command {
                BackupCommand::List {
                    limit,
                    max_bytes,
                    no_roots,
                } => {
                    let store = open_store(project.as_deref())?;
                    let mut backups: Vec<Value> = store
                        .list()?
                        .into_iter()
                        .map(|record| {
                            let count = record.roots.len();
                            let mut value = serde_json::to_value(record).unwrap_or(Value::Null);
                            if *no_roots {
                                if let Some(object) = value.as_object_mut() {
                                    object.remove("roots");
                                    object.insert("rootCount".to_owned(), json!(count));
                                }
                            }
                            value
                        })
                        .collect();
                    let total = backups.len();
                    if let Some(limit) = limit {
                        backups.truncate(*limit);
                    }
                    if let Some(max_bytes) = max_bytes {
                        let mut used = 0;
                        let fits = backups
                            .iter()
                            .take_while(|record| {
                                used += serde_json::to_string(record)
                                    .map_or(usize::MAX, |s| s.len() + 1);
                                used <= *max_bytes
                            })
                            .count();
                        backups.truncate(fits);
                    }
                    let mut orphans = store.orphans()?;
                    let orphans_total = orphans.len();
                    orphans.truncate(100);
                    if self.json {
                        return print_json(
                            &json!({
                                "backups": backups,
                                "dir": store.dir(),
                                "total": total,
                                "truncated": backups.len() < total,
                                "orphaned": orphans,
                                "orphanedTotal": orphans_total,
                            }),
                            true,
                        );
                    }
                    if !orphans.is_empty() {
                        eprintln!(
                            "Models without a record, left by an interrupted backup (remove them with `rojo studio backup remove <id>`): {}",
                            orphans.join(", ")
                        );
                    }
                    let records: Vec<backup::Record> = backups
                        .into_iter()
                        .filter_map(|value| serde_json::from_value(value).ok())
                        .collect();
                    backup::print_list(&store, &records);
                    return Ok(());
                }
                BackupCommand::Remove { backup } => {
                    let store = open_store(project.as_deref())?;
                    store.remove(backup)?;
                    return print_json(&json!({"removed": backup}), self.json);
                }
                _ => {}
            }
        }
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
            // Scripts parse stdout, which stays JSON. The summary is for
            // people reading a terminal.
            if !self.json && std::io::stderr().is_terminal() {
                eprint!("{}", describe_sessions(&result));
            }
            return print_json(&result, self.json);
        }
        if let StudioSubcommand::Verify {
            target,
            fix,
            include_held,
            no_backup,
            all,
            limit,
            max_bytes,
        } = &self.command
        {
            let mut args = json!({"fix": fix, "includeHeld": include_held, "backup": !no_backup});
            if *all {
                args["listLimit"] = json!(crate::web::verify::MAX_LISTED);
            } else if let Some(limit) = limit {
                args["listLimit"] = json!(limit);
            }
            if let Some(max_bytes) = max_bytes {
                args["maxBytes"] = json!(max_bytes);
            }
            let result = request(
                &client,
                &address,
                token.trim(),
                Some(json!({
                    "sessionId": target.session,
                    "action": "verify",
                    "args": args,
                    "requestId": self.request_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                })),
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
            let report = &response["value"];
            if !self.json {
                eprint!("{}", describe_verify(report));
            }
            print_json(&result, self.json)?;
            return if verify_passed(report) {
                Ok(())
            } else {
                Err(super::ExitStatus(1).into())
            };
        }
        if let StudioSubcommand::Backup { command, project } = &self.command {
            let store = open_store(project.as_deref())?;
            match command {
                BackupCommand::Create {
                    target,
                    instance,
                    name,
                    note,
                } => {
                    let bridge = Bridge {
                        client: &client,
                        address: &address,
                        token: token.trim(),
                        session: &target.session,
                    };
                    let exported = bridge.command("export", json!({"instance": instance}))?;
                    let model = BASE64.decode(
                        exported["model"]
                            .as_str()
                            .context("Studio did not return native model bytes")?
                            .as_bytes(),
                    )?;
                    anyhow::ensure!(
                        model.len() <= MAX_MODEL_BYTES,
                        "Export exceeds the 5MiB model limit"
                    );
                    let status = bridge.command("status", json!({}))?;
                    let described = &exported["instance"];
                    if let Some(name) = name {
                        anyhow::ensure!(
                            !name.is_empty() && name.len() <= 64,
                            super::UsageError(anyhow::anyhow!("--name must be 1 to 64 bytes"))
                        );
                    }
                    if let Some(note) = note {
                        anyhow::ensure!(
                            note.len() <= 4096,
                            super::UsageError(anyhow::anyhow!("--note must be at most 4096 bytes"))
                        );
                    }
                    let record = store.save(
                        backup::Record {
                            id: String::new(),
                            name: name.clone().unwrap_or_else(|| {
                                described["name"].as_str().unwrap_or("backup").to_owned()
                            }),
                            instance: described["path"].as_str().unwrap_or(instance).to_owned(),
                            class_name: described["className"]
                                .as_str()
                                .unwrap_or("Instance")
                                .to_owned(),
                            place_id: status["placeId"].as_u64(),
                            place_name: status["placeName"].as_str().map(str::to_owned),
                            git_head: None,
                            git_branch: None,
                            created_at: backup::now_millis(),
                            bytes: 0,
                            note: note.clone(),
                            automatic: false,
                            roots: Vec::new(),
                        },
                        &model,
                    )?;
                    if !self.json {
                        eprintln!(
                            "Saved backup {} of {} in {}",
                            record.id,
                            record.instance,
                            store.dir().display()
                        );
                    }
                    return print_json(&json!({ "backup": record }), self.json);
                }
                BackupCommand::Restore {
                    target,
                    backup,
                    parent,
                    allow_scripts,
                } => {
                    let (record, path) = store.get(backup)?;
                    let bytes = read_model(&path, *allow_scripts)?;
                    let bridge = Bridge {
                        client: &client,
                        address: &address,
                        token: token.trim(),
                        session: &target.session,
                    };
                    // The import is the step that changes the place, so it gets the
                    // caller's request ID, which `rojo studio cancel` can then stop.
                    let imported = bridge.command_as(
                        "import",
                        json!({"parent": parent, "model": BASE64.encode(&bytes), "allowScripts": allow_scripts}),
                        self.request_id
                            .clone()
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    )?;
                    return print_json(
                        &json!({"restored": record.id, "result": imported}),
                        self.json,
                    );
                }
                BackupCommand::List { .. } | BackupCommand::Remove { .. } => unreachable!(),
            }
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

/// The backup store of `--project`, or of the current directory.
fn open_store(project: Option<&Path>) -> anyhow::Result<backup::Store> {
    match project {
        Some(project) => backup::Store::for_project(project),
        None => backup::Store::open(),
    }
}

/// Whether `verify` found Studio matching the files, or, with `--fix`,
/// managed to make it match. Held wipes that were left alone still count as
/// differences, because Studio keeps code the files no longer have.
fn verify_passed(report: &Value) -> bool {
    if report["inSync"] == json!(true) {
        return true;
    }
    let count = |key: &str| report[key].as_u64().unwrap_or(0);
    let fixed = &report["fixed"];
    fixed.is_object()
        && count("missingCount") == 0
        && count("detachedCount") == 0
        && fixed["failed"].as_u64() == Some(0)
        && fixed["skippedHeld"].as_u64() == Some(0)
        && fixed["skippedChanged"].as_u64().unwrap_or(0) == 0
        && fixed["updated"].as_u64() == Some(count("differentCount"))
}

fn describe_verify(report: &Value) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let count = |key: &str| report[key].as_u64().unwrap_or(0);
    let _ = writeln!(
        out,
        "Checked {} scripts: {} match their files.",
        count("checked"),
        count("matching")
    );
    // Paths and file names come from the place and the project, so they are
    // escaped like logs before reaching the terminal.
    let line = |item: &Value| {
        let file = escape_controls(item["file"].as_str().unwrap_or("?"));
        format!(
            "{} ({file})",
            escape_controls(item["path"].as_str().unwrap_or("?"))
        )
    };
    if count("differentCount") > 0 {
        let _ = writeln!(out, "{} differ from their files:", count("differentCount"));
        for item in report["different"].as_array().into_iter().flatten() {
            let held = if item["held"] == json!(true) {
                " [held: its file is empty]"
            } else {
                ""
            };
            let _ = writeln!(out, "  {}{held}", line(item));
        }
    }
    if count("missingCount") > 0 {
        let _ = writeln!(out, "{} are not in Studio:", count("missingCount"));
        for item in report["missing"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "  {}", line(item));
        }
    }
    if count("detachedCount") > 0 {
        let _ = writeln!(
            out,
            "{} were removed from the place in Studio:",
            count("detachedCount")
        );
        for item in report["detached"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "  {}", line(item));
        }
    }
    if report["truncated"] == json!(true) {
        let _ = writeln!(
            out,
            "(Lists are cut short; --all lists every script, and --json has the counts.)"
        );
    }
    let fixed = &report["fixed"];
    for backup in report["backups"].as_array().into_iter().flatten() {
        let id = escape_controls(backup["id"].as_str().unwrap_or("?"));
        let _ = writeln!(
            out,
            "Saved the Studio copies of the scripts it overwrites as backup {id}; \
             `rojo studio backup restore {id} --parent <instance> --allow-scripts` puts them back."
        );
        if let Some(dir) = backup["storeDir"].as_str() {
            let _ = writeln!(out, "  (in {})", escape_controls(dir));
        }
    }
    if fixed.is_object() {
        let _ = writeln!(
            out,
            "Fixed {} of {} scripts.",
            fixed["updated"].as_u64().unwrap_or(0),
            fixed["attempted"].as_u64().unwrap_or(0)
        );
        if fixed["skippedChanged"].as_u64().unwrap_or(0) > 0 {
            let _ = writeln!(
                out,
                "Skipped {} scripts that changed in Studio after the check; run verify again.",
                fixed["skippedChanged"]
            );
        }
        if fixed["failed"].as_u64().unwrap_or(0) > 0 {
            let _ = writeln!(
                out,
                "{} scripts did not change in Studio; run verify again, or reconnect.",
                fixed["failed"]
            );
        }
        if fixed["skippedHeld"].as_u64().unwrap_or(0) > 0 {
            let _ = writeln!(
                out,
                "Left {} held scripts alone; pass --include-held to empty them as their files are.",
                fixed["skippedHeld"]
            );
        }
        for error in fixed["errors"].as_array().into_iter().flatten() {
            let _ = writeln!(
                out,
                "Error: {}",
                escape_controls(error.as_str().unwrap_or("?"))
            );
        }
    } else if count("differentCount") > 0 {
        let _ = writeln!(
            out,
            "Run again with --fix to set them back to their files' contents."
        );
    }
    if count("missingCount") > 0 || count("detachedCount") > 0 {
        let _ = writeln!(
            out,
            "Scripts that are missing or removed in Studio come back when Studio reconnects to Rojo."
        );
    }
    out
}

fn ago(millis: Option<u64>) -> String {
    let Some(then) = millis else {
        return "never".to_owned();
    };
    let secs = backup::now_millis().saturating_sub(then) / 1000;
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}

fn describe_sessions(result: &Value) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let sessions = result["sessions"].as_array().cloned().unwrap_or_default();
    if sessions.is_empty() {
        let _ = writeln!(out, "No Studio is connected to this server's bridge.");
        return out;
    }
    let server = crate::web::interface::SERVER_VERSION;
    for session in &sessions {
        // Everything here was sent by the plugin, so it is escaped like logs.
        let text = |value: &Value| escape_controls(value.as_str().unwrap_or("?"));
        let _ = writeln!(
            out,
            "{} \"{}\" (place {}) session {}",
            text(&session["mode"]),
            text(&session["placeName"]),
            escape_controls(&session["placeId"].to_string()),
            text(&session["sessionId"]),
        );
        let sync = &session["sync"];
        if !sync.is_object() {
            let _ = writeln!(out, "  sync: unknown (plugin too old to report it)");
            continue;
        }
        let _ = writeln!(
            out,
            "  sync: {}, last change {}, {} patches applied",
            text(&sync["status"]),
            ago(sync["lastPatchAt"].as_u64()),
            sync["patchesApplied"].as_u64().unwrap_or(0),
        );
        let mut warnings = Vec::new();
        let unapplied = sync["lastPatchUnapplied"].as_u64().unwrap_or(0);
        if unapplied > 0 {
            warnings.push(format!(
                "{unapplied} instances in the last change could not be applied"
            ));
        }
        let failed_writes = sync["failedSourceWrites"].as_u64().unwrap_or(0);
        if failed_writes > 0 {
            warnings.push(format!(
                "{failed_writes} script sources could not be written in Studio"
            ));
        }
        let undone = sync["undoneRojoPatches"].as_u64().unwrap_or(0);
        if undone > 0 {
            warnings.push(format!("{undone} Rojo changes were undone in Studio"));
        }
        let held = sync["heldWipeCount"].as_u64().unwrap_or(0);
        if held > 0 {
            warnings.push(format!("{held} scripts held back from being emptied"));
        }
        if let Some(plugin) = sync["pluginVersion"].as_str().map(escape_controls) {
            let minor = |version: &str| {
                let mut parts = version.split('.');
                (
                    parts.next().map(str::to_owned),
                    parts.next().map(str::to_owned),
                )
            };
            if minor(&plugin) != minor(server) {
                warnings.push(format!(
                    "plugin {plugin} differs from server {server}; run `rojo plugin install`"
                ));
            }
        }
        for warning in &warnings {
            let _ = writeln!(out, "  warning: {warning}");
        }
        if unapplied > 0 || undone > 0 || held > 0 || failed_writes > 0 {
            let _ = writeln!(
                out,
                "  run `rojo studio verify --session {}` to compare its scripts with the files",
                text(&session["sessionId"])
            );
        }
    }
    out
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
        self.command_as(action, args, uuid::Uuid::new_v4().to_string())
    }

    /// Like [`Self::command`], with the request ID a caller chose so that it
    /// can cancel the command.
    fn command_as(&self, action: &str, args: Value, request_id: String) -> anyhow::Result<Value> {
        let mut result = request(
            self.client,
            self.address,
            self.token,
            Some(json!({
                "sessionId": self.session,
                "action": action,
                "args": args,
                "requestId": request_id,
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
            json_escape_controls(serde_json::to_string(entry)?)
        } else {
            let kind = entry["kind"].as_str().unwrap_or("Message");
            format!(
                "[{}] {}: {}",
                entry["sequence"],
                escape_controls(
                    kind.strip_prefix("Message")
                        .filter(|kind| !kind.is_empty())
                        .unwrap_or(kind)
                ),
                // Continuation lines are indented, so a message cannot print
                // a line that looks like another entry.
                escape_controls(entry["message"].as_str().unwrap_or_default())
                    .replace('\n', "\n    ")
            )
        }));
    }
    Ok(lines)
}

/// Escapes control characters other than line breaks and tabs, as in
/// `\u{1b}`. Scripts in the place choose what they log, and printed raw, an
/// escape sequence could rewrite the terminal's title, clipboard or earlier
/// output.
pub(super) fn escape_controls(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() && !matches!(character, '\n' | '\t') {
            escaped.extend(character.escape_unicode());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn print_json(value: &Value, compact: bool) -> anyhow::Result<()> {
    println!(
        "{}",
        json_escape_controls(if compact {
            serde_json::to_string(value)?
        } else {
            serde_json::to_string_pretty(value)?
        })
    );
    Ok(())
}

/// serde_json escapes the control characters JSON requires it to, but leaves
/// DEL and the C1 controls, which some terminals read as the start of an
/// escape sequence. Outside strings JSON has no control characters, so each
/// one left can be written as a `\u` escape in place.
fn json_escape_controls(json: String) -> String {
    if !json
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return json;
    }
    let mut escaped = String::with_capacity(json.len());
    for character in json.chars() {
        if character.is_control() && character != '\n' {
            escaped.push_str(&format!("\\u{:04x}", u32::from(character)));
        } else {
            escaped.push(character);
        }
    }
    escaped
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
    model_check::prepare_restore(bytes, allow_scripts, IMPORT_CHECK_LIMIT)
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
                FollowLine::Out("[6] Error: line\n    break".into()),
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
    fn followed_entries_escape_terminal_control_characters() {
        let page = LogPage {
            entries: vec![json!({
                "sequence": 1,
                "kind": "MessageOutput\u{9b}",
                "message": "\u{1b}]52;c;aGk=\u{7}\u{1b}[2Jcarriage\rreturn\ttab\nnext \u{7f}é",
            })],
            latest_sequence: 1,
            next_after: None,
            dropped: 0,
            more: false,
            restarted: false,
        };

        assert_eq!(
            follow_lines(&page, false).unwrap(),
            vec![FollowLine::Out(
                r"[1] Output\u{9b}: \u{1b}]52;c;aGk=\u{7}\u{1b}[2Jcarriage\u{d}return".to_owned()
                    + "\ttab\n    next "
                    + r"\u{7f}é"
            )]
        );
        // JSON output escapes them too, including DEL and C1 controls that
        // serde_json leaves as they are, and still reads back the same.
        let FollowLine::Out(line) = &follow_lines(&page, true).unwrap()[0] else {
            panic!("expected an entry line");
        };
        assert!(!line.chars().any(char::is_control), "{line}");
        assert_eq!(
            serde_json::from_str::<Value>(line).unwrap(),
            page.entries[0]
        );
        let pretty = json_escape_controls(serde_json::to_string_pretty(&page.entries[0]).unwrap());
        assert!(
            !pretty.chars().any(|c| c.is_control() && c != '\n'),
            "{pretty}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&pretty).unwrap(),
            page.entries[0]
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
    fn summaries_escape_text_from_studio() {
        let esc = '\u{1b}';
        let sessions = json!({"sessions":[{
            "mode":"Edit","placeName":format!("Evil{esc}]0;title"),"placeId":1,
            "sessionId":"s","sync":{"status":format!("Connected{esc}[2J"),"patchesApplied":1}
        }]});
        let text = describe_sessions(&sessions);
        assert!(!text.contains(esc), "{text:?}");
        assert!(text.contains("\\u{1b}"));
        let report = json!({"checked":1,"matching":0,"differentCount":1,"different":[
            {"path":format!("game/A{esc}[31m"),"file":"src/A.luau","held":false}]});
        let text = describe_verify(&report);
        assert!(!text.contains(esc), "{text:?}");
    }

    #[test]
    fn backup_commands_take_a_project_before_or_after_the_subcommand() {
        use crate::cli::{Options, Subcommand};
        for args in [
            vec![
                "rojo",
                "studio",
                "backup",
                "--project",
                "game",
                "list",
                "--no-roots",
            ],
            vec![
                "rojo",
                "studio",
                "backup",
                "list",
                "--project",
                "game",
                "--no-roots",
            ],
        ] {
            let options = Options::try_parse_from(args).unwrap();
            let Subcommand::Studio(studio) = options.subcommand else {
                panic!("expected a studio command");
            };
            let StudioSubcommand::Backup { project, command } = studio.command else {
                panic!("expected a backup command");
            };
            assert_eq!(project.as_deref(), Some(Path::new("game")));
            assert!(matches!(
                command,
                BackupCommand::List { no_roots: true, .. }
            ));
        }
    }

    #[test]
    fn verify_fix_backs_up_unless_told_not_to() {
        use crate::cli::{Options, Subcommand};
        let session = "00000000-0000-4000-8000-000000000000";
        let parse = |extra: &[&str]| {
            let mut args = vec!["rojo", "studio", "verify", "--session", session];
            args.extend_from_slice(extra);
            Options::try_parse_from(args).map(|options| match options.subcommand {
                Subcommand::Studio(studio) => studio.command,
                _ => panic!("expected a studio command"),
            })
        };
        assert!(matches!(
            parse(&["--fix"]).unwrap(),
            StudioSubcommand::Verify {
                no_backup: false,
                ..
            }
        ));
        assert!(matches!(
            parse(&["--fix", "--no-backup"]).unwrap(),
            StudioSubcommand::Verify {
                no_backup: true,
                ..
            }
        ));
        // Without --fix nothing is overwritten, so there is nothing to opt out of.
        assert!(parse(&["--no-backup"]).is_err());

        let report = json!({"checked":1,"matching":0,"differentCount":1,"different":[],
            "backups":[{"id":"20261107-000000-auto-verify-fix"}],
            "fixed":{"attempted":1,"updated":1,"failed":0,"skippedChanged":0,"skippedHeld":0,"errors":[]}});
        let text = describe_verify(&report);
        assert!(
            text.contains("backup 20261107-000000-auto-verify-fix")
                && text.contains("rojo studio backup restore 20261107-000000-auto-verify-fix"),
            "{text}"
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
