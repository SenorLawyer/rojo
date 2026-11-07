//! `rojo logs`: reads the Error and Warning logs of an experience's live game
//! servers through Open Cloud's server management API (beta). See the
//! "Game server logs" entry in [`crate::open_cloud`] for the endpoints.
//!
//! Roblox only offers logs per server, and servers per place version, so one
//! read lists the place's versions, then their servers, then each server's
//! logs. All of that shares the key owner's 100 requests per minute, so
//! requests are paced and the number of servers read per round is capped.

use std::{
    collections::{HashMap, HashSet},
    io::Write as _,
    thread::sleep,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::{ArgEnum, Parser};
use serde_json::{json, Map, Value};

use super::{
    studio::{escape_controls, json_escape_controls},
    UsageError,
};
use crate::open_cloud::{
    GameServer, GameServerLog, GameServerLogPage, GameServerPage, LogSeverity, OpenCloud,
    OpenCloudError, DEFAULT_API_KEY_VAR,
};

/// Place versions read when none are given: the newest few, since servers
/// on older versions are usually gone.
const DEFAULT_VERSIONS: usize = 3;
/// Pages of 100 read per place version or server in one round.
const MAX_PAGES: usize = 10;
/// Roblox takes about three minutes to make a log entry readable, so
/// following reads back this far past the newest entry it has seen, and
/// keeps reading a server this long after it shut down.
const OVERLAP: Duration = Duration::from_secs(5 * 60);
/// Consecutive failed rounds before `--follow` gives up.
const MAX_FOLLOW_FAILURES: u32 = 5;
/// Open Cloud's documented limit for these endpoints, per key owner.
const MAX_REQUESTS_PER_MINUTE: u32 = 100;

/// Read Error and Warning logs from an experience's live game servers through
/// Open Cloud (beta).
///
/// Needs an API key with the `universe:read` scope for the experience. Roblox
/// keeps only server Error and Warning logs so far (not Output, Info or
/// client logs), and entries take about three minutes to appear. The key
/// owner's requests are limited to 100 a minute across all of their keys, so
/// this paces itself and reads at most --max-servers servers per round.
#[derive(Debug, Parser)]
pub struct LogsCommand {
    /// The universe (experience) to read logs of.
    #[clap(long)]
    pub universe_id: u64,
    /// The place in that universe.
    #[clap(long)]
    pub place_id: u64,
    /// Read servers running this place version. Can be given more than once.
    /// Defaults to the newest 3 versions Roblox lists for the place.
    #[clap(long = "place-version", value_name = "VERSION")]
    pub place_versions: Vec<String>,
    /// Read only this server (`game.JobId`). Can be given more than once.
    #[clap(long = "job-id", value_name = "JOB_ID")]
    pub job_ids: Vec<String>,
    /// Show only these severities, such as `--level error`.
    #[clap(long, arg_enum, value_delimiter = ',')]
    pub level: Vec<Level>,
    /// Show entries from this far back, such as 15m or 2h.
    #[clap(long, default_value = "1h")]
    pub since: humantime::Duration,
    /// Show only entries whose message, template, context or stack trace
    /// contains this text, ignoring case. Roblox does the matching.
    #[clap(long)]
    pub search: Option<String>,
    /// A CEL filter passed to Roblox as is, such as
    /// `message_timestamp > ...`. Combined with --search.
    #[clap(long, value_name = "CEL")]
    pub filter: Option<String>,
    /// Show at most this many of the newest entries; with --follow, before
    /// following.
    #[clap(long, default_value = "100")]
    pub limit: usize,
    /// Keep reading new entries until interrupted. With --json, prints one
    /// JSON object per entry and line.
    #[clap(long)]
    pub follow: bool,
    /// With --follow: seconds between the starts of two rounds.
    #[clap(long, default_value = "30")]
    pub interval: u64,
    /// The most servers read per round. Active servers come first, then the
    /// most recently shut down.
    #[clap(long, default_value = "20")]
    pub max_servers: usize,
    /// Open Cloud requests this command may send per minute, at most 100.
    /// Other uses of the key owner's keys count against the same 100.
    #[clap(long, default_value = "60")]
    pub requests_per_minute: u32,
    /// The environment variable that holds the API key.
    #[clap(long, value_name = "VAR", default_value = DEFAULT_API_KEY_VAR)]
    pub api_key_env: String,
    /// Print JSON.
    #[clap(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ArgEnum)]
pub enum Level {
    Error,
    Warning,
    Info,
    Output,
}

impl Level {
    fn matches(self, severity: &LogSeverity) -> bool {
        matches!(
            (self, severity),
            (Level::Error, LogSeverity::Error)
                | (Level::Warning, LogSeverity::Warning)
                | (Level::Info, LogSeverity::Info)
                | (Level::Output, LogSeverity::Output)
        )
    }
}

impl LogsCommand {
    pub fn run(self) -> anyhow::Result<()> {
        self.validate().map_err(UsageError)?;
        let client = OpenCloud::from_env(&self.api_key_env).map_err(UsageError)?;
        let mut reader = Reader::new(&client, &self);
        let mut out = std::io::stdout();
        if self.follow {
            return self.follow(&mut reader, &mut out);
        }
        let batch = reader
            .round(now_ms())
            .map_err(|error| self.explain(error))?;
        for notice in &batch.notices {
            eprintln!("{notice}");
        }
        let entries = newest(batch.entries, self.limit);
        if self.json {
            let value = json!({
                "entries": entries.iter().map(entry_json).collect::<Vec<_>>(),
                "serversRead": batch.servers_read,
                "truncated": batch.truncated,
            });
            writeln!(
                out,
                "{}",
                json_escape_controls(serde_json::to_string(&value)?)
            )?;
        } else {
            if entries.is_empty() {
                eprintln!("No matching log entries in the last {}.", self.since);
            }
            for entry in &entries {
                writeln!(out, "{}", entry_text(entry))?;
            }
        }
        Ok(())
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.universe_id > 0 && self.place_id > 0,
            "--universe-id and --place-id must be positive"
        );
        anyhow::ensure!(self.limit > 0, "--limit must be at least 1");
        anyhow::ensure!(self.max_servers > 0, "--max-servers must be at least 1");
        anyhow::ensure!(self.interval > 0, "--interval must be at least 1 second");
        anyhow::ensure!(
            (1..=MAX_REQUESTS_PER_MINUTE).contains(&self.requests_per_minute),
            "--requests-per-minute must be from 1 to {MAX_REQUESTS_PER_MINUTE}"
        );
        Ok(())
    }

    fn follow(
        &self,
        reader: &mut Reader<impl LogSource>,
        out: &mut impl std::io::Write,
    ) -> anyhow::Result<()> {
        let interval = Duration::from_secs(self.interval);
        let mut first = true;
        let mut failures = 0;
        loop {
            let started = Instant::now();
            match reader.round(now_ms()) {
                Ok(batch) => {
                    failures = 0;
                    for notice in &batch.notices {
                        eprintln!("{notice}");
                    }
                    // Like `tail -f`, start with the newest entries only.
                    let entries = if first {
                        newest(batch.entries, self.limit)
                    } else {
                        batch.entries
                    };
                    first = false;
                    for entry in &entries {
                        let line = if self.json {
                            json_escape_controls(serde_json::to_string(&entry_json(entry))?)
                        } else {
                            entry_text(entry)
                        };
                        // A reader that stops early, such as `head`, ends
                        // following like Ctrl+C does.
                        match writeln!(out, "{line}") {
                            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                                return Ok(())
                            }
                            result => result?,
                        }
                    }
                }
                Err(error) if is_usage_problem(&error) => return Err(self.explain(error)),
                Err(error) => {
                    failures += 1;
                    if failures >= MAX_FOLLOW_FAILURES {
                        return Err(
                            anyhow::Error::new(error).context("Stopped following game server logs")
                        );
                    }
                    log::warn!("Could not read game server logs, trying again: {error}");
                }
            }
            sleep(interval.saturating_sub(started.elapsed()));
        }
    }

    /// Turns errors that mean the command cannot work as given into advice.
    fn explain(&self, error: OpenCloudError) -> anyhow::Error {
        let advice = match &error {
            OpenCloudError::Status { status, .. } => match status.as_u16() {
                401 => format!(
                    "Open Cloud rejected the API key in {}: check that it is valid and not expired.",
                    self.api_key_env
                ),
                403 => format!(
                    "The API key in {} may not read game servers of universe {}. Give it the \
                     universe:read scope with this experience added under Access Permissions; \
                     its owner must also be able to read the experience.",
                    self.api_key_env, self.universe_id
                ),
                404 => "Open Cloud found no such universe, place, place version or server: \
                        check --universe-id, --place-id, --place-version and --job-id."
                    .to_owned(),
                400 => "Open Cloud refused the request; a --filter or --search it cannot \
                        parse does this."
                    .to_owned(),
                _ => return error.into(),
            },
            OpenCloudError::NotSent(_) => return UsageError(error.into()).into(),
            _ => return error.into(),
        };
        UsageError(anyhow::Error::new(error).context(advice)).into()
    }
}

/// Errors that retrying will not fix.
fn is_usage_problem(error: &OpenCloudError) -> bool {
    match error {
        OpenCloudError::Status { status, .. } => matches!(status.as_u16(), 400 | 401 | 403 | 404),
        _ => false,
    }
}

/// What `rojo logs` needs from Open Cloud, so the reading logic can be tested
/// without a network.
trait LogSource {
    fn place_versions(&self, universe: u64, place: u64) -> Result<Vec<String>, OpenCloudError>;
    fn game_servers(
        &self,
        universe: u64,
        place: u64,
        version: &str,
        token: Option<&str>,
    ) -> Result<GameServerPage, OpenCloudError>;
    #[allow(clippy::too_many_arguments)]
    fn logs(
        &self,
        universe: u64,
        place: u64,
        version: &str,
        job: &str,
        filter: &str,
        token: Option<&str>,
    ) -> Result<GameServerLogPage, OpenCloudError>;
}

impl LogSource for OpenCloud {
    fn place_versions(&self, universe: u64, place: u64) -> Result<Vec<String>, OpenCloudError> {
        OpenCloud::place_versions(self, universe, place)
    }

    fn game_servers(
        &self,
        universe: u64,
        place: u64,
        version: &str,
        token: Option<&str>,
    ) -> Result<GameServerPage, OpenCloudError> {
        self.list_game_servers(universe, place, version, token)
    }

    fn logs(
        &self,
        universe: u64,
        place: u64,
        version: &str,
        job: &str,
        filter: &str,
        token: Option<&str>,
    ) -> Result<GameServerLogPage, OpenCloudError> {
        self.list_game_server_logs(universe, place, version, job, filter, token)
    }
}

/// Spaces requests evenly so a round stays under the per-minute limit.
struct Pacer {
    spacing: Duration,
    next: Option<Instant>,
}

impl Pacer {
    fn new(per_minute: u32) -> Pacer {
        Pacer {
            spacing: Duration::from_secs(60) / per_minute.max(1),
            next: None,
        }
    }

    fn wait(&mut self) {
        let now = Instant::now();
        if let Some(next) = self.next.filter(|next| *next > now) {
            sleep(next - now);
        }
        self.next = Some(Instant::now() + self.spacing);
    }
}

/// The result of one round: entries not shown before, oldest first.
#[derive(Debug, Default)]
struct Batch {
    entries: Vec<GameServerLog>,
    notices: Vec<String>,
    servers_read: usize,
    /// Whether some servers or entries were left out by the caps.
    truncated: bool,
}

/// A server to read, with the place version its logs are filed under.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    version: String,
    job: String,
    /// When it shut down, in milliseconds; `None` while it runs or when the
    /// time could not be read.
    terminated_ms: Option<u64>,
    shut_down: bool,
}

struct Reader<'a, S> {
    source: &'a S,
    universe: u64,
    place: u64,
    versions: Vec<String>,
    jobs: Vec<String>,
    levels: Vec<Level>,
    since: Duration,
    filter: String,
    max_servers: usize,
    pacer: Pacer,
    /// Entries already shown, by [`entry_key`], with their timestamps so the
    /// set can be pruned.
    seen: HashMap<[u8; 32], u64>,
    /// The newest timestamp shown per server.
    newest: HashMap<String, u64>,
    /// Shut-down servers read since their last entries became readable.
    finished: HashSet<String>,
}

impl<'a, S: LogSource> Reader<'a, S> {
    fn new(source: &'a S, command: &LogsCommand) -> Self {
        Reader {
            source,
            universe: command.universe_id,
            place: command.place_id,
            versions: command.place_versions.clone(),
            jobs: command.job_ids.clone(),
            levels: command.level.clone(),
            since: command.since.into(),
            filter: cel_filter(command.filter.as_deref(), command.search.as_deref()),
            max_servers: command.max_servers,
            pacer: Pacer::new(command.requests_per_minute),
            seen: HashMap::new(),
            newest: HashMap::new(),
            finished: HashSet::new(),
        }
    }

    /// Reads every selected server once and returns the entries not returned
    /// by an earlier round.
    fn round(&mut self, now_ms: u64) -> Result<Batch, OpenCloudError> {
        let since_ms = now_ms.saturating_sub(self.since.as_millis() as u64);
        let overlap_ms = OVERLAP.as_millis() as u64;
        let mut batch = Batch::default();
        let targets = self.targets(since_ms, &mut batch)?;

        for target in targets {
            let cutoff = self.newest.get(&target.job).map_or(since_ms, |newest| {
                newest.saturating_sub(overlap_ms).max(since_ms)
            });
            let (logs, truncated) = self.read_server(&target, cutoff)?;
            batch.servers_read += 1;
            if truncated {
                batch.truncated = true;
                batch.notices.push(format!(
                    "Server {} has more than {} entries since the cutoff; only the newest were read.",
                    target.job,
                    MAX_PAGES * 100
                ));
            }
            for log in logs {
                if let Some(ms) = log.timestamp_ms {
                    if ms < cutoff {
                        continue;
                    }
                    let newest = self.newest.entry(target.job.clone()).or_insert(ms);
                    *newest = (*newest).max(ms);
                }
                if !self.levels.is_empty() && !self.levels.iter().any(|l| l.matches(&log.severity))
                {
                    continue;
                }
                let key = entry_key(&log);
                if self
                    .seen
                    .insert(key, log.timestamp_ms.unwrap_or(now_ms))
                    .is_none()
                {
                    batch.entries.push(log);
                }
            }
            // Its last entries are readable by now, so later rounds can skip it.
            if target.shut_down
                && target
                    .terminated_ms
                    .is_some_and(|ms| ms.saturating_add(overlap_ms) < now_ms)
            {
                self.finished.insert(target.job.clone());
            }
        }

        // Entries older than every cutoff cannot come back.
        let keep_after = self
            .newest
            .values()
            .copied()
            .min()
            .unwrap_or(now_ms)
            .min(now_ms)
            .saturating_sub(2 * overlap_ms)
            .min(since_ms);
        self.seen.retain(|_, ms| *ms >= keep_after);
        batch
            .entries
            .sort_by_key(|entry| entry.timestamp_ms.unwrap_or(now_ms));
        Ok(batch)
    }

    /// The servers to read this round.
    fn targets(&mut self, since_ms: u64, batch: &mut Batch) -> Result<Vec<Target>, OpenCloudError> {
        // A server named together with its one version can be read directly.
        if !self.jobs.is_empty() && self.versions.len() == 1 {
            return Ok(self
                .jobs
                .iter()
                .map(|job| Target {
                    version: self.versions[0].clone(),
                    job: job.clone(),
                    terminated_ms: None,
                    shut_down: false,
                })
                .collect());
        }

        let versions = if self.versions.is_empty() {
            self.pacer.wait();
            let all = self.source.place_versions(self.universe, self.place)?;
            let chosen = newest_versions(&all, DEFAULT_VERSIONS);
            if all.len() > chosen.len() {
                batch.notices.push(format!(
                    "Reading servers of place versions {} ({} of {} that Roblox lists); pass --place-version for others.",
                    chosen.join(", "),
                    chosen.len(),
                    all.len()
                ));
            }
            chosen
        } else {
            self.versions.clone()
        };

        let mut servers = Vec::new();
        for version in &versions {
            let mut token: Option<String> = None;
            for page in 0.. {
                if page == MAX_PAGES {
                    batch.truncated = true;
                    batch.notices.push(format!(
                        "Place version {version} has more than {} servers; only the first were considered.",
                        MAX_PAGES * 100
                    ));
                    break;
                }
                self.pacer.wait();
                let result = self.source.game_servers(
                    self.universe,
                    self.place,
                    version,
                    token.as_deref(),
                )?;
                servers.extend(
                    result
                        .servers
                        .into_iter()
                        .map(|server| (version.clone(), server)),
                );
                match result.next_page_token {
                    Some(next) => token = Some(next),
                    None => break,
                }
            }
        }

        let mut targets = select_servers(servers, since_ms, &self.jobs, &self.finished);
        if !self.jobs.is_empty() {
            let found: HashSet<&str> = targets.iter().map(|t| t.job.as_str()).collect();
            for job in self.jobs.iter().filter(|job| !found.contains(job.as_str())) {
                batch.notices.push(format!(
                    "Server {job} is not among the servers of place versions {} that ran in the last {}.",
                    versions.join(", "),
                    humantime::format_duration(self.since)
                ));
            }
        }
        if targets.len() > self.max_servers {
            batch.truncated = true;
            batch.notices.push(format!(
                "{} servers ran in the last {}; reading the first {}. Pass --job-id or --max-servers to choose.",
                targets.len(),
                humantime::format_duration(self.since),
                self.max_servers
            ));
            targets.truncate(self.max_servers);
        }
        Ok(targets)
    }

    /// Reads a server's entries back to `cutoff`, newest first, and whether
    /// the page cap stopped it early.
    fn read_server(
        &mut self,
        target: &Target,
        cutoff: u64,
    ) -> Result<(Vec<GameServerLog>, bool), OpenCloudError> {
        let mut logs = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            self.pacer.wait();
            let page = self.source.logs(
                self.universe,
                self.place,
                &target.version,
                &target.job,
                &self.filter,
                token.as_deref(),
            )?;
            let reached_cutoff = page
                .logs
                .last()
                .and_then(|log| log.timestamp_ms)
                .is_some_and(|ms| ms < cutoff);
            logs.extend(page.logs);
            match page.next_page_token {
                Some(next) if !reached_cutoff => token = Some(next),
                _ => return Ok((logs, false)),
            }
        }
        Ok((logs, true))
    }
}

/// The servers worth reading: those still running, then those that shut
/// down after `since_ms`, newest first. With `jobs`, only those servers.
fn select_servers(
    servers: Vec<(String, GameServer)>,
    since_ms: u64,
    jobs: &[String],
    finished: &HashSet<String>,
) -> Vec<Target> {
    let mut seen = HashSet::new();
    let mut targets: Vec<Target> = servers
        .into_iter()
        .filter(|(_, server)| jobs.is_empty() || jobs.contains(&server.job_id))
        .filter(|(_, server)| !finished.contains(&server.job_id))
        .map(|(version, server)| Target {
            terminated_ms: server
                .termination_time
                .as_deref()
                .and_then(|time| humantime::parse_rfc3339_weak(time).ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_millis() as u64),
            // The version the server reports is what its logs are filed
            // under; the one it was listed by is the fallback.
            version: if server.place_version.is_empty() {
                version
            } else {
                server.place_version
            },
            job: server.job_id,
            shut_down: server.shut_down,
        })
        // A shut-down server with an unreadable time is kept rather than
        // silently dropped.
        .filter(|target| !target.shut_down || target.terminated_ms.is_none_or(|ms| ms >= since_ms))
        .filter(|target| seen.insert(target.job.clone()))
        .collect();
    targets.sort_by_key(|target| {
        (
            target.shut_down,
            std::cmp::Reverse(target.terminated_ms.unwrap_or(u64::MAX)),
        )
    });
    targets
}

/// The `count` newest versions, numerically.
fn newest_versions(versions: &[String], count: usize) -> Vec<String> {
    let mut sorted: Vec<&String> = versions.iter().collect();
    sorted.sort_by_key(|version| std::cmp::Reverse(version.parse::<u64>().unwrap_or(0)));
    sorted.dedup();
    sorted.into_iter().take(count).cloned().collect()
}

/// The CEL filter sent to Roblox. Severity and time are filtered here instead,
/// since the docs do not say how their values are written in CEL; `search`
/// is documented as an equality with a string.
fn cel_filter(filter: Option<&str>, search: Option<&str>) -> String {
    let filter = filter.map(str::trim).filter(|filter| !filter.is_empty());
    let search = search
        .filter(|search| !search.is_empty())
        .map(|search| format!("search == {}", cel_string(search)));
    match (filter, search) {
        (Some(filter), Some(search)) => format!("({filter}) && {search}"),
        (Some(filter), None) => filter.to_owned(),
        (None, Some(search)) => search,
        (None, None) => String::new(),
    }
}

/// A CEL string literal.
fn cel_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if character.is_control() => {
                out.push_str(&format!("\\u{:04x}", character as u32))
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

/// Identifies an entry across rounds. The counts are left out because
/// Roblox may raise them on an entry it already returned.
fn entry_key(log: &GameServerLog) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for part in [
        log.job_id.as_str(),
        &log.timestamp_ms
            .map(|ms| ms.to_string())
            .unwrap_or_default(),
        log.severity.as_str(),
        &log.message,
        &log.stack_trace,
    ] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// The newest `limit` entries of an oldest-first list.
fn newest(mut entries: Vec<GameServerLog>, limit: usize) -> Vec<GameServerLog> {
    let excess = entries.len().saturating_sub(limit);
    entries.drain(..excess);
    entries
}

fn timestamp(ms: Option<u64>) -> String {
    match ms {
        Some(ms) => {
            humantime::format_rfc3339_millis(UNIX_EPOCH + Duration::from_millis(ms)).to_string()
        }
        None => "unknown time".to_owned(),
    }
}

fn entry_json(log: &GameServerLog) -> Value {
    let mut entry = Map::new();
    if let Some(ms) = log.timestamp_ms {
        entry.insert("timestamp".into(), json!(timestamp(Some(ms))));
        entry.insert("timestampMs".into(), json!(ms));
    }
    entry.insert("severity".into(), json!(log.severity.as_str()));
    entry.insert("universeId".into(), json!(log.universe_id));
    entry.insert("placeId".into(), json!(log.place_id));
    entry.insert("placeVersion".into(), json!(log.place_version));
    entry.insert("jobId".into(), json!(log.job_id));
    entry.insert("message".into(), json!(log.message));
    entry.insert("stackTrace".into(), json!(log.stack_trace));
    for (name, value) in [
        ("messageTemplate", &log.message_template),
        ("context", &log.context),
        ("loggerFullPath", &log.logger_full_path),
    ] {
        if let Some(value) = value {
            entry.insert(name.into(), json!(value));
        }
    }
    entry.insert("skippedCount".into(), json!(log.skipped_count));
    entry.insert("rateLimitedCount".into(), json!(log.rate_limited_count));
    if !log.structured_stack_trace.is_null() {
        entry.insert(
            "structuredStackTrace".into(),
            log.structured_stack_trace.clone(),
        );
    }
    Value::Object(entry)
}

/// One entry for a terminal. Everything but the layout came from game
/// servers, so control characters are escaped, and continuation lines are
/// indented so a message cannot print a line that looks like another entry.
fn entry_text(log: &GameServerLog) -> String {
    let mut text = format!(
        "{} {} [v{} {}] {}",
        timestamp(log.timestamp_ms),
        escape_controls(log.severity.as_str()).to_ascii_uppercase(),
        escape_controls(&log.place_version),
        escape_controls(&log.job_id),
        escape_controls(&log.message).replace('\n', "\n    "),
    );
    let mut counts = Vec::new();
    if log.skipped_count > 0 {
        counts.push(format!("+{} identical", log.skipped_count));
    }
    if log.rate_limited_count > 0 {
        counts.push(format!("+{} rate-limited", log.rate_limited_count));
    }
    if !counts.is_empty() {
        text.push_str(&format!(" ({})", counts.join(", ")));
    }
    for line in log
        .stack_trace
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        text.push_str("\n    ");
        text.push_str(&escape_controls(line));
    }
    text
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};

    use reqwest::StatusCode;

    const NOW: u64 = 1_794_052_800_000;
    const MINUTE: u64 = 60_000;

    fn parse(args: &[&str]) -> LogsCommand {
        let mut all = vec!["logs", "--universe-id", "1", "--place-id", "2"];
        all.extend_from_slice(args);
        LogsCommand::try_parse_from(all).unwrap()
    }

    fn log(job: &str, ms: u64, severity: LogSeverity, message: &str) -> GameServerLog {
        GameServerLog {
            timestamp_ms: Some(ms),
            severity,
            universe_id: "1".into(),
            place_id: "2".into(),
            place_version: "45".into(),
            job_id: job.into(),
            message: message.into(),
            stack_trace: String::new(),
            message_template: None,
            context: None,
            logger_full_path: None,
            skipped_count: 0,
            rate_limited_count: 0,
            structured_stack_trace: Value::Null,
        }
    }

    fn server(job: &str, shut_down: bool, terminated: Option<&str>) -> GameServer {
        GameServer {
            job_id: job.into(),
            place_version: "45".into(),
            shut_down,
            termination_time: terminated.map(str::to_owned),
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Versions,
        Servers(String, Option<String>),
        Logs(String, String, String, Option<String>),
    }

    /// Answers from queues and records every call.
    #[derive(Default)]
    struct Fake {
        versions: Vec<String>,
        servers: RefCell<VecDeque<Result<GameServerPage, OpenCloudError>>>,
        logs: RefCell<HashMap<String, VecDeque<GameServerLogPage>>>,
        calls: RefCell<Vec<Call>>,
    }

    impl Fake {
        fn servers(&self, servers: Vec<GameServer>, next: Option<&str>) {
            self.servers.borrow_mut().push_back(Ok(GameServerPage {
                servers,
                next_page_token: next.map(str::to_owned),
            }));
        }

        fn logs(&self, job: &str, logs: Vec<GameServerLog>, next: Option<&str>) {
            self.logs
                .borrow_mut()
                .entry(job.to_owned())
                .or_default()
                .push_back(GameServerLogPage {
                    logs,
                    next_page_token: next.map(str::to_owned),
                });
        }
    }

    impl LogSource for Fake {
        fn place_versions(&self, _: u64, _: u64) -> Result<Vec<String>, OpenCloudError> {
            self.calls.borrow_mut().push(Call::Versions);
            Ok(self.versions.clone())
        }

        fn game_servers(
            &self,
            _: u64,
            _: u64,
            version: &str,
            token: Option<&str>,
        ) -> Result<GameServerPage, OpenCloudError> {
            self.calls
                .borrow_mut()
                .push(Call::Servers(version.into(), token.map(str::to_owned)));
            self.servers
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(GameServerPage {
                    servers: Vec::new(),
                    next_page_token: None,
                }))
        }

        fn logs(
            &self,
            _: u64,
            _: u64,
            version: &str,
            job: &str,
            filter: &str,
            token: Option<&str>,
        ) -> Result<GameServerLogPage, OpenCloudError> {
            self.calls.borrow_mut().push(Call::Logs(
                version.into(),
                job.into(),
                filter.into(),
                token.map(str::to_owned),
            ));
            Ok(self
                .logs
                .borrow_mut()
                .get_mut(job)
                .and_then(VecDeque::pop_front)
                .unwrap_or(GameServerLogPage {
                    logs: Vec::new(),
                    next_page_token: None,
                }))
        }
    }

    fn reader<'a>(fake: &'a Fake, command: &LogsCommand) -> Reader<'a, Fake> {
        let mut reader = Reader::new(fake, command);
        reader.pacer = Pacer {
            spacing: Duration::ZERO,
            next: None,
        };
        reader
    }

    #[test]
    fn arguments_parse_with_defaults() {
        let command = parse(&[]);
        assert_eq!(Duration::from(command.since), Duration::from_secs(3600));
        assert_eq!(command.limit, 100);
        assert_eq!(command.api_key_env, "ROBLOX_API_KEY");
        assert!(command.validate().is_ok());

        let command = parse(&["--level", "error,warning", "--since", "15m", "--json"]);
        assert_eq!(command.level, [Level::Error, Level::Warning]);
        assert_eq!(Duration::from(command.since), Duration::from_secs(900));

        assert!(parse(&["--requests-per-minute", "101"]).validate().is_err());
        assert!(parse(&["--limit", "0"]).validate().is_err());
        assert!(LogsCommand::try_parse_from(["logs", "--place-id", "2"]).is_err());
    }

    #[test]
    fn search_and_filters_become_cel() {
        assert_eq!(cel_filter(None, None), "");
        assert_eq!(cel_filter(None, Some("nil")), r#"search == "nil""#);
        assert_eq!(
            cel_filter(Some("severity == 3"), Some("say \"hi\"\\")),
            r#"(severity == 3) && search == "say \"hi\"\\""#
        );
        assert_eq!(cel_filter(Some("  "), Some("")), "");
        assert_eq!(cel_string("a\nb\u{7}"), r#""a\nb\u0007""#);
    }

    #[test]
    fn a_round_lists_newest_versions_then_servers_then_logs() {
        let fake = Fake {
            versions: ["43", "45", "9", "44"].map(String::from).to_vec(),
            ..Fake::default()
        };
        fake.servers(vec![server("a", false, None)], Some("p2"));
        fake.servers(
            vec![
                // Shut down an hour and a half ago: outside --since 1h.
                server("old", true, Some("2026-11-07T10:30:00Z")),
                server("b", true, Some("2026-11-07T11:50:00Z")),
            ],
            None,
        );
        fake.logs(
            "a",
            vec![
                log("a", NOW - MINUTE, LogSeverity::Error, "boom"),
                log("a", NOW - 2 * MINUTE, LogSeverity::Warning, "careful"),
            ],
            Some("more"),
        );
        fake.logs(
            "a",
            vec![log("a", NOW - 90 * MINUTE, LogSeverity::Error, "too old")],
            Some("even more"),
        );
        fake.logs(
            "b",
            vec![log("b", NOW - 10 * MINUTE, LogSeverity::Error, "b")],
            None,
        );

        let command = parse(&["--search", "x"]);
        let mut reader = reader(&fake, &command);
        let batch = reader.round(NOW).unwrap();

        let messages: Vec<_> = batch.entries.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(messages, ["b", "careful", "boom"]);
        assert_eq!(batch.servers_read, 2);
        assert!(!batch.truncated);
        assert!(
            batch.notices[0].contains("45, 44, 43 (3 of 4"),
            "{:?}",
            batch.notices
        );

        let filter = r#"search == "x""#.to_owned();
        assert_eq!(
            *fake.calls.borrow(),
            [
                Call::Versions,
                Call::Servers("45".into(), None),
                Call::Servers("45".into(), Some("p2".into())),
                Call::Servers("44".into(), None),
                Call::Servers("43".into(), None),
                Call::Logs("45".into(), "a".into(), filter.clone(), None),
                // The second page reached the cutoff, so paging stopped.
                Call::Logs("45".into(), "a".into(), filter.clone(), Some("more".into())),
                Call::Logs("45".into(), "b".into(), filter, None),
            ]
        );
    }

    #[test]
    fn following_returns_only_new_entries_and_skips_finished_servers() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45"]);
        let mut reader = reader(&fake, &command);

        fake.servers(
            vec![
                server("a", false, None),
                server("gone", true, Some("2026-11-07T11:50:00Z")),
            ],
            None,
        );
        fake.logs(
            "a",
            vec![log("a", NOW - MINUTE, LogSeverity::Error, "first")],
            None,
        );
        let first = reader.round(NOW).unwrap();
        assert_eq!(first.entries.len(), 1);
        assert_eq!(first.servers_read, 2);

        // Later, the same entry comes back with a late arrival older than it.
        fake.servers(
            vec![
                server("a", false, None),
                server("gone", true, Some("2026-11-07T11:50:00Z")),
            ],
            None,
        );
        fake.logs(
            "a",
            vec![
                log("a", NOW + MINUTE, LogSeverity::Error, "second"),
                log("a", NOW - MINUTE, LogSeverity::Error, "first"),
                log("a", NOW - 2 * MINUTE, LogSeverity::Warning, "late"),
            ],
            None,
        );
        let second = reader.round(NOW + 2 * MINUTE).unwrap();
        let messages: Vec<_> = second.entries.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(messages, ["late", "second"]);
        // "gone" shut down more than the overlap ago and was read already.
        assert_eq!(second.servers_read, 1);
    }

    #[test]
    fn levels_are_filtered_locally() {
        let fake = Fake::default();
        let command = parse(&[
            "--place-version",
            "45",
            "--job-id",
            "a",
            "--level",
            "warning",
        ]);
        let mut reader = reader(&fake, &command);
        fake.logs(
            "a",
            vec![
                log("a", NOW - MINUTE, LogSeverity::Error, "e"),
                log("a", NOW - MINUTE, LogSeverity::Warning, "w"),
            ],
            None,
        );
        let batch = reader.round(NOW).unwrap();
        assert_eq!(batch.entries.len(), 1);
        assert_eq!(batch.entries[0].message, "w");
        // A job with its single version is read without listing servers.
        assert_eq!(
            *fake.calls.borrow(),
            [Call::Logs("45".into(), "a".into(), String::new(), None)]
        );
    }

    #[test]
    fn servers_past_the_cap_are_reported() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45", "--max-servers", "1"]);
        let mut reader = reader(&fake, &command);
        fake.servers(
            vec![
                server("shut", true, Some("2026-11-07T11:59:00Z")),
                server("live", false, None),
            ],
            None,
        );
        let batch = reader.round(NOW).unwrap();
        assert!(batch.truncated);
        assert_eq!(batch.servers_read, 1);
        // Active servers come first.
        assert!(matches!(&fake.calls.borrow()[1], Call::Logs(_, job, _, _) if job == "live"));
        assert!(
            batch.notices[0].contains("2 servers ran"),
            "{:?}",
            batch.notices
        );
    }

    #[test]
    fn missing_jobs_are_reported() {
        let fake = Fake::default();
        let command = parse(&[
            "--place-version",
            "45",
            "--place-version",
            "44",
            "--job-id",
            "x",
        ]);
        let mut reader = reader(&fake, &command);
        fake.servers(vec![server("a", false, None)], None);
        let batch = reader.round(NOW).unwrap();
        assert_eq!(batch.servers_read, 0);
        assert!(
            batch.notices[0].contains("Server x is not among"),
            "{:?}",
            batch.notices
        );
    }

    #[test]
    fn server_selection_orders_and_filters() {
        let since = NOW - 60 * MINUTE;
        let servers = vec![
            (
                "45".into(),
                server("old", true, Some("2026-11-07T10:00:00Z")),
            ),
            (
                "45".into(),
                server("recent", true, Some("2026-11-07T11:55:00Z")),
            ),
            (
                "45".into(),
                server("earlier", true, Some("2026-11-07T11:30:00Z")),
            ),
            ("45".into(), server("unreadable", true, Some("yesterday"))),
            ("44".into(), server("live", false, None)),
            ("44".into(), server("live", false, None)),
        ];
        let jobs: Vec<_> = select_servers(servers, since, &[], &HashSet::new())
            .into_iter()
            .map(|t| t.job)
            .collect();
        assert_eq!(jobs, ["live", "unreadable", "recent", "earlier"]);
        assert_eq!(
            newest_versions(&["2".into(), "10".into(), "2".into()], 5),
            ["10", "2"]
        );
    }

    #[test]
    fn entries_are_formatted_safely() {
        let mut entry = log(
            "job-1",
            1_794_052_800_123,
            LogSeverity::Error,
            "bad\nnext\u{1b}[2J",
        );
        entry.stack_trace = "Script 'Main', Line 3\n\nScript 'Other', Line 9".into();
        entry.skipped_count = 2;
        entry.rate_limited_count = 5;
        assert_eq!(
            entry_text(&entry),
            "2026-11-07T12:00:00.123Z ERROR [v45 job-1] bad\n    next\\u{1b}[2J (+2 identical, +5 rate-limited)\n    Script 'Main', Line 3\n    Script 'Other', Line 9"
        );

        let value = entry_json(&entry);
        assert_eq!(value["timestamp"], "2026-11-07T12:00:00.123Z");
        assert_eq!(value["timestampMs"], 1_794_052_800_123u64);
        assert_eq!(value["severity"], "error");
        assert_eq!(value["jobId"], "job-1");
        assert_eq!(value["skippedCount"], 2);
        assert!(value.get("messageTemplate").is_none());
        assert!(value.get("structuredStackTrace").is_none());
        assert_eq!(
            entry_text(&log("j", 0, LogSeverity::Other("7".into()), "m")),
            "1970-01-01T00:00:00.000Z 7 [v45 j] m"
        );
    }

    #[test]
    fn newest_keeps_the_last_entries() {
        let entries: Vec<_> = (0..5)
            .map(|i| log("a", i, LogSeverity::Error, "m"))
            .collect();
        let kept: Vec<_> = newest(entries, 2).iter().map(|e| e.timestamp_ms).collect();
        assert_eq!(kept, [Some(3), Some(4)]);
    }

    #[test]
    fn auth_errors_are_explained_as_usage_errors() {
        let command = parse(&["--api-key-env", "MY_KEY"]);
        let status = |code| OpenCloudError::Status {
            status: StatusCode::from_u16(code).unwrap(),
            request: "GET /server-management/v1/universes/1/places/2/game-servers:filter-options"
                .into(),
            body: String::new(),
        };
        let error = command.explain(status(403));
        assert!(error.is::<UsageError>());
        let message = format!("{error:#}");
        assert!(message.contains("universe:read"), "{message}");
        assert!(message.contains("MY_KEY"), "{message}");
        assert!(message.contains("HTTP 403"), "{message}");
        assert!(format!("{:#}", command.explain(status(401))).contains("rejected the API key"));
        assert!(!command.explain(status(500)).is::<UsageError>());
        assert!(is_usage_problem(&status(403)));
        assert!(!is_usage_problem(&status(503)));
        assert!(!is_usage_problem(&OpenCloudError::Unconfirmed("x".into())));
    }

    #[test]
    fn a_missing_key_is_a_usage_error_before_any_request() {
        let mut command = parse(&[]);
        command.api_key_env = "ROJO_TEST_UNSET_LOGS_KEY_VARIABLE".into();
        let error = command.run().unwrap_err();
        assert!(error.is::<UsageError>());
        assert!(error
            .to_string()
            .contains("ROJO_TEST_UNSET_LOGS_KEY_VARIABLE"));
    }
}
