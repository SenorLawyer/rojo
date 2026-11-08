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
    is_path_segment, GameServer, GameServerLog, GameServerLogPage, GameServerPage, LogSeverity,
    OpenCloud, OpenCloudError, DEFAULT_API_KEY_VAR,
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
/// owner's requests are limited to 100 a minute across all of their keys and
/// every program using them, including other `rojo logs` runs, so this paces
/// every request, retries included, to 50 a minute unless
/// --requests-per-minute says otherwise, and reads at most --max-servers servers
/// and 10 pages of 100 entries per server per round. A server with more is
/// reported as behind and read further in the next rounds. A server that
/// cannot be read is skipped with a warning (exit status 1 without --follow);
/// only a rejected key, a refused filter or an unknown --job-id stops it.
///
/// Assumptions from Roblox's beta docs, not checked against the live API:
/// logs come newest first when asked for `message_timestamp desc`, so reading
/// a server stops at the first page that reaches --since; and an entry
/// becomes readable about three minutes after it was logged. --follow
/// therefore keeps the --since boundary of its first read for the whole run,
/// reads each server again from 5 minutes before the newest entry it showed,
/// and keeps reading a server for 5 minutes after it shut down. An entry that
/// turns up later than that is missed. When Roblox says it could not list
/// every server, a warning says so and --json reports `"incomplete": true`.
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
    /// The most servers read per round, including those named with
    /// --job-id. Active servers come first, then the most recently shut down.
    #[clap(long, default_value = "20")]
    pub max_servers: usize,
    /// Open Cloud requests this command may send per minute, at most 100,
    /// counting each retry. Roblox's limit is per API key owner, not per key
    /// or per process: several `rojo logs` running at once, and any other use
    /// of the owner's keys, share the same 100, so give each a share. The
    /// default of 50 leaves room for one other run at the default.
    #[clap(long, default_value = "50")]
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
        let client = OpenCloud::from_env(&self.api_key_env)
            .map_err(UsageError)?
            .with_request_spacing(Duration::from_secs(60) / self.requests_per_minute);
        // Open Cloud's own errors leave the key out already, but job ids,
        // versions and messages that came back from Roblox, or from the
        // arguments, are printed too, and none of it may show the key.
        let redact = |text: &str| client.redact_key(text);
        let mut reader = Reader::new(&client, &self);
        let result = if self.follow {
            self.follow(&mut reader, &mut std::io::stdout(), &redact)
        } else {
            self.read_once(
                &mut reader,
                &mut std::io::stdout(),
                &mut std::io::stderr(),
                &redact,
                now_ms(),
            )
        };
        result.map_err(|error| redact_error(error, &redact))
    }

    /// One read without --follow: prints the newest entries, and fails when
    /// the read was not whole.
    fn read_once(
        &self,
        reader: &mut Reader<impl LogSource>,
        out: &mut impl std::io::Write,
        err: &mut impl std::io::Write,
        redact: &dyn Fn(&str) -> String,
        now_ms: u64,
    ) -> anyhow::Result<()> {
        let batch = reader.round(now_ms);
        for notice in &batch.notices {
            writeln!(err, "{}", redact(notice))?;
        }
        let failed_servers: Vec<String> =
            batch.failed_servers.iter().map(|job| redact(job)).collect();
        let entries: Vec<GameServerLog> = newest(batch.entries, self.limit)
            .iter()
            .map(|entry| redact_entry(entry, redact))
            .collect();
        // Entries from the servers read before a failure are still printed.
        if self.json {
            let mut value = json!({
                "entries": entries.iter().map(entry_json).collect::<Vec<_>>(),
                "serversRead": batch.servers_read,
                "truncated": batch.truncated,
                "incomplete": batch.incomplete,
                "failedServers": failed_servers,
            });
            if let Some(error) = &batch.error {
                value["error"] = json!(redact(&error.to_string()));
            }
            writeln!(
                out,
                "{}",
                json_escape_controls(serde_json::to_string(&value)?)
            )?;
        } else {
            if entries.is_empty() && batch.error.is_none() {
                writeln!(err, "No matching log entries in the last {}.", self.since)?;
            }
            for entry in &entries {
                writeln!(out, "{}", entry_text(entry))?;
            }
        }
        match batch.error {
            Some(error) => Err(self.explain(error)),
            // The entries were printed, but the read was not whole.
            None if !failed_servers.is_empty() => Err(anyhow::anyhow!(
                "Could not read {} of the servers: {}",
                failed_servers.len(),
                failed_servers.join(", ")
            )),
            None => Ok(()),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.universe_id > 0 && self.place_id > 0,
            "--universe-id and --place-id must be positive"
        );
        // They go into request paths, where Open Cloud refuses them before
        // sending anything, and would do so again every round.
        for (flag, values) in [
            ("--place-version", &self.place_versions),
            ("--job-id", &self.job_ids),
        ] {
            if let Some(value) = values.iter().find(|value| !is_path_segment(value)) {
                anyhow::bail!(
                    "{flag} {:?} is not valid: use only letters, digits, '-', '_' and '.'",
                    escape_controls(value)
                );
            }
        }
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
        redact: &dyn Fn(&str) -> String,
    ) -> anyhow::Result<()> {
        let interval = Duration::from_secs(self.interval);
        let mut first = true;
        let mut failures = 0;
        loop {
            let started = Instant::now();
            let batch = reader.round(now_ms());
            for notice in &batch.notices {
                eprintln!("{}", redact(notice));
            }
            // Like `tail -f`, start with the newest entries only.
            let entries = if first {
                newest(batch.entries, self.limit)
            } else {
                batch.entries
            };
            first = false;
            // The reader counts these as shown, so they are printed even
            // when a later server in the round failed.
            for entry in &entries {
                let entry = redact_entry(entry, redact);
                let line = if self.json {
                    json_escape_controls(serde_json::to_string(&entry_json(&entry))?)
                } else {
                    entry_text(&entry)
                };
                // A reader that stops early, such as `head`, ends following
                // like Ctrl+C does.
                match writeln!(out, "{line}") {
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
                    result => result?,
                }
            }
            match batch.error {
                None => failures = 0,
                Some(error) if is_usage_problem(&error) => return Err(self.explain(error)),
                Some(error) => {
                    failures += 1;
                    if failures >= MAX_FOLLOW_FAILURES {
                        return Err(
                            anyhow::Error::new(error).context("Stopped following game server logs")
                        );
                    }
                    log::warn!(
                        "Could not read game server logs, trying again: {}",
                        redact(&error.to_string())
                    );
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

/// Errors that retrying will not fix. A request that was not sent, such as
/// one with a job id that cannot go into its path, fails the same way every
/// time.
fn is_usage_problem(error: &OpenCloudError) -> bool {
    match error {
        OpenCloudError::Status { status, .. } => matches!(status.as_u16(), 400 | 401 | 403 | 404),
        OpenCloudError::NotSent(_) => true,
        _ => false,
    }
}

/// `log` with the API key taken out of every text it has. The entries come
/// from game servers, which can log anything, a key read from a secret
/// included, and all of it is printed.
fn redact_entry(log: &GameServerLog, redact: &dyn Fn(&str) -> String) -> GameServerLog {
    fn redact_value(value: &Value, redact: &dyn Fn(&str) -> String) -> Value {
        match value {
            Value::String(text) => Value::String(redact(text)),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| redact_value(item, redact))
                    .collect(),
            ),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(name, value)| (redact(name), redact_value(value, redact)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let optional = |text: &Option<String>| text.as_deref().map(|text| redact(text));
    GameServerLog {
        timestamp_ms: log.timestamp_ms,
        severity: match &log.severity {
            LogSeverity::Other(text) => LogSeverity::Other(redact(text)),
            severity => severity.clone(),
        },
        universe_id: redact(&log.universe_id),
        place_id: redact(&log.place_id),
        place_version: redact(&log.place_version),
        job_id: redact(&log.job_id),
        message: redact(&log.message),
        stack_trace: redact(&log.stack_trace),
        message_template: optional(&log.message_template),
        context: optional(&log.context),
        logger_full_path: optional(&log.logger_full_path),
        skipped_count: log.skipped_count,
        rate_limited_count: log.rate_limited_count,
        structured_stack_trace: redact_value(&log.structured_stack_trace, redact),
    }
}

/// `error` with every occurrence of the API key taken out, keeping whether
/// it is a usage error, which sets the exit status.
fn redact_error(error: anyhow::Error, redact: &dyn Fn(&str) -> String) -> anyhow::Error {
    let text = format!("{error:#}");
    let redacted = redact(&text);
    if redacted == text {
        return error;
    }
    let usage = error.is::<UsageError>();
    let replaced = anyhow::anyhow!(redacted);
    if usage {
        UsageError(replaced).into()
    } else {
        replaced
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

/// The result of one round: entries not shown before, oldest first.
#[derive(Debug, Default)]
struct Batch {
    entries: Vec<GameServerLog>,
    notices: Vec<String>,
    servers_read: usize,
    /// Whether some servers or entries were left out by the caps.
    truncated: bool,
    /// Whether Roblox said it could not list every server, so servers may
    /// have been missed.
    incomplete: bool,
    /// Whether listing servers stopped at Rojo's own page cap.
    listing_capped: bool,
    /// Servers that could not be read this round and will be tried again.
    failed_servers: Vec<String>,
    /// Why the round stopped early. `entries` still holds what the servers
    /// read before it returned, and the reader counts those as shown.
    error: Option<OpenCloudError>,
}

impl Batch {
    /// Whether the listing covered every server, so a server missing from
    /// it is really gone.
    fn listing_complete(&self) -> bool {
        !self.incomplete && !self.listing_capped
    }
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

/// What following remembers about one server.
#[derive(Debug, Default)]
struct ServerState {
    /// The newest entry timestamp read from it.
    newest: Option<u64>,
    /// Entries already shown, by [`occurrence_key`], with their timestamps
    /// so the set can be pruned once they fall behind the server's cutoff.
    seen: HashMap<[u8; 32], u64>,
    /// When it was last among the listed servers.
    last_listed_ms: u64,
    /// Older entries a read could not reach within [`MAX_PAGES`], read
    /// before anything newer in the next rounds.
    gap: Option<Gap>,
}

/// Where a read that hit the page cap stopped.
#[derive(Debug, Clone)]
struct Gap {
    /// Continues the read; `None` reads again from the newest entry, which
    /// the seen entries make safe.
    token: Option<String>,
    /// How far back the read goes.
    cutoff: u64,
    /// The oldest timestamp read so far.
    boundary: Option<u64>,
    /// Occurrence counts of the entries at `boundary`, whose identical
    /// copies may continue on the next page.
    counts: HashMap<[u8; 32], u64>,
}

/// Pages of one server's logs read in one go.
struct Read {
    logs: Vec<GameServerLog>,
    cutoff: u64,
    /// Occurrence counts the read continues from.
    counts: HashMap<[u8; 32], u64>,
    pages: usize,
    /// Set when the page budget ran out with entries left.
    next_token: Option<String>,
    /// Whether it started at the server's newest entry, rather than where an
    /// earlier read stopped.
    from_newest: bool,
}

/// Why one server could not be read, and what that means.
enum ServerFailure {
    /// The command cannot work as given; stop.
    Fatal(OpenCloudError),
    /// Roblox no longer has the server.
    Gone,
    /// Worth trying again next round.
    Transient(OpenCloudError),
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
    servers: HashMap<String, ServerState>,
    /// Servers not to read again while Roblox lists them, with when they
    /// shut down: those read after their last entries became readable, and
    /// those Roblox answered 404 for.
    finished: HashMap<String, u64>,
    /// `now - --since` at the first round: how far back servers are read
    /// that have shown nothing yet.
    history_ms: Option<u64>,
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
            servers: HashMap::new(),
            finished: HashMap::new(),
            history_ms: None,
        }
    }

    /// Reads every selected server once and returns the entries not returned
    /// by an earlier round. A server's state only changes once all of its
    /// reads this round succeeded, together with adding its entries to the
    /// batch. One server failing does not stop the others, unless the error
    /// means the command cannot work at all.
    fn round(&mut self, now_ms: u64) -> Batch {
        // Fixed at the first round: a moving bound would pass entries by
        // before Roblox makes them readable, minutes after they were logged.
        let since_ms = *self
            .history_ms
            .get_or_insert(now_ms.saturating_sub(self.since.as_millis() as u64));
        let overlap_ms = OVERLAP.as_millis() as u64;
        let mut batch = Batch::default();
        let targets = match self.targets(since_ms, now_ms, &mut batch) {
            Ok(targets) => targets,
            Err(error) => {
                batch.error = Some(error);
                return batch;
            }
        };

        for target in targets {
            let reads = match self.read_target(&target, since_ms, &mut batch) {
                Ok(reads) => reads,
                Err(ServerFailure::Fatal(error)) => {
                    batch.error = Some(error);
                    break;
                }
                Err(ServerFailure::Gone) => {
                    // Its logs expired or it never existed: skip it from now
                    // on, without a word, as Roblox's listing may lag.
                    self.servers.remove(&target.job);
                    self.finished.insert(target.job.clone(), now_ms);
                    continue;
                }
                Err(ServerFailure::Transient(error)) => {
                    batch.notices.push(format!(
                        "Warning: could not read server {}, trying again next round: {error}",
                        target.job
                    ));
                    batch.failed_servers.push(target.job.clone());
                    continue;
                }
            };
            batch.servers_read += 1;
            let read_from_newest = reads.iter().any(|read| read.from_newest);
            let state = self.servers.entry(target.job.clone()).or_default();
            state.last_listed_ms = now_ms;
            let mut gap = None;
            for read in reads {
                let counts = absorb(
                    state,
                    &self.levels,
                    read.logs,
                    read.cutoff,
                    read.counts,
                    now_ms,
                    &mut batch.entries,
                );
                if let Some(token) = read.next_token {
                    let (boundary, counts) = counts;
                    gap = Some(Gap {
                        token: Some(token),
                        cutoff: read.cutoff,
                        boundary,
                        counts,
                    });
                }
            }
            state.gap = gap;
            if let Some(gap) = &state.gap {
                batch.truncated = true;
                batch.notices.push(format!(
                    "Server {} is behind: it has more entries than {} pages a round; read back to {}, older ones are read next round.",
                    target.job,
                    MAX_PAGES,
                    timestamp(gap.boundary)
                ));
            }
            // Entries behind every cutoff still to be read are never read again.
            let keep_from = match &state.gap {
                Some(gap) => gap.cutoff.min(read_cutoff(state, since_ms)),
                None => read_cutoff(state, since_ms),
            };
            state.seen.retain(|_, ms| *ms >= keep_from);

            // Its last entries are readable by now, nothing older is left to
            // read, and this round read it from its newest entry, so later
            // rounds can skip it. A round that only drained older entries,
            // using up the page budget, has not seen the newest ones yet.
            if let Some(terminated) = target.terminated_ms.filter(|ms| {
                target.shut_down
                    && state.gap.is_none()
                    && read_from_newest
                    && ms.saturating_add(overlap_ms) < now_ms
            }) {
                self.servers.remove(&target.job);
                self.finished.insert(target.job.clone(), terminated);
            }
        }

        // Servers that left the listing are not coming back. Only a listing
        // that covered every server shows that, and servers named with
        // --job-id are always read again.
        if batch.listing_complete() {
            let jobs = &self.jobs;
            self.servers.retain(|job, state| {
                jobs.contains(job) || state.last_listed_ms.saturating_add(overlap_ms) >= now_ms
            });
        }
        batch
            .entries
            .sort_by_key(|entry| entry.timestamp_ms.unwrap_or(now_ms));
        batch
    }

    /// This round's reads of one server: what is left of an earlier capped
    /// read first, then, if the page budget allows, the newest entries.
    fn read_target(
        &self,
        target: &Target,
        since_ms: u64,
        batch: &mut Batch,
    ) -> Result<Vec<Read>, ServerFailure> {
        let state = self.servers.get(&target.job);
        let mut reads = Vec::new();
        let mut budget = MAX_PAGES;
        if let Some(gap) = state.and_then(|state| state.gap.clone()) {
            let read = match self.read_pages(target, gap.token.clone(), gap.cutoff, gap.counts.clone(), budget) {
                // Roblox may let page tokens expire; reading from the newest
                // entry again finds the same entries, already seen.
                Err(error) if gap.token.is_some() && status_of(&error) == Some(400) => {
                    batch.notices.push(format!(
                        "Roblox refused to continue reading server {}; reading it again from its newest entry.",
                        target.job
                    ));
                    self.read_pages(target, None, gap.cutoff, HashMap::new(), budget)
                }
                read => read,
            }
            .map_err(|error| self.classify(target, error))?;
            budget -= read.pages;
            let behind = read.next_token.is_some();
            reads.push(read);
            if behind {
                return Ok(reads);
            }
        }
        if budget > 0 {
            let cutoff = state.map_or(since_ms, |state| read_cutoff(state, since_ms));
            let read = self
                .read_pages(target, None, cutoff, HashMap::new(), budget)
                .map_err(|error| self.classify(target, error))?;
            reads.push(read);
        }
        Ok(reads)
    }

    fn classify(&self, target: &Target, error: OpenCloudError) -> ServerFailure {
        if matches!(error, OpenCloudError::NotSent(_)) {
            // Refused before sending, such as a listed job id that cannot go
            // into a path; it would be refused again every round.
            return ServerFailure::Fatal(error);
        }
        match status_of(&error) {
            Some(400 | 401 | 403) => ServerFailure::Fatal(error),
            // A server named with --job-id that Roblox does not know is a
            // mistake in the arguments; a listed one has gone since.
            Some(404) if self.jobs.contains(&target.job) => ServerFailure::Fatal(error),
            Some(404) => ServerFailure::Gone,
            _ => ServerFailure::Transient(error),
        }
    }

    /// The servers to read this round.
    fn targets(
        &mut self,
        since_ms: u64,
        now_ms: u64,
        batch: &mut Batch,
    ) -> Result<Vec<Target>, OpenCloudError> {
        // A server named together with its one version can be read without
        // listing servers; the cap below applies all the same.
        let mut targets = if !self.jobs.is_empty() && self.versions.len() == 1 {
            let mut seen = HashSet::new();
            self.jobs
                .iter()
                .filter(|job| seen.insert(job.as_str()))
                .map(|job| Target {
                    version: self.versions[0].clone(),
                    job: job.clone(),
                    terminated_ms: None,
                    shut_down: false,
                })
                .collect()
        } else {
            self.listed_targets(since_ms, batch)?
        };

        // Servers left out by the cap below are still listed, so what was
        // shown from them is kept.
        for target in &targets {
            if let Some(state) = self.servers.get_mut(&target.job) {
                state.last_listed_ms = now_ms;
            }
        }
        if targets.len() > self.max_servers {
            batch.truncated = true;
            batch.notices.push(format!(
                "{} servers to read; reading the first {}. Pass --job-id or --max-servers to choose.",
                targets.len(),
                self.max_servers
            ));
            targets.truncate(self.max_servers);
        }
        Ok(targets)
    }

    /// The servers worth reading among those Roblox lists for the place
    /// versions.
    fn listed_targets(
        &mut self,
        since_ms: u64,
        batch: &mut Batch,
    ) -> Result<Vec<Target>, OpenCloudError> {
        let versions = if self.versions.is_empty() {
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
                    batch.listing_capped = true;
                    batch.notices.push(format!(
                        "Place version {version} has more than {} servers; only the first were considered.",
                        MAX_PAGES * 100
                    ));
                    break;
                }
                let result = self.source.game_servers(
                    self.universe,
                    self.place,
                    version,
                    token.as_deref(),
                )?;
                if result.incomplete && !batch.incomplete {
                    batch.incomplete = true;
                    batch.notices.push(format!(
                        "Warning: Roblox could not list all servers of place version {version}, so some servers may not have been read."
                    ));
                }
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

        // A finished server stays skipped while Roblox lists it; one that a
        // listing covering every server leaves out can be forgotten.
        if batch.listing_complete() {
            let listed: HashSet<&str> = servers.iter().map(|(_, s)| s.job_id.as_str()).collect();
            self.finished.retain(|job, _| listed.contains(job.as_str()));
        }
        let targets = select_servers(servers, since_ms, &self.jobs, &self.finished);
        if !self.jobs.is_empty() {
            let found: HashSet<&str> = targets.iter().map(|t| t.job.as_str()).collect();
            for job in self.jobs.iter().filter(|job| !found.contains(job.as_str())) {
                batch.notices.push(format!(
                    "Server {job} is not among the servers of place versions {} that ran since {}.",
                    versions.join(", "),
                    timestamp(Some(since_ms))
                ));
            }
        }
        Ok(targets)
    }

    /// Reads pages of a server's entries, newest first, from `token` (or the
    /// newest entry) back to `cutoff`, at most `budget` pages.
    fn read_pages(
        &self,
        target: &Target,
        mut token: Option<String>,
        cutoff: u64,
        counts: HashMap<[u8; 32], u64>,
        budget: usize,
    ) -> Result<Read, OpenCloudError> {
        let from_newest = token.is_none();
        let mut logs = Vec::new();
        for pages in 1..=budget {
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
                _ => {
                    return Ok(Read {
                        logs,
                        cutoff,
                        counts,
                        pages,
                        next_token: None,
                        from_newest,
                    })
                }
            }
        }
        Ok(Read {
            logs,
            cutoff,
            counts,
            pages: budget,
            next_token: token,
            from_newest,
        })
    }
}

/// Adds one read's entries to a server's state and the new ones to `out`,
/// and returns the oldest timestamp read with the occurrence counts of the
/// entries at it, for a read that continues on the next page.
fn absorb(
    state: &mut ServerState,
    levels: &[Level],
    logs: Vec<GameServerLog>,
    cutoff: u64,
    counts: HashMap<[u8; 32], u64>,
    now_ms: u64,
    out: &mut Vec<GameServerLog>,
) -> (Option<u64>, HashMap<[u8; 32], u64>) {
    // Identical entries within one read are separate occurrences; numbering
    // them tells a repeat apart from a re-read.
    let mut occurrences: HashMap<[u8; 32], (u64, Option<u64>)> = counts
        .into_iter()
        .map(|(key, count)| (key, (count, None)))
        .collect();
    let mut oldest = None;
    for log in logs {
        if let Some(ms) = log.timestamp_ms {
            if ms < cutoff {
                continue;
            }
            state.newest = Some(state.newest.map_or(ms, |newest| newest.max(ms)));
        }
        oldest = log.timestamp_ms.or(oldest);
        let base = entry_key(&log);
        let entry = occurrences.entry(base).or_insert((0, None));
        let key = occurrence_key(&base, entry.0);
        entry.0 += 1;
        entry.1 = log.timestamp_ms;
        let shown = state
            .seen
            .insert(key, log.timestamp_ms.unwrap_or(now_ms))
            .is_some();
        let wanted = levels.is_empty() || levels.iter().any(|l| l.matches(&log.severity));
        if !shown && wanted {
            out.push(log);
        }
    }
    let at_oldest = occurrences
        .into_iter()
        .filter(|(_, (_, ms))| ms.is_some() && *ms == oldest)
        .map(|(key, (count, _))| (key, count))
        .collect();
    (oldest, at_oldest)
}

fn status_of(error: &OpenCloudError) -> Option<u16> {
    match error {
        OpenCloudError::Status { status, .. } => Some(status.as_u16()),
        _ => None,
    }
}

/// How far back a server is read: to the overlap before its newest entry,
/// but never before the boundary --since set at the first round, which
/// stays fixed while following.
fn read_cutoff(state: &ServerState, since_ms: u64) -> u64 {
    state.newest.map_or(since_ms, |newest| {
        newest
            .saturating_sub(OVERLAP.as_millis() as u64)
            .max(since_ms)
    })
}

/// The servers worth reading: those still running, then those that shut
/// down after `since_ms`, newest first. With `jobs`, only those servers.
fn select_servers(
    servers: Vec<(String, GameServer)>,
    since_ms: u64,
    jobs: &[String],
    finished: &HashMap<String, u64>,
) -> Vec<Target> {
    let mut seen = HashSet::new();
    let mut targets: Vec<Target> = servers
        .into_iter()
        .filter(|(_, server)| jobs.is_empty() || jobs.contains(&server.job_id))
        .filter(|(_, server)| !finished.contains_key(&server.job_id))
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

/// Every field that tells two entries apart. The counts are left out
/// because Roblox may raise them on an entry it already returned.
fn entry_key(log: &GameServerLog) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    let timestamp = log.timestamp_ms.map(|ms| ms.to_string());
    let structured =
        (!log.structured_stack_trace.is_null()).then(|| log.structured_stack_trace.to_string());
    for part in [
        Some(log.job_id.as_str()),
        Some(log.place_version.as_str()),
        timestamp.as_deref(),
        Some(log.severity.as_str()),
        Some(&log.message),
        Some(&log.stack_trace),
        log.message_template.as_deref(),
        log.context.as_deref(),
        log.logger_full_path.as_deref(),
        structured.as_deref(),
    ] {
        // Length-prefixed, with absent fields distinct from empty ones, so
        // no two different entries hash the same parts.
        match part {
            Some(part) => {
                hasher.update(&[1]);
                hasher.update(&(part.len() as u64).to_le_bytes());
                hasher.update(part.as_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }
    *hasher.finalize().as_bytes()
}

/// The key of the `index`th entry with key `base` in one read of a server.
/// Identical entries share a timestamp, so a read either covers all of them
/// or none, and the numbering is the same in every round that reads them.
fn occurrence_key(base: &[u8; 32], index: u64) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(base);
    hasher.update(&index.to_le_bytes());
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
        logs: RefCell<HashMap<String, VecDeque<Result<GameServerLogPage, OpenCloudError>>>>,
        calls: RefCell<Vec<Call>>,
    }

    impl Fake {
        fn servers(&self, servers: Vec<GameServer>, next: Option<&str>) {
            self.servers.borrow_mut().push_back(Ok(GameServerPage {
                servers,
                next_page_token: next.map(str::to_owned),
                incomplete: false,
            }));
        }

        fn partial_servers(&self, servers: Vec<GameServer>) {
            self.servers.borrow_mut().push_back(Ok(GameServerPage {
                servers,
                next_page_token: None,
                incomplete: true,
            }));
        }

        fn logs(&self, job: &str, logs: Vec<GameServerLog>, next: Option<&str>) {
            self.logs
                .borrow_mut()
                .entry(job.to_owned())
                .or_default()
                .push_back(Ok(GameServerLogPage {
                    logs,
                    next_page_token: next.map(str::to_owned),
                }));
        }

        fn status_logs(&self, job: &str, code: u16) {
            self.logs
                .borrow_mut()
                .entry(job.to_owned())
                .or_default()
                .push_back(Err(status_error(code)));
        }

        fn failing_logs(&self, job: &str) {
            self.logs
                .borrow_mut()
                .entry(job.to_owned())
                .or_default()
                .push_back(Err(OpenCloudError::Unconfirmed("connection reset".into())));
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
                    incomplete: false,
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
            self.logs
                .borrow_mut()
                .get_mut(job)
                .and_then(VecDeque::pop_front)
                .unwrap_or(Ok(GameServerLogPage {
                    logs: Vec::new(),
                    next_page_token: None,
                }))
        }
    }

    fn reader<'a>(fake: &'a Fake, command: &LogsCommand) -> Reader<'a, Fake> {
        Reader::new(fake, command)
    }

    fn messages(batch: &Batch) -> Vec<&str> {
        batch.entries.iter().map(|e| e.message.as_str()).collect()
    }

    fn status_error(code: u16) -> OpenCloudError {
        OpenCloudError::Status {
            status: StatusCode::from_u16(code).unwrap(),
            request: "GET /server-management/v1/...".into(),
            body: String::new(),
        }
    }

    /// The page tokens of the log requests for `job`, in order.
    fn log_tokens(fake: &Fake, job: &str) -> Vec<Option<String>> {
        fake.calls
            .borrow()
            .iter()
            .filter_map(|call| match call {
                Call::Logs(_, j, _, token) if j == job => Some(token.clone()),
                _ => None,
            })
            .collect()
    }

    /// Queues `pages` pages of one entry each for `job`, newest first, each
    /// but the last pointing to the next.
    fn queue_pages(fake: &Fake, job: &str, pages: u64, newest: u64) {
        for i in 0..pages {
            let next = (i + 1 < pages).then(|| format!("t{i}"));
            fake.logs(
                job,
                vec![log(
                    job,
                    newest - i * 1000,
                    LogSeverity::Error,
                    &format!("e{i}"),
                )],
                next.as_deref(),
            );
        }
    }

    #[test]
    fn a_server_past_the_page_cap_is_drained_in_later_rounds() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45"]);
        let mut reader = reader(&fake, &command);
        // Shut down long enough ago that it would be retired at once.
        let ended = || vec![server("a", true, Some("2026-11-07T11:50:00Z"))];

        fake.servers(ended(), None);
        queue_pages(&fake, "a", MAX_PAGES as u64 + 2, NOW - MINUTE);
        let first = reader.round(NOW);
        assert_eq!(first.entries.len(), MAX_PAGES);
        assert!(first.truncated);
        assert!(
            first.notices.iter().any(|n| n.contains("is behind")),
            "{:?}",
            first.notices
        );
        // Not retired while entries are left.
        assert!(!reader.finished.contains_key("a"));
        assert!(reader.servers["a"].gap.is_some());

        fake.servers(ended(), None);
        let second = reader.round(NOW + MINUTE);
        assert_eq!(messages(&second), ["e11", "e10"]);
        assert!(!second.truncated);
        // The drain continued from the saved token.
        let tokens = log_tokens(&fake, "a");
        assert_eq!(tokens[MAX_PAGES], Some(format!("t{}", MAX_PAGES - 1)));
        assert!(reader.finished.contains_key("a"));
    }

    #[test]
    fn a_drain_that_uses_the_whole_budget_does_not_retire_the_server() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45"]);
        let mut reader = reader(&fake, &command);
        let ended = || vec![server("a", true, Some("2026-11-07T11:50:00Z"))];

        // Exactly two rounds' worth of pages: the second round drains the
        // rest with its whole budget and has none left to read from the top.
        fake.servers(ended(), None);
        queue_pages(&fake, "a", 2 * MAX_PAGES as u64, NOW - MINUTE);
        reader.round(NOW);
        fake.servers(ended(), None);
        let second = reader.round(NOW + MINUTE);
        assert_eq!(second.entries.len(), MAX_PAGES);
        assert!(reader.servers["a"].gap.is_none());
        assert!(
            !reader.finished.contains_key("a"),
            "retired without reading its newest entries after it shut down"
        );
        assert_eq!(log_tokens(&fake, "a").len(), 2 * MAX_PAGES);

        // The next round reads it from the top, and only then retires it.
        fake.servers(ended(), None);
        fake.logs(
            "a",
            vec![log("a", NOW - MINUTE / 2, LogSeverity::Error, "late")],
            None,
        );
        let third = reader.round(NOW + 2 * MINUTE);
        assert_eq!(messages(&third), ["late"]);
        assert_eq!(log_tokens(&fake, "a").last(), Some(&None));
        assert!(reader.finished.contains_key("a"));
    }

    #[test]
    fn path_arguments_are_checked_before_any_request() {
        for args in [
            ["--job-id", "../other"],
            ["--job-id", "a/b"],
            ["--place-version", "45?x=1"],
            ["--place-version", ".."],
        ] {
            let error = parse(&args).validate().unwrap_err().to_string();
            assert!(error.contains(args[0]), "{error}");
        }
        assert!(
            parse(&["--job-id", "0f8e-11aa_b.c", "--place-version", "45"])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn a_request_that_could_not_be_sent_is_fatal_not_retried() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45"]);
        let mut reader = reader(&fake, &command);
        fake.servers(
            vec![server("bad", false, None), server("b", false, None)],
            None,
        );
        fake.logs
            .borrow_mut()
            .entry("bad".to_owned())
            .or_default()
            .push_back(Err(OpenCloudError::NotSent(
                "job id \"bad\" cannot be used in a request path".into(),
            )));
        let batch = reader.round(NOW);
        assert!(matches!(batch.error, Some(OpenCloudError::NotSent(_))));
        assert!(batch.failed_servers.is_empty());
        assert!(log_tokens(&fake, "b").is_empty());
        let error = batch.error.unwrap();
        assert!(is_usage_problem(&error));
        assert!(command.explain(error).is::<UsageError>());
    }

    #[test]
    fn the_api_key_is_left_out_of_everything_printed() {
        const KEY: &str = "SECRETKEY";
        let redact = |text: &str| text.replace(KEY, "[redacted]");
        let fake = Fake::default();
        let command = parse(&["--place-version", "45", "--json"]);
        let mut reader = reader(&fake, &command);
        let job = format!("job-{KEY}");
        fake.servers(vec![server(&job, false, None)], None);
        fake.logs
            .borrow_mut()
            .entry(job.clone())
            .or_default()
            .push_back(Err(OpenCloudError::Unconfirmed(format!("echoed {KEY}"))));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let error = command
            .read_once(&mut reader, &mut out, &mut err, &redact, NOW)
            .map_err(|error| redact_error(error, &redact))
            .unwrap_err();
        let (out, err) = (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        );
        let error = format!("{error:#}");
        for text in [&out, &err, &error] {
            assert!(!text.contains(KEY), "{text}");
        }
        assert!(
            err.contains("could not read server job-[redacted]"),
            "{err}"
        );
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["failedServers"], json!(["job-[redacted]"]));
        assert!(error.contains("job-[redacted]"), "{error}");

        // A usage error stays one once redacted, so the exit status holds.
        let usage: anyhow::Error = UsageError(anyhow::anyhow!("bad {KEY}")).into();
        let redacted = redact_error(usage, &redact);
        assert!(redacted.is::<UsageError>());
        assert!(format!("{redacted:#}").contains("bad [redacted]"));
    }

    /// An entry with the key in every text field it has.
    fn leaky_entry(job: &str, ms: u64, key: &str) -> GameServerLog {
        let mut entry = log(job, ms, LogSeverity::Error, &format!("message {key}"));
        entry.stack_trace = format!("Script line 1 {key}");
        entry.message_template = Some(format!("template {key}"));
        entry.context = Some(format!("context {key}"));
        entry.logger_full_path = Some(format!("logger {key}"));
        entry.structured_stack_trace = json!([{ "source": format!("frame {key}") }]);
        entry
    }

    #[test]
    fn the_api_key_is_left_out_of_entries_printed_once() {
        const KEY: &str = "SECRETKEY";
        let redact = |text: &str| text.replace(KEY, "[redacted]");
        for json in [false, true] {
            let fake = Fake::default();
            let mut args = vec!["--place-version", "45"];
            if json {
                args.push("--json");
            }
            let command = parse(&args);
            let mut reader = reader(&fake, &command);
            fake.servers(vec![server("a", false, None)], None);
            fake.logs("a", vec![leaky_entry("a", NOW - MINUTE, KEY)], None);
            let (mut out, mut err) = (Vec::new(), Vec::new());
            command
                .read_once(&mut reader, &mut out, &mut err, &redact, NOW)
                .unwrap();
            let out = String::from_utf8(out).unwrap();
            assert!(!out.contains(KEY), "{out}");
            assert!(out.contains("message [redacted]"), "{out}");
            if json {
                assert!(out.contains("frame [redacted]"), "{out}");
                assert!(out.contains("context [redacted]"), "{out}");
            } else {
                assert!(out.contains("Script line 1 [redacted]"), "{out}");
            }
        }
    }

    /// Takes whole lines, then reports a closed pipe, which ends following.
    struct Lines {
        written: Vec<u8>,
        lines: usize,
    }

    impl std::io::Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.written.iter().filter(|&&b| b == b'\n').count() >= self.lines {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_api_key_is_left_out_of_entries_followed() {
        const KEY: &str = "SECRETKEY";
        let redact = |text: &str| text.replace(KEY, "[redacted]");
        let now = now_ms();
        for json in [false, true] {
            let fake = Fake::default();
            let mut args = vec!["--place-version", "45", "--follow"];
            if json {
                args.push("--json");
            }
            let command = parse(&args);
            let mut reader = reader(&fake, &command);
            fake.servers(vec![server("a", false, None)], None);
            fake.logs(
                "a",
                vec![
                    leaky_entry("a", now - MINUTE, KEY),
                    leaky_entry("a", now - 2 * MINUTE, KEY),
                ],
                None,
            );
            let mut out = Lines {
                written: Vec::new(),
                // The first entry whole: its message, and in text its stack trace.
                lines: if json { 1 } else { 2 },
            };
            command.follow(&mut reader, &mut out, &redact).unwrap();
            let out = String::from_utf8(out.written).unwrap();
            assert!(!out.contains(KEY), "{out}");
            assert!(out.contains("message [redacted]"), "{out}");
        }
    }

    #[test]
    fn a_refused_page_token_reads_the_server_again_from_the_top() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45", "--job-id", "a"]);
        let mut reader = reader(&fake, &command);
        queue_pages(&fake, "a", MAX_PAGES as u64 + 1, NOW - MINUTE);
        reader.round(NOW);
        // The saved token has expired; the next read from the top finds
        // what was shown and the entry that was left.
        let mut drained = fake.logs.borrow_mut();
        let queue = drained.get_mut("a").unwrap();
        let left = queue.pop_front().unwrap().unwrap();
        queue.push_back(Err(status_error(400)));
        queue.push_back(Ok(GameServerLogPage {
            logs: [
                vec![log("a", NOW - MINUTE, LogSeverity::Error, "e0")],
                left.logs,
            ]
            .concat(),
            next_page_token: None,
        }));
        drop(drained);
        let second = reader.round(NOW + MINUTE);
        assert!(second.error.is_none());
        assert_eq!(messages(&second), [format!("e{MAX_PAGES}")]);
        assert!(second.notices[0].contains("Roblox refused to continue"));
    }

    #[test]
    fn a_listed_server_that_is_gone_is_forgotten_quietly() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45"]);
        let mut reader = reader(&fake, &command);
        let both = || vec![server("gone", false, None), server("b", false, None)];
        fake.servers(both(), None);
        fake.status_logs("gone", 404);
        fake.logs(
            "b",
            vec![log("b", NOW - MINUTE, LogSeverity::Error, "b")],
            None,
        );
        let batch = reader.round(NOW);
        assert!(batch.error.is_none());
        assert!(batch.failed_servers.is_empty());
        assert!(batch.notices.is_empty(), "{:?}", batch.notices);
        assert_eq!(messages(&batch), ["b"]);

        fake.servers(both(), None);
        reader.round(NOW + MINUTE);
        assert_eq!(log_tokens(&fake, "gone").len(), 1);
    }

    #[test]
    fn auth_errors_and_unknown_named_jobs_stop_the_round() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45", "--job-id", "a"]);
        let mut reader = reader(&fake, &command);
        fake.status_logs("a", 404);
        let batch = reader.round(NOW);
        assert_eq!(status_of(batch.error.as_ref().unwrap()), Some(404));
        assert!(command.explain(batch.error.unwrap()).is::<UsageError>());

        let fake = Fake::default();
        let command = parse(&["--place-version", "45"]);
        let mut reader = Reader::new(&fake, &command);
        fake.servers(
            vec![server("a", false, None), server("b", false, None)],
            None,
        );
        fake.status_logs("a", 403);
        let batch = reader.round(NOW);
        assert_eq!(status_of(batch.error.as_ref().unwrap()), Some(403));
        assert!(log_tokens(&fake, "b").is_empty());
    }

    #[test]
    fn servers_are_only_forgotten_after_a_listing_that_was_not_capped() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45"]);
        let mut reader = reader(&fake, &command);
        let listed = || {
            vec![
                server("a", false, None),
                server("fin", true, Some("2026-11-07T11:30:00Z")),
            ]
        };
        let old = || vec![log("a", NOW - MINUTE, LogSeverity::Error, "old")];
        fake.servers(listed(), None);
        fake.logs("a", old(), None);
        assert_eq!(messages(&reader.round(NOW)), ["old"]);
        assert!(reader.finished.contains_key("fin"));

        // Listings that stop at Rojo's own page cap, before reaching either.
        for minute in 1..=7 {
            for _ in 0..MAX_PAGES {
                fake.servers(Vec::new(), Some("more"));
            }
            let batch = reader.round(NOW + minute * MINUTE);
            assert!(batch.listing_capped && !batch.incomplete);
        }
        assert!(reader.servers.contains_key("a"));
        assert!(reader.finished.contains_key("fin"));

        fake.servers(listed(), None);
        fake.logs("a", old(), None);
        let batch = reader.round(NOW + 8 * MINUTE);
        assert!(batch.entries.is_empty(), "{:?}", messages(&batch));
        assert!(log_tokens(&fake, "fin").len() == 1);
    }

    #[test]
    fn a_failing_server_keeps_the_entries_of_servers_read_before_it() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45"]);
        let mut reader = reader(&fake, &command);
        let both = || vec![server("a", false, None), server("b", false, None)];

        fake.servers(both(), None);
        fake.logs(
            "a",
            vec![log("a", NOW - MINUTE, LogSeverity::Error, "from a")],
            None,
        );
        fake.failing_logs("b");
        let first = reader.round(NOW);
        assert_eq!(messages(&first), ["from a"]);
        assert!(first.error.is_none());
        assert_eq!(first.failed_servers, ["b"]);
        assert!(first.notices[0].starts_with("Warning: could not read server b"));
        assert_eq!(first.servers_read, 1);

        // Next round, a's entry is not shown again and b's are shown, since
        // b's failed read changed nothing.
        fake.servers(both(), None);
        fake.logs(
            "a",
            vec![log("a", NOW - MINUTE, LogSeverity::Error, "from a")],
            None,
        );
        fake.logs(
            "b",
            vec![log("b", NOW - MINUTE, LogSeverity::Error, "from b")],
            None,
        );
        let second = reader.round(NOW + MINUTE);
        assert!(second.error.is_none());
        assert_eq!(messages(&second), ["from b"]);
    }

    #[test]
    fn identical_entries_in_one_read_are_all_shown_once() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45", "--job-id", "a"]);
        let mut reader = reader(&fake, &command);
        let twice = || {
            vec![
                log("a", NOW - MINUTE, LogSeverity::Error, "same"),
                log("a", NOW - MINUTE, LogSeverity::Error, "same"),
            ]
        };

        fake.logs("a", twice(), None);
        assert_eq!(messages(&reader.round(NOW)), ["same", "same"]);
        // The same page read again shows nothing.
        fake.logs("a", twice(), None);
        assert!(reader.round(NOW + MINUTE).entries.is_empty());
        // A third occurrence is new.
        let mut three = twice();
        three.push(log("a", NOW - MINUTE, LogSeverity::Error, "same"));
        fake.logs("a", three, None);
        assert_eq!(messages(&reader.round(NOW + 2 * MINUTE)), ["same"]);

        // Entries that differ only in a structured logging field are
        // different entries.
        let mut with_context = log("a", NOW, LogSeverity::Error, "same");
        with_context.context = Some("{\"id\":1}".into());
        let mut other_logger = log("a", NOW, LogSeverity::Error, "same");
        other_logger.logger_full_path = Some("Game.Logger".into());
        fake.logs(
            "a",
            vec![
                log("a", NOW, LogSeverity::Error, "same"),
                with_context,
                other_logger,
            ],
            None,
        );
        assert_eq!(reader.round(NOW + 3 * MINUTE).entries.len(), 3);
    }

    #[test]
    fn following_state_stays_bounded() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45", "--since", "10m"]);
        let mut reader = reader(&fake, &command);

        for round in 0..60u64 {
            let now = NOW + round * MINUTE;
            let mut servers = vec![server("busy", false, None)];
            // "quiet" logs once, then stays listed but silent, then leaves.
            if round < 20 {
                servers.push(server("quiet", false, None));
            }
            // "ended" shut down at the first round and is listed until 40.
            if round < 40 {
                servers.push(server("ended", true, Some("2026-11-07T12:00:00Z")));
            }
            fake.servers(servers, None);
            fake.logs(
                "busy",
                vec![log("busy", now - 1000, LogSeverity::Error, "tick")],
                None,
            );
            if round == 0 {
                fake.logs(
                    "quiet",
                    vec![log("quiet", now, LogSeverity::Error, "once")],
                    None,
                );
            }
            let batch = reader.round(now);
            assert!(batch.error.is_none());
            assert_eq!(
                batch.entries.len(),
                if round == 0 { 2 } else { 1 },
                "round {round}"
            );

            // Only entries inside the overlap window are remembered.
            let busy = &reader.servers["busy"];
            assert!(busy.seen.len() <= 6, "round {round}: {}", busy.seen.len());
            if let Some(quiet) = reader.servers.get("quiet") {
                assert!(quiet.seen.len() <= 1);
            }
            // "ended" is skipped once finished, for as long as it is listed.
            let skipped = (6..40).contains(&round);
            assert_eq!(
                reader.finished.contains_key("ended"),
                skipped,
                "round {round}"
            );
        }
        // The quiet and ended servers left the listing long ago.
        assert_eq!(reader.servers.keys().collect::<Vec<_>>(), ["busy"]);
        assert!(reader.finished.is_empty());
    }

    #[test]
    fn following_shows_entries_that_become_readable_after_since_passed() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45", "--job-id", "a", "--since", "1m"]);
        let mut reader = reader(&fake, &command);
        assert!(reader.round(NOW).entries.is_empty());
        // Logged 30 seconds in, readable three minutes later: by then
        // `now - 1m` is past it.
        fake.logs(
            "a",
            vec![log("a", NOW + 30_000, LogSeverity::Error, "late")],
            None,
        );
        assert_eq!(messages(&reader.round(NOW + 3 * MINUTE + 30_000)), ["late"]);
        // Later ones are read back from the newest entry, not from --since.
        fake.logs(
            "a",
            vec![
                log("a", NOW + 2 * MINUTE, LogSeverity::Error, "later"),
                log("a", NOW + 30_000, LogSeverity::Error, "late"),
            ],
            None,
        );
        assert_eq!(messages(&reader.round(NOW + 6 * MINUTE)), ["later"]);
    }

    #[test]
    fn named_jobs_are_remembered_through_failures() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45", "--job-id", "a"]);
        let mut reader = reader(&fake, &command);
        let old = || vec![log("a", NOW - MINUTE, LogSeverity::Error, "old")];
        fake.logs("a", old(), None);
        assert_eq!(messages(&reader.round(NOW)), ["old"]);
        // Ten minutes of failed reads, then the same entry again.
        for minute in 1..=10 {
            fake.failing_logs("a");
            assert_eq!(reader.round(NOW + minute * MINUTE).failed_servers, ["a"]);
        }
        fake.logs("a", old(), None);
        let batch = reader.round(NOW + 11 * MINUTE);
        assert!(batch.error.is_none());
        assert!(batch.entries.is_empty(), "{:?}", messages(&batch));
    }

    #[test]
    fn named_jobs_are_capped_too() {
        let fake = Fake::default();
        let command = parse(&[
            "--place-version",
            "45",
            "--job-id",
            "a",
            "--job-id",
            "b",
            "--job-id",
            "c",
            "--max-servers",
            "2",
        ]);
        let mut reader = reader(&fake, &command);
        let batch = reader.round(NOW);
        assert!(batch.truncated);
        assert_eq!(batch.servers_read, 2);
        assert!(
            batch.notices[0].contains("3 servers to read"),
            "{:?}",
            batch.notices
        );
    }

    #[test]
    fn partial_listings_are_reported_and_do_not_forget_servers() {
        let fake = Fake::default();
        let command = parse(&["--place-version", "45"]);
        let mut reader = reader(&fake, &command);
        let old = || vec![log("a", NOW - MINUTE, LogSeverity::Error, "old")];
        fake.servers(vec![server("a", false, None)], None);
        fake.logs("a", old(), None);
        assert_eq!(messages(&reader.round(NOW)), ["old"]);

        // Ten minutes of listings that Roblox says are partial and lack a.
        for minute in 1..=10 {
            fake.partial_servers(Vec::new());
            let batch = reader.round(NOW + minute * MINUTE);
            assert!(batch.incomplete);
            assert!(
                batch.notices[0].starts_with("Warning:"),
                "{:?}",
                batch.notices
            );
        }
        assert!(reader.servers.contains_key("a"));
        fake.servers(vec![server("a", false, None)], None);
        fake.logs("a", old(), None);
        let batch = reader.round(NOW + 11 * MINUTE);
        assert!(!batch.incomplete);
        assert!(batch.entries.is_empty(), "{:?}", messages(&batch));
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

        // Half of the owner's 100 a minute, leaving room for another run.
        assert_eq!(command.requests_per_minute, 50);
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
        let batch = reader.round(NOW);

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
        let first = reader.round(NOW);
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
        let second = reader.round(NOW + 2 * MINUTE);
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
        let batch = reader.round(NOW);
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
        let batch = reader.round(NOW);
        assert!(batch.truncated);
        assert_eq!(batch.servers_read, 1);
        // Active servers come first.
        assert!(matches!(&fake.calls.borrow()[1], Call::Logs(_, job, _, _) if job == "live"));
        assert!(
            batch.notices[0].contains("2 servers to read"),
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
        let batch = reader.round(NOW);
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
        let jobs: Vec<_> = select_servers(servers, since, &[], &HashMap::new())
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
