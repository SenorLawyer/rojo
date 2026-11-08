//! A small blocking client for the Roblox Open Cloud APIs that Rojo uses:
//! place publishing, Luau Execution, asset uploads and game server logs.
//!
//! # Safety properties
//!
//! - The API key is only ever sent in the `x-api-key` header, which is marked
//!   sensitive so reqwest leaves it out of its debug output. It never appears
//!   in [`OpenCloud`]'s `Debug` output, and any error text that would contain
//!   it (for example a server echoing it back) has it replaced by `[redacted]`.
//! - Redirects are disabled, so the key is never forwarded to another host.
//! - Every request has a timeout. Polling loops have a bounded total wait.
//! - HTTP 429 is retried with bounded exponential backoff that honours
//!   `Retry-After`. Server errors (5xx) and failures after a request was sent
//!   are only retried for `GET` requests: a `POST` that failed that way may
//!   already have created a place version, task or asset, and repeating it
//!   could create a duplicate. [`OpenCloudError::may_have_applied`] tells
//!   callers whether a failed request could still have taken effect.
//! - Paths returned by Roblox (task and operation paths) are checked before
//!   they are requested, and are always appended to the configured base URL.
//!
//! # Endpoints (checked against create.roblox.com/docs/cloud in November 2026)
//!
//! - Place publishing:
//!   `POST /universes/v1/{universe}/places/{place}/versions?versionType=Saved|Published`
//!   with the raw place as the body (`application/octet-stream` for `.rbxl`,
//!   `application/xml` for `.rbxlx`). The response is `{"versionNumber": n}`.
//!   Scope: `universe-places:write`. While the place is open in a Team
//!   Create session, Roblox rejects saves with HTTP 409 and "Server is busy
//!   and unable to process your upload request" (seen live in November 2026).
//! - Luau Execution: `POST /cloud/v2/universes/{u}/places/{p}[/versions/{v}]/luau-execution-session-tasks`
//!   with `{"script": ..., "timeout": "<n>s"}` (`timeout` is a protobuf
//!   duration string, default and maximum five minutes). The response is a
//!   `LuauExecutionSessionTask` whose `path` is
//!   `universes/{u}/places/{p}/versions/{v}/luau-execution-sessions/{s}/tasks/{t}`,
//!   with `state` one of `QUEUED`, `PROCESSING`, `COMPLETE`, `FAILED` or
//!   `CANCELLED`, `output.results` (the script's return values; values that
//!   are not JSON serializable become `null`) and `error.code` /
//!   `error.message` (codes such as `SCRIPT_ERROR` and `DEADLINE_EXCEEDED`).
//!   It is polled with `GET /cloud/v2/{path}`. Logs come from
//!   `GET /cloud/v2/{path}/logs?view=STRUCTURED&maxPageSize=..&pageToken=..`,
//!   which returns `luauExecutionSessionTaskLogs[].structuredMessages[]`
//!   (`message`, `createTime`, `messageType`) and `nextPageToken` (omitted on
//!   the last page). Roblox keeps at most about 450 KB of logs per task.
//!   Differences from older descriptions: the get and logs paths always
//!   include a version and session segment, so the returned `path` must be
//!   used as is rather than rebuilt; a place allows at most ten unfinished
//!   tasks (more return 429); creation is limited to five per minute per key
//!   owner and logs to 45 requests per minute. Scopes:
//!   `universe.place.luau-execution-session:write` (and `:read` for polling).
//!   Without a version, the task runs on the place's current published
//!   version and the returned `path` names it.
//! - Assets: `POST /assets/v1/assets` as `multipart/form-data` with a
//!   `request` part (JSON: `assetType`, `displayName`, `description`,
//!   `creationContext.creator.userId|groupId`) and a `fileContent` part. The
//!   response is an operation `{"path": "operations/{id}", "done": ..}`,
//!   polled with `GET /assets/v1/operations/{id}` until `done` is true, when
//!   `response.assetId` (a decimal string), `response.assetType` (such as
//!   `Image`), `response.moderationResult.moderationState` (such as
//!   `Approved`) or `error` is set. Scopes:
//!   `asset:read` and `asset:write`. Files may be up to 20 MB. Once the
//!   `POST` has returned an operation, the asset may exist even if polling
//!   fails (for example with 403 when the key lacks `asset:read`), so such
//!   errors are [`OpenCloudError::Unfollowed`] and the operation can be
//!   polled again later with [`OpenCloud::resume_asset_upload`].
//! - Game server logs (Observability, beta), all under
//!   `/server-management/v1/universes/{u}/places/{p}` with scope
//!   `universe:read` and a limit of 100 requests per minute per key owner
//!   (shared by every use of the owner's keys):
//!   `GET .../game-servers:filter-options?Filter=PlaceVersion` gives
//!   `filters.PlaceVersion.values`; `GET .../versions/{v}/game-servers`
//!   gives `gameServers[]` (`jobId`, `placeVersion`, `shutDown`,
//!   `terminationTime`, ...) and `nextPageToken`; and
//!   `GET .../versions/{v}/game-servers/{jobId}/logs` gives
//!   `gameServerLogs[]` (`messageTimestampMs` as a string, `severity` as
//!   `LogSeverity` 0-3 for Output, Info, Warning and Error, `message`,
//!   `stackTrace`, `skippedCount`, `rateLimitedCount`, ...) and
//!   `nextPageToken`. List queries take `MaxPageSize` (at most 100),
//!   `PageToken`, `OrderBy` (`message_timestamp desc` for logs) and a CEL
//!   `Filter` (for logs: `severity`, `message_timestamp` and `search ==
//!   "text"`). Roblox only keeps Error and Warning logs so far, which take
//!   about three minutes to appear, and folds repeats past 10 identical or
//!   500 total messages per minute per server into those counts. Documented
//!   at create.roblox.com/docs/cloud/reference/features/observability; not
//!   checked against the live API.
//!
//! # Image or Decal
//!
//! The asset docs list "Decal, Image" together and their examples only use
//! `Decal`, but an asset uploaded as `Decal` gets a Decal id, which does not
//! load when used as `rbxassetid://` in `ImageLabel.Image` or
//! `Texture.Texture`: those need the id of the Image behind the decal.
//! Uploading with `assetType` `Image` is accepted and returns the Image id
//! itself (the operation reports `assetType` `Image`). Checked live in
//! November 2026: `ContentProvider:PreloadAsync` on an `ImageLabel` whose
//! `Image` is `rbxassetid://` plus that id reports `Success`. So
//! [`AssetType::Image`] is the right choice for UI and texture content.
//! [`AssetType::Decal`] is only for content meant to be inserted as a Decal.

use std::{
    fmt,
    sync::Mutex,
    thread::sleep,
    time::{Duration, Instant},
};

use reqwest::{
    blocking::{Client, RequestBuilder},
    header::HeaderValue,
    Method, StatusCode,
};
use serde::Deserialize;
use serde_json::Value;

/// The environment variable that holds the API key unless told otherwise.
pub const DEFAULT_API_KEY_VAR: &str = "ROBLOX_API_KEY";

const DEFAULT_BASE_URL: &str = "https://apis.roblox.com";

/// Logs past this size are dropped. Roblox keeps about 450 KB per task, so
/// this only guards against a misbehaving server.
const MAX_LOG_BYTES: usize = 1024 * 1024;

/// Bounds the number of log pages fetched for one task.
const MAX_LOG_PAGES: usize = 100;

/// How long an asset upload may take to finish processing after it was sent.
const ASSET_OPERATION_TIMEOUT: Duration = Duration::from_secs(300);

/// A Luau task can wait in a queue and for a server to start before its own
/// timeout begins, so polling allows this much on top of that timeout.
const LUAU_POLL_MARGIN: Duration = Duration::from_secs(120);

/// Timeout for small JSON requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How requests are retried and how often long-running work is polled.
#[derive(Debug, Clone)]
struct Timing {
    max_attempts: u32,
    retry_base: Duration,
    retry_max: Duration,
    poll_initial: Duration,
    poll_max: Duration,
    luau_margin: Duration,
    asset_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            retry_base: Duration::from_secs(1),
            // Long enough for Luau Execution's five-per-minute creation limit.
            retry_max: Duration::from_secs(60),
            poll_initial: Duration::from_secs(1),
            poll_max: Duration::from_secs(5),
            luau_margin: LUAU_POLL_MARGIN,
            asset_timeout: ASSET_OPERATION_TIMEOUT,
        }
    }
}

/// The format of a place file, which decides the upload's content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The full set of values Open Cloud accepts or returns; not every command
// uses each one yet.
#[allow(dead_code)]
pub enum PlaceFormat {
    /// `.rbxl`
    Binary,
    /// `.rbxlx`
    Xml,
}

impl PlaceFormat {
    fn content_type(self) -> &'static str {
        match self {
            PlaceFormat::Binary => "application/octet-stream",
            PlaceFormat::Xml => "application/xml",
        }
    }
}

/// Whether a new place version only gets saved or also goes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The full set of values Open Cloud accepts or returns; not every command
// uses each one yet.
#[allow(dead_code)]
pub enum VersionType {
    /// Saved as a version without changing what players join.
    Saved,
    /// Saved and made the version that new servers run.
    Published,
}

impl VersionType {
    fn as_str(self) -> &'static str {
        match self {
            VersionType::Saved => "Saved",
            VersionType::Published => "Published",
        }
    }
}

/// Asset types that the Open Cloud Assets API accepts for new uploads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The full set of values Open Cloud accepts or returns; not every command
// uses each one yet.
#[allow(dead_code)]
pub enum AssetType {
    /// PNG, JPEG, BMP or TGA. Its id works directly in `rbxassetid://` for
    /// image and texture properties.
    Image,
    /// The same formats as [`AssetType::Image`], wrapped in a Decal. Its id
    /// does not work in image properties.
    Decal,
    /// MP3, OGG, WAV or FLAC.
    Audio,
    /// FBX, glTF, RBXM or RBXMX, uploaded as a package.
    Model,
    /// RBXM or RBXMX animations.
    Animation,
    /// MP4 or MOV.
    Video,
}

impl AssetType {
    fn as_str(self) -> &'static str {
        match self {
            AssetType::Image => "Image",
            AssetType::Decal => "Decal",
            AssetType::Audio => "Audio",
            AssetType::Model => "Model",
            AssetType::Animation => "Animation",
            AssetType::Video => "Video",
        }
    }
}

/// Who owns an uploaded asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Creator {
    User(u64),
    Group(u64),
}

/// The final state of a Luau Execution task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Complete,
    Failed,
    Cancelled,
}

/// One message a Luau task logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogMessage {
    pub message: String,
    /// `OUTPUT`, `INFO`, `WARNING` or `ERROR`; empty when Roblox gave none.
    pub kind: String,
}

/// What a finished Luau Execution task returned. A script error is not an
/// `Err`: it is reported here with [`TaskState::Failed`] and `error_code`.
#[derive(Debug, Clone)]
pub struct LuauTaskResult {
    /// The task's resource path, for referring back to it.
    pub path: String,
    pub state: TaskState,
    /// The script's return values. Empty unless the task completed.
    pub results: Vec<Value>,
    /// Such as `SCRIPT_ERROR` or `DEADLINE_EXCEEDED`.
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub logs: Vec<LogMessage>,
    /// Set when logs were left out because they exceeded the size cap or
    /// could not all be fetched.
    pub logs_truncated: bool,
    /// Why the logs could not all be fetched. The task's result is still
    /// complete; only `logs` is partial.
    pub logs_error: Option<String>,
}

/// A finished asset upload.
#[derive(Debug, Clone)]
// The full set of values Open Cloud accepts or returns; not every command
// uses each one yet.
#[allow(dead_code)]
pub struct UploadedAsset {
    pub asset_id: u64,
    /// As Roblox reports it, such as `Image`.
    pub asset_type: Option<String>,
    /// As Roblox reports it, such as `Approved`.
    pub moderation_state: Option<String>,
}

/// An asset upload that Roblox accepted and is still processing.
#[derive(Debug)]
pub struct StartedUpload {
    /// The operation's path, such as `operations/abc`, for
    /// [`OpenCloud::resume_asset_upload`].
    pub operation: String,
    state: Operation,
}

/// Why an Open Cloud call failed. None of these contain the API key.
#[derive(Debug, thiserror::Error)]
pub enum OpenCloudError {
    /// The request never reached Roblox, so nothing changed.
    #[error("Request to Open Cloud was not sent: {0}")]
    NotSent(String),
    /// Roblox answered with an error status.
    #[error("Open Cloud returned HTTP {status} for {request}{body}")]
    Status {
        status: StatusCode,
        /// The method and path, such as `POST /assets/v1/assets`.
        request: String,
        /// A truncated, single-line copy of the response body, prefixed with
        /// `": "` when not empty.
        body: String,
    },
    /// The request was sent but no usable answer came back: the connection
    /// failed, the response could not be read or was not what Open Cloud
    /// documents.
    #[error("Open Cloud gave no usable answer: {0}")]
    Unconfirmed(String),
    /// Roblox did not finish the work within the allowed time.
    #[error("Timed out waiting for Open Cloud: {0}")]
    TimedOut(String),
    /// Roblox finished the work and reported that it failed.
    #[error("Open Cloud reported a failure: {0}")]
    Failed(String),
    /// Roblox accepted the work as `path` (an asset operation or a Luau
    /// task), but a later request to follow it failed. The work exists and
    /// may still finish, whatever kind of error `error` is.
    #[error("Open Cloud accepted the request as {path}, but following it failed: {error}")]
    Unfollowed {
        path: String,
        error: Box<OpenCloudError>,
    },
}

impl OpenCloudError {
    /// Whether the request may have taken effect despite the error, such as a
    /// place version or asset having been created. Callers should not blindly
    /// repeat such requests.
    pub fn may_have_applied(&self) -> bool {
        match self {
            OpenCloudError::NotSent(_) | OpenCloudError::Failed(_) => false,
            // 408 (request timeout), 409 (conflict) and 499 (client closed
            // request) can be reported after the server already acted.
            OpenCloudError::Status { status, .. } => {
                !status.is_client_error() || matches!(status.as_u16(), 408 | 409 | 499)
            }
            OpenCloudError::Unconfirmed(_)
            | OpenCloudError::TimedOut(_)
            | OpenCloudError::Unfollowed { .. } => true,
        }
    }

    /// Marks an error from following `path` after Roblox accepted the work.
    /// A poll that was refused (say, a key without the read scope) or never
    /// sent says nothing about the work itself, which already exists.
    fn following(path: &str, error: OpenCloudError) -> OpenCloudError {
        match error {
            // Already says the work may have applied, and callers match on it.
            OpenCloudError::TimedOut(_) | OpenCloudError::Unfollowed { .. } => error,
            error => OpenCloudError::Unfollowed {
                path: path.to_owned(),
                error: Box::new(error),
            },
        }
    }
}

type Result<T> = std::result::Result<T, OpenCloudError>;

/// A configured Open Cloud client. Cheap to share by reference.
pub struct OpenCloud {
    client: Client,
    key: HeaderValue,
    base_url: String,
    timing: Timing,
    spacing: Option<Spacing>,
}

/// The minimum time between two HTTP attempts and when the next may start.
struct Spacing {
    spacing: Duration,
    next: Mutex<Option<Instant>>,
}

impl fmt::Debug for OpenCloud {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenCloud")
            .field("base_url", &self.base_url)
            .field("key", &"[redacted]")
            .finish()
    }
}

impl OpenCloud {
    /// Creates a client using the API key in the environment variable `var`
    /// (normally [`DEFAULT_API_KEY_VAR`]).
    pub fn from_env(var: &str) -> anyhow::Result<OpenCloud> {
        let key = match std::env::var(var) {
            Ok(key) => key,
            Err(std::env::VarError::NotPresent) => {
                anyhow::bail!("Set the {var} environment variable to a Roblox Open Cloud API key")
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                anyhow::bail!("The {var} environment variable is not valid Unicode")
            }
        };
        Self::with_base_url(key.trim(), DEFAULT_BASE_URL)
            .map_err(|error| anyhow::anyhow!("The {var} environment variable {error}"))
    }

    /// Creates a client that sends requests to `base_url` instead of
    /// `https://apis.roblox.com`, for tests. Errors never contain the key.
    pub fn with_base_url(key: &str, base_url: &str) -> anyhow::Result<OpenCloud> {
        anyhow::ensure!(!key.is_empty(), "is empty");
        let mut key = HeaderValue::from_str(key)
            .map_err(|_| anyhow::anyhow!("is not a valid HTTP header value"))?;
        key.set_sensitive(true);
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                anyhow::anyhow!("could not be used: cannot create HTTP client: {error}")
            })?;
        Ok(OpenCloud {
            client,
            key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            timing: Timing::default(),
            spacing: None,
        })
    }

    /// Spaces every HTTP attempt this client sends, retries included, at
    /// least `spacing` apart, for endpoints whose per-minute limit a caller
    /// must stay under.
    pub fn with_request_spacing(mut self, spacing: Duration) -> OpenCloud {
        self.spacing = Some(Spacing {
            spacing,
            next: Mutex::new(None),
        });
        self
    }

    /// Waits for the next request slot when requests are spaced.
    fn pace(&self) {
        let Some(spacing) = &self.spacing else {
            return;
        };
        loop {
            let wait = {
                let mut next = spacing.next.lock().unwrap_or_else(|e| e.into_inner());
                admit(&mut next, Instant::now(), spacing.spacing)
            };
            match wait {
                Some(wait) => sleep(wait),
                None => return,
            }
        }
    }

    /// Retries and polls within milliseconds and gives Luau tasks no extra
    /// time past their timeout, so tests of callers against a fake server
    /// finish quickly.
    #[cfg(test)]
    pub(crate) fn with_fast_timing(mut self) -> OpenCloud {
        self.timing = Timing {
            max_attempts: 3,
            retry_base: Duration::from_millis(10),
            retry_max: Duration::from_millis(50),
            poll_initial: Duration::from_millis(10),
            poll_max: Duration::from_millis(20),
            luau_margin: Duration::ZERO,
            asset_timeout: Duration::from_millis(300),
        };
        self
    }

    /// Uploads a place file as a new version and returns its version number.
    pub fn publish_place(
        &self,
        universe_id: u64,
        place_id: u64,
        bytes: &[u8],
        format: PlaceFormat,
        version_type: VersionType,
    ) -> Result<u64> {
        let path = format!(
            "/universes/v1/{universe_id}/places/{place_id}/versions?versionType={}",
            version_type.as_str()
        );
        let body = self.send(
            Method::POST,
            &path,
            Some((bytes, format.content_type())),
            upload_timeout(bytes.len()),
        )?;
        let receipt: Value = self.parse(&body, "a publication receipt")?;
        receipt["versionNumber"]
            .as_u64()
            .filter(|version| *version > 0)
            .ok_or_else(|| {
                self.unconfirmed(format!(
                    "response has no valid versionNumber{}",
                    self.excerpt(&body)
                ))
            })
    }

    /// Runs `script` in a server for the place (at `version`, or the latest
    /// one when `None`), waits for it to finish and fetches its logs.
    ///
    /// `timeout` is how long the script may run; it is rounded up to whole
    /// seconds. Polling stops after `timeout` plus a margin for queueing.
    pub fn run_luau(
        &self,
        universe_id: u64,
        place_id: u64,
        version: Option<u64>,
        script: &str,
        timeout: Duration,
    ) -> Result<LuauTaskResult> {
        let version = version.map_or(String::new(), |v| format!("/versions/{v}"));
        let path = format!(
            "/cloud/v2/universes/{universe_id}/places/{place_id}{version}/luau-execution-session-tasks"
        );
        let seconds = timeout.as_secs() + u64::from(timeout.subsec_nanos() > 0);
        let request = serde_json::json!({
            "script": script,
            "timeout": format!("{}s", seconds.max(1)),
        });
        let request = serde_json::to_vec(&request).expect("JSON values always serialize");
        let body = self.send(
            Method::POST,
            &path,
            Some((&request, "application/json")),
            // Scripts may be up to 4 MB.
            upload_timeout(request.len()),
        )?;
        let mut task: TaskResource = self.parse(&body, "a Luau execution task")?;
        let task_path = self
            .checked_path(&task.path, "universes/")
            .map_err(|error| self.unconfirmed(error))?
            .to_owned();

        let deadline = Instant::now() + timeout + self.timing.luau_margin;
        let mut delay = self.timing.poll_initial;
        let state = loop {
            match task.state.as_str() {
                "COMPLETE" => break TaskState::Complete,
                "FAILED" => break TaskState::Failed,
                "CANCELLED" => break TaskState::Cancelled,
                // QUEUED, PROCESSING and any state added later mean "not yet".
                _ => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(OpenCloudError::TimedOut(format!(
                    "Luau task {task_path} was still {} after {} seconds",
                    if task.state.is_empty() {
                        "unfinished"
                    } else {
                        &task.state
                    },
                    (timeout + self.timing.luau_margin).as_secs()
                )));
            }
            sleep(delay.min(deadline - now));
            delay = (delay * 3 / 2).min(self.timing.poll_max);
            // The task exists from here on, so a failed poll must not read as
            // "nothing happened".
            task = self
                .send(
                    Method::GET,
                    &format!("/cloud/v2/{task_path}"),
                    None,
                    REQUEST_TIMEOUT,
                )
                .and_then(|body| self.parse(&body, "a Luau execution task"))
                .map_err(|error| OpenCloudError::following(&task_path, error))?;
        };

        // The task has finished, so failing to read its logs must not throw
        // away its result.
        let mut logs = Vec::new();
        let (logs_truncated, logs_error) = match self.task_logs(&task_path, &mut logs) {
            Ok(truncated) => (truncated, None),
            Err(error) => (true, Some(error.to_string())),
        };
        Ok(LuauTaskResult {
            path: task_path,
            state,
            results: task.output.map(|output| output.results).unwrap_or_default(),
            error_code: task.error.as_ref().and_then(|error| error.code.clone()),
            error_message: task.error.and_then(|error| error.message),
            logs,
            logs_truncated,
            logs_error,
        })
    }

    /// Fetches every page of a task's logs into `logs`, up to
    /// [`MAX_LOG_BYTES`], and returns whether some were left out. On an error,
    /// `logs` keeps the pages fetched before it.
    fn task_logs(&self, task_path: &str, logs: &mut Vec<LogMessage>) -> Result<bool> {
        let mut total = 0;
        let mut token = String::new();
        for _ in 0..MAX_LOG_PAGES {
            let mut path = format!("/cloud/v2/{task_path}/logs?view=STRUCTURED&maxPageSize=10000");
            if !token.is_empty() {
                path.push_str("&pageToken=");
                path.push_str(&query_escape(&token));
            }
            let body = self.send(Method::GET, &path, None, REQUEST_TIMEOUT)?;
            let page: LogPage = self.parse(&body, "a page of Luau task logs")?;
            for entry in page.luau_execution_session_task_logs {
                // Roblox fills `messages` instead for the FLAT view; accept
                // both in case the view is ignored.
                let structured = entry.structured_messages.into_iter();
                let flat = entry.messages.into_iter().map(|message| StructuredMessage {
                    message,
                    message_type: String::new(),
                });
                for message in structured.chain(flat) {
                    total += message.message.len();
                    if total > MAX_LOG_BYTES {
                        return Ok(true);
                    }
                    logs.push(LogMessage {
                        message: message.message,
                        kind: message.message_type,
                    });
                }
            }
            match page.next_page_token {
                Some(next) if !next.is_empty() => token = next,
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Uploads a new asset, waits for Roblox to process it and returns its id.
    /// Callers that want to resume an interrupted upload later use
    /// [`OpenCloud::start_asset_upload`] and [`OpenCloud::resume_asset_upload`]
    /// instead.
    #[allow(clippy::too_many_arguments, dead_code)]
    pub fn upload_asset(
        &self,
        asset_type: AssetType,
        display_name: &str,
        description: &str,
        creator: Creator,
        file_name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<UploadedAsset> {
        let started = self.start_asset_upload(
            asset_type,
            display_name,
            description,
            creator,
            file_name,
            content_type,
            bytes,
        )?;
        self.finish_asset_upload(started)
    }

    /// Sends a new asset and returns the operation Roblox created for it,
    /// without waiting for processing. From a successful return on, the asset
    /// may exist: a caller that wants to avoid duplicates records
    /// [`StartedUpload::operation`] before calling
    /// [`OpenCloud::finish_asset_upload`], and passes it to
    /// [`OpenCloud::resume_asset_upload`] if that is interrupted.
    #[allow(clippy::too_many_arguments)]
    pub fn start_asset_upload(
        &self,
        asset_type: AssetType,
        display_name: &str,
        description: &str,
        creator: Creator,
        file_name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<StartedUpload> {
        let creator = match creator {
            Creator::User(id) => serde_json::json!({ "userId": id.to_string() }),
            Creator::Group(id) => serde_json::json!({ "groupId": id.to_string() }),
        };
        let request = serde_json::json!({
            "assetType": asset_type.as_str(),
            "displayName": display_name,
            "description": description,
            "creationContext": { "creator": creator },
        });
        let (body, content_type) = multipart(
            &serde_json::to_vec(&request).expect("JSON values always serialize"),
            file_name,
            content_type,
            bytes,
        )
        .map_err(OpenCloudError::NotSent)?;
        let response = self.send(
            Method::POST,
            "/assets/v1/assets",
            Some((&body, &content_type)),
            upload_timeout(body.len()),
        )?;
        let operation: Operation = self.parse(&response, "an asset operation")?;
        let path = self
            .checked_path(&operation.path, "operations/")
            .map_err(|error| self.unconfirmed(error))?
            .to_owned();
        Ok(StartedUpload {
            operation: path,
            state: operation,
        })
    }

    /// Waits for an upload from [`OpenCloud::start_asset_upload`] to finish
    /// processing and returns its asset. Errors other than
    /// [`OpenCloudError::Failed`] leave the operation to be resumed.
    pub fn finish_asset_upload(&self, started: StartedUpload) -> Result<UploadedAsset> {
        let StartedUpload {
            operation: operation_path,
            state: mut operation,
        } = started;
        let deadline = Instant::now() + self.timing.asset_timeout;
        let mut delay = self.timing.poll_initial;
        while !operation.done {
            let now = Instant::now();
            if now >= deadline {
                return Err(OpenCloudError::TimedOut(format!(
                    "asset upload {operation_path} was not done after {} seconds",
                    self.timing.asset_timeout.as_secs()
                )));
            }
            sleep(delay.min(deadline - now));
            delay = (delay * 3 / 2).min(self.timing.poll_max);
            // The asset may exist from here on, so a refused or unsent poll
            // must not read as "nothing happened".
            operation = self
                .send(
                    Method::GET,
                    &format!("/assets/v1/{operation_path}"),
                    None,
                    REQUEST_TIMEOUT,
                )
                .and_then(|response| self.parse(&response, "an asset operation"))
                .map_err(|error| OpenCloudError::following(&operation_path, error))?;
        }

        if let Some(error) = operation.error.filter(|error| !error.is_null()) {
            return Err(OpenCloudError::Failed(self.redact(format!(
                "asset upload {operation_path} failed: {}",
                self.excerpt(&error.to_string()).trim_start_matches(": ")
            ))));
        }
        let asset = operation.response.unwrap_or(Value::Null);
        // Open Cloud v1 encodes 64-bit ids as strings, but accept numbers too.
        let asset_id = match &asset["assetId"] {
            Value::String(id) => id.parse().ok(),
            id => id.as_u64(),
        }
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            self.unconfirmed(format!(
                "asset upload {operation_path} finished without a valid assetId{}",
                self.excerpt(&asset.to_string())
            ))
        })?;
        Ok(UploadedAsset {
            asset_id,
            asset_type: asset["assetType"].as_str().map(str::to_owned),
            moderation_state: asset["moderationResult"]["moderationState"]
                .as_str()
                .map(str::to_owned),
        })
    }

    /// Picks up an asset upload whose operation path (such as
    /// `operations/abc`) was recorded by an earlier, interrupted run, and
    /// waits for it like [`OpenCloud::finish_asset_upload`]. The path is
    /// checked the same way as one Roblox returned, so a damaged or crafted
    /// record cannot send the key anywhere but an asset operation.
    pub fn resume_asset_upload(&self, operation: &str) -> Result<UploadedAsset> {
        let path = self
            .checked_path(operation, "operations/")
            .map_err(|error| {
                OpenCloudError::NotSent(format!("cannot resume asset upload: {error}"))
            })?
            .to_owned();
        self.finish_asset_upload(StartedUpload {
            state: Operation {
                path: path.clone(),
                done: false,
                error: None,
                response: None,
            },
            operation: path,
        })
    }

    /// The place versions Roblox offers as game server filters for a place,
    /// as given (decimal strings, whether Roblox sends numbers or strings).
    pub fn place_versions(&self, universe_id: u64, place_id: u64) -> Result<Vec<String>> {
        let path = format!(
            "/server-management/v1/universes/{universe_id}/places/{place_id}/game-servers:filter-options?Filter=PlaceVersion"
        );
        let body = self.send(Method::GET, &path, None, REQUEST_TIMEOUT)?;
        let options: Value = self.parse(&body, "game server filter options")?;
        let values = match &options["filters"]["PlaceVersion"]["values"] {
            Value::Array(values) => values,
            Value::Null => return Ok(Vec::new()),
            _ => {
                return Err(self.unconfirmed(format!(
                    "filter options have no list of place versions{}",
                    self.excerpt(&body)
                )))
            }
        };
        Ok(values
            .iter()
            .filter_map(|value| match value {
                Value::String(text) => Some(text.trim().to_owned()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            })
            .filter(|version| path_segment(version))
            .collect())
    }

    /// One page of the game servers, active and shut down, that ran
    /// `place_version` of a place.
    pub fn list_game_servers(
        &self,
        universe_id: u64,
        place_id: u64,
        place_version: &str,
        page_token: Option<&str>,
    ) -> Result<GameServerPage> {
        let version = self.segment(place_version, "place version")?;
        let path = format!(
            "/server-management/v1/universes/{universe_id}/places/{place_id}/versions/{version}/game-servers{}",
            query(&[("MaxPageSize", "100"), ("PageToken", page_token.unwrap_or(""))])
        );
        let body = self.send(Method::GET, &path, None, REQUEST_TIMEOUT)?;
        let page: RawGameServerPage = self.parse(&body, "a page of game servers")?;
        Ok(GameServerPage {
            incomplete: page.active_servers_fetch_error == Some(true)
                || page.shutdown_servers_fetch_error == Some(true),
            servers: page
                .game_servers
                .unwrap_or_default()
                .iter()
                .filter_map(GameServer::from_value)
                .collect(),
            next_page_token: page.next_page_token.filter(|token| !token.is_empty()),
        })
    }

    /// One page of a game server's logs, newest first. `filter` is a CEL
    /// expression Roblox applies, or empty for every entry.
    pub fn list_game_server_logs(
        &self,
        universe_id: u64,
        place_id: u64,
        place_version: &str,
        job_id: &str,
        filter: &str,
        page_token: Option<&str>,
    ) -> Result<GameServerLogPage> {
        let version = self.segment(place_version, "place version")?;
        let job = self.segment(job_id, "job id")?;
        let path = format!(
            "/server-management/v1/universes/{universe_id}/places/{place_id}/versions/{version}/game-servers/{job}/logs{}",
            game_server_logs_query(filter, page_token)
        );
        let body = self.send(Method::GET, &path, None, REQUEST_TIMEOUT)?;
        let page: RawGameServerLogPage = self.parse(&body, "a page of game server logs")?;
        Ok(GameServerLogPage {
            logs: page
                .game_server_logs
                .unwrap_or_default()
                .iter()
                .map(GameServerLog::from_value)
                .collect(),
            next_page_token: page.next_page_token.filter(|token| !token.is_empty()),
        })
    }

    /// Checks a value that goes into a URL path, such as a job id that came
    /// from a response, so it cannot change which endpoint is requested.
    fn segment<'a>(&self, value: &'a str, what: &str) -> Result<&'a str> {
        if path_segment(value) {
            Ok(value)
        } else {
            Err(OpenCloudError::NotSent(format!(
                "{what} {:?} cannot be used in a request path",
                self.redact(value.to_owned())
            )))
        }
    }

    /// Sends one request, retrying where that cannot duplicate its effect,
    /// and returns the body of a successful response.
    fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<(&[u8], &str)>,
        timeout: Duration,
    ) -> Result<String> {
        let idempotent = method == Method::GET;
        // The query is left out of messages since page tokens are long.
        let request_name = format!("{method} {}", path.split('?').next().unwrap_or(path));
        let url = format!("{}{path}", self.base_url);
        let mut attempt = 1;
        loop {
            let mut request: RequestBuilder = self
                .client
                .request(method.clone(), &url)
                .header("x-api-key", self.key.clone())
                .timeout(timeout);
            if let Some((bytes, content_type)) = body {
                request = request
                    .header("Content-Type", content_type)
                    .body(bytes.to_vec());
            }
            let can_retry = attempt < self.timing.max_attempts;
            // Each attempt counts against Roblox's limits, retries too.
            self.pace();
            let response = match request.send() {
                Ok(response) => response,
                Err(error) => {
                    // Builder errors and connection failures (DNS, refused
                    // connections, TLS handshakes, connect timeouts) happen
                    // before anything reaches Roblox.
                    let sent = !(error.is_builder() || error.is_connect());
                    if can_retry && !error.is_builder() && (idempotent || !sent) {
                        sleep(self.backoff(attempt));
                        attempt += 1;
                        continue;
                    }
                    let message = self.redact(format!("{request_name}: {error}"));
                    return Err(if sent {
                        OpenCloudError::Unconfirmed(message)
                    } else {
                        OpenCloudError::NotSent(message)
                    });
                }
            };
            let status = response.status();
            // 429 means Roblox refused the request without acting on it.
            let retryable =
                status == StatusCode::TOO_MANY_REQUESTS || (idempotent && status.is_server_error());
            if retryable && can_retry {
                let delay = retry_after(&response).unwrap_or_else(|| self.backoff(attempt));
                sleep(delay.min(self.timing.retry_max));
                attempt += 1;
                continue;
            }
            let text = response.text();
            if !status.is_success() {
                return Err(OpenCloudError::Status {
                    status,
                    request: request_name,
                    body: text.map(|text| self.excerpt(&text)).unwrap_or_default(),
                });
            }
            return text.map_err(|error| {
                self.unconfirmed(format!(
                    "{request_name}: response could not be read: {error}"
                ))
            });
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempt.saturating_sub(1));
        self.timing
            .retry_base
            .saturating_mul(factor)
            .min(self.timing.retry_max)
    }

    fn parse<T: for<'de> Deserialize<'de>>(&self, body: &str, what: &str) -> Result<T> {
        serde_json::from_str(body).map_err(|_| {
            self.unconfirmed(format!(
                "expected {what}, got{}",
                excerpt_or_empty(&self.redact(body.to_owned()))
            ))
        })
    }

    fn unconfirmed(&self, message: String) -> OpenCloudError {
        OpenCloudError::Unconfirmed(self.redact(message))
    }

    /// Removes the key from text that came from a server, in case it echoes
    /// request headers back.
    /// `text` with every occurrence of this client's API key taken out, for
    /// output that does not come from Open Cloud errors, which are already
    /// redacted.
    pub fn redact_key(&self, text: &str) -> String {
        self.redact(text.to_owned())
    }

    fn redact(&self, text: String) -> String {
        match std::str::from_utf8(self.key.as_bytes()) {
            Ok(key) if text.contains(key) => text.replace(key, "[redacted]"),
            _ => text,
        }
    }

    /// [`excerpt`] of server text with the key removed first: shortening
    /// first could cut an echoed key in two and leave its start behind,
    /// where [`OpenCloud::redact`] no longer finds it.
    fn excerpt(&self, text: &str) -> String {
        excerpt(&self.redact(text.to_owned()))
    }

    /// Checks that a resource path from a response is a plain relative path
    /// starting with `prefix`, so it cannot add a query or climb out of the
    /// API.
    fn checked_path<'a>(
        &self,
        path: &'a str,
        prefix: &str,
    ) -> std::result::Result<&'a str, String> {
        let valid = path.starts_with(prefix)
            && path.len() > prefix.len()
            && path.len() <= 512
            && path
                .split('/')
                .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
            && path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'));
        if valid {
            Ok(path)
        } else {
            Err(format!(
                "response has an unexpected resource path{}",
                self.excerpt(path)
            ))
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskResource {
    path: String,
    #[serde(default)]
    state: String,
    error: Option<TaskError>,
    output: Option<TaskOutput>,
}

#[derive(Deserialize)]
struct TaskError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct TaskOutput {
    #[serde(default)]
    results: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogPage {
    #[serde(default)]
    luau_execution_session_task_logs: Vec<LogEntry>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogEntry {
    #[serde(default)]
    messages: Vec<String>,
    #[serde(default)]
    structured_messages: Vec<StructuredMessage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StructuredMessage {
    #[serde(default)]
    message: String,
    #[serde(default)]
    message_type: String,
}

#[derive(Debug, Deserialize)]
struct Operation {
    path: String,
    #[serde(default)]
    done: bool,
    error: Option<Value>,
    response: Option<Value>,
}

/// A game server from the server management API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameServer {
    pub job_id: String,
    pub place_version: String,
    pub shut_down: bool,
    /// As Roblox gives it; `None` while the server runs.
    pub termination_time: Option<String>,
}

impl GameServer {
    /// Reads a `GameServer`, skipping one without a job id that could be put
    /// in a path, since its logs could not be requested.
    fn from_value(value: &Value) -> Option<GameServer> {
        let job_id = id_string(&value["jobId"])?;
        Some(GameServer {
            job_id,
            place_version: id_string(&value["placeVersion"]).unwrap_or_default(),
            shut_down: value["shutDown"].as_bool().unwrap_or(false),
            termination_time: value["terminationTime"]
                .as_str()
                .filter(|time| !time.is_empty())
                .map(str::to_owned),
        })
    }
}

#[derive(Debug)]
pub struct GameServerPage {
    pub servers: Vec<GameServer>,
    pub next_page_token: Option<String>,
    /// Roblox could not fetch its active or its shut-down servers, so the
    /// page may be missing some (`activeServersFetchError` or
    /// `shutdownServersFetchError`).
    pub incomplete: bool,
}

/// A log entry's severity. Roblox documents `LogSeverity` as the numbers
/// 0 to 3, but names are accepted too in case it serializes the enum by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogSeverity {
    Output,
    Info,
    Warning,
    Error,
    /// A value Rojo does not know, kept as Roblox sent it.
    Other(String),
}

impl LogSeverity {
    pub fn parse(value: &Value) -> LogSeverity {
        let text = match value {
            Value::Number(number) => number.to_string(),
            Value::String(text) => text.trim().to_ascii_lowercase(),
            Value::Null => return LogSeverity::Other(String::new()),
            other => return LogSeverity::Other(other.to_string()),
        };
        let name = text
            .strip_prefix("log_severity_")
            .or_else(|| text.strip_prefix("message"))
            .unwrap_or(&text);
        match name {
            "0" | "output" => LogSeverity::Output,
            "1" | "info" => LogSeverity::Info,
            "2" | "warning" | "warn" => LogSeverity::Warning,
            "3" | "error" => LogSeverity::Error,
            _ => LogSeverity::Other(text),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            LogSeverity::Output => "output",
            LogSeverity::Info => "info",
            LogSeverity::Warning => "warning",
            LogSeverity::Error => "error",
            LogSeverity::Other(text) => text,
        }
    }
}

/// One entry from a game server's logs.
#[derive(Debug, Clone, PartialEq)]
pub struct GameServerLog {
    /// Milliseconds since the Unix epoch; `None` when Roblox gave none that
    /// could be read.
    pub timestamp_ms: Option<u64>,
    pub severity: LogSeverity,
    pub universe_id: String,
    pub place_id: String,
    pub place_version: String,
    pub job_id: String,
    pub message: String,
    pub stack_trace: String,
    pub message_template: Option<String>,
    pub context: Option<String>,
    pub logger_full_path: Option<String>,
    /// Identical messages in the same window that were folded into this one.
    pub skipped_count: u64,
    /// Messages past the server's overall limit folded into this one.
    pub rate_limited_count: u64,
    /// As Roblox sent it, or `Null`.
    pub structured_stack_trace: Value,
}

impl GameServerLog {
    /// Reads a `GameServerLog` field by field, so one field of an unexpected
    /// type does not lose the whole entry.
    fn from_value(value: &Value) -> GameServerLog {
        let text = |name: &str| value[name].as_str().unwrap_or_default().to_owned();
        let optional = |name: &str| {
            value[name]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let count = |name: &str| match &value[name] {
            Value::String(text) => text.trim().parse().unwrap_or(0),
            other => other.as_u64().unwrap_or(0),
        };
        GameServerLog {
            timestamp_ms: parse_timestamp_ms(&value["messageTimestampMs"]),
            severity: LogSeverity::parse(&value["severity"]),
            universe_id: id_string(&value["universeId"]).unwrap_or_default(),
            place_id: id_string(&value["placeId"]).unwrap_or_default(),
            place_version: id_string(&value["placeVersion"]).unwrap_or_default(),
            job_id: id_string(&value["jobId"]).unwrap_or_default(),
            message: text("message"),
            stack_trace: text("stackTrace"),
            message_template: optional("messageTemplate"),
            context: optional("context"),
            logger_full_path: optional("loggerFullPath"),
            skipped_count: count("skippedCount"),
            rate_limited_count: count("rateLimitedCount"),
            structured_stack_trace: value["structuredStackTrace"].clone(),
        }
    }
}

#[derive(Debug)]
pub struct GameServerLogPage {
    pub logs: Vec<GameServerLog>,
    pub next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawGameServerPage {
    // Option, so the explicit `null` the schema allows reads as empty.
    #[serde(default)]
    game_servers: Option<Vec<Value>>,
    next_page_token: Option<String>,
    #[serde(default)]
    active_servers_fetch_error: Option<bool>,
    #[serde(default)]
    shutdown_servers_fetch_error: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawGameServerLogPage {
    #[serde(default)]
    game_server_logs: Option<Vec<Value>>,
    next_page_token: Option<String>,
}

/// The query for one page of game server logs. Entries come newest first,
/// so a caller can stop paging once it reaches entries it has seen.
fn game_server_logs_query(filter: &str, page_token: Option<&str>) -> String {
    query(&[
        ("MaxPageSize", "100"),
        ("OrderBy", "message_timestamp desc"),
        ("Filter", filter),
        ("PageToken", page_token.unwrap_or("")),
    ])
}

/// Builds `?name=value&...` from the parameters that have a value.
fn query(params: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (name, value) in params.iter().filter(|(_, value)| !value.is_empty()) {
        out.push(if out.is_empty() { '?' } else { '&' });
        out.push_str(name);
        out.push('=');
        out.push_str(&query_escape(value));
    }
    out
}

/// Whether `value` is safe as one URL path segment: Roblox's job ids are
/// GUIDs and place versions are numbers, so anything else is refused.
/// Whether `value` can be used as one segment of a request path, as Open
/// Cloud requests check before sending. Callers can check arguments with it
/// up front, rather than have every request with them refused.
pub fn is_path_segment(value: &str) -> bool {
    path_segment(value)
}

fn path_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// An id that Open Cloud may send as a number or a string.
fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Reads `messageTimestampMs`, documented as a string of milliseconds. A
/// number or an RFC 3339 time is accepted as well.
fn parse_timestamp_ms(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64)),
        Value::String(text) => {
            let text = text.trim();
            text.parse().ok().or_else(|| {
                humantime::parse_rfc3339_weak(text)
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|elapsed| elapsed.as_millis() as u64)
            })
        }
        _ => None,
    }
}

/// Admits a request at `now` if the slot `next` has come, booking the next
/// slot `spacing` after the actual admission, and otherwise says how long to
/// wait before asking again. Booking from `now` rather than from the slot
/// keeps a late wakeup from letting two requests through back to back.
fn admit(next: &mut Option<Instant>, now: Instant, spacing: Duration) -> Option<Duration> {
    match *next {
        Some(slot) if slot > now => Some(slot - now),
        _ => {
            *next = Some(now + spacing);
            None
        }
    }
}

/// The whole request must finish within this time. Large uploads need longer
/// on slow uplinks: 120 seconds plus one second per 256 KiB.
fn upload_timeout(bytes: usize) -> Duration {
    Duration::from_secs(120 + bytes as u64 / (256 * 1024))
}

/// Reads a `Retry-After` header given in seconds. Roblox does not send the
/// HTTP-date form.
fn retry_after(response: &reqwest::blocking::Response) -> Option<Duration> {
    let value = response.headers().get("retry-after")?.to_str().ok()?;
    let seconds: f64 = value.trim().parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds.min(3600.0)))
}

/// Percent-encodes everything but unreserved characters.
fn query_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            escaped.push(byte as char);
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    escaped
}

/// Builds a `multipart/form-data` body with a JSON `request` part and a
/// `fileContent` part, returning it with its content type. Built by hand to
/// avoid pulling in reqwest's multipart support and its dependencies.
fn multipart(
    request: &[u8],
    file_name: &str,
    content_type: &str,
    bytes: &[u8],
) -> std::result::Result<(Vec<u8>, String), String> {
    // Quotes and backslashes are refused rather than escaped, since servers
    // disagree on how a quoted filename is unescaped.
    let header_safe = |value: &str| {
        !value
            .bytes()
            .any(|b| b.is_ascii_control() || b == b'"' || b == b'\\')
    };
    if !header_safe(file_name) || file_name.is_empty() {
        return Err(format!(
            "file name {file_name:?} cannot be used in an upload"
        ));
    }
    if !header_safe(content_type) || content_type.is_empty() {
        return Err(format!(
            "content type {content_type:?} cannot be used in an upload"
        ));
    }
    // A random boundary is practically never in the file, but checking
    // keeps a corrupt body impossible.
    let boundary = loop {
        let boundary = format!("rojo-{}", uuid::Uuid::new_v4().simple());
        let needle = boundary.as_bytes();
        if !bytes.windows(needle.len()).any(|window| window == needle) {
            break boundary;
        }
    };
    let mut body = Vec::with_capacity(bytes.len() + request.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"request\"\r\nContent-Type: application/json\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(request);
    body.extend_from_slice(
        format!(
            "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"fileContent\"; filename=\"{file_name}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((body, format!("multipart/form-data; boundary={boundary}")))
}

/// A short, single-line copy of a response body for error messages, prefixed
/// with `": "` unless empty.
fn excerpt(body: &str) -> String {
    let text: String = body
        .trim()
        .chars()
        .take(1000)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

fn excerpt_or_empty(body: &str) -> String {
    match excerpt(body) {
        text if text.is_empty() => " an empty body".to_owned(),
        text => format!(" a body other than that{text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
    };

    const KEY: &str = "fixture-key-0123456789";

    #[derive(Debug, Clone)]
    struct Recorded {
        method: String,
        target: String,
        headers: String,
        body: Vec<u8>,
        /// When the request arrived.
        at: Instant,
    }

    struct Reply {
        status: &'static str,
        headers: &'static str,
        body: String,
    }

    fn reply(status: &'static str, body: impl Into<String>) -> Reply {
        Reply {
            status,
            headers: "",
            body: body.into(),
        }
    }

    /// Serves the replies in order on loopback, one connection each, and
    /// records the requests it received.
    fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Recorded>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&recorded);
        // Not joined: when a client makes fewer requests than expected, the
        // test fails on the recorded count instead of hanging.
        std::thread::spawn(move || {
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut data = Vec::new();
                let mut chunk = [0; 4096];
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
                    at: Instant::now(),
                });
                write!(
                    stream,
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.headers,
                    reply.body
                )
                .unwrap();
            }
        });
        (format!("http://{address}"), recorded)
    }

    fn client(base: &str) -> OpenCloud {
        let mut client = OpenCloud::with_base_url(KEY, base).unwrap();
        client.timing = Timing {
            max_attempts: 3,
            retry_base: Duration::from_millis(10),
            retry_max: Duration::from_millis(50),
            poll_initial: Duration::from_millis(10),
            poll_max: Duration::from_millis(20),
            luau_margin: Duration::ZERO,
            asset_timeout: Duration::from_millis(300),
        };
        client
    }

    fn publish(client: &OpenCloud) -> Result<u64> {
        client.publish_place(1, 2, b"\x00place", PlaceFormat::Binary, VersionType::Saved)
    }

    fn received(recorded: &Arc<Mutex<Vec<Recorded>>>) -> Vec<Recorded> {
        recorded.lock().unwrap().clone()
    }

    #[test]
    fn publish_sends_place_and_returns_version() {
        let (base, recorded) = serve(vec![reply("200 OK", r#"{"versionNumber":42}"#)]);
        assert_eq!(publish(&client(&base)).unwrap(), 42);
        let requests = received(&recorded);
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/universes/v1/1/places/2/versions?versionType=Saved"
        );
        assert!(request.headers.contains(&format!("x-api-key: {KEY}")));
        assert!(request
            .headers
            .contains("content-type: application/octet-stream"));
        assert_eq!(request.body, b"\x00place");

        let (base, recorded) = serve(vec![reply("200 OK", r#"{"versionNumber":7}"#)]);
        let version = client(&base)
            .publish_place(1, 2, b"<roblox/>", PlaceFormat::Xml, VersionType::Published)
            .unwrap();
        assert_eq!(version, 7);
        let request = &received(&recorded)[0];
        assert!(request.target.ends_with("versionType=Published"));
        assert!(request.headers.contains("content-type: application/xml"));
    }

    #[test]
    fn malformed_success_responses_are_unconfirmed() {
        for body in ["not json", "{}", r#"{"versionNumber":0}"#, ""] {
            let (base, _) = serve(vec![reply("200 OK", body)]);
            let error = publish(&client(&base)).unwrap_err();
            assert!(
                matches!(error, OpenCloudError::Unconfirmed(_)),
                "{body}: {error:?}"
            );
            // A success status without a usable receipt may still have published.
            assert!(error.may_have_applied());
        }
    }

    #[test]
    fn client_errors_report_status_and_body_without_retrying() {
        let (base, recorded) = serve(vec![reply(
            "403 Forbidden",
            r#"{"code":"PERMISSION_DENIED","message":"Insufficient scope"}"#,
        )]);
        let error = publish(&client(&base)).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("403"), "{message}");
        assert!(message.contains("Insufficient scope"), "{message}");
        assert!(
            message.contains("POST /universes/v1/1/places/2/versions"),
            "{message}"
        );
        assert!(!error.may_have_applied());
        assert_eq!(received(&recorded).len(), 1);
    }

    #[test]
    fn long_error_bodies_are_truncated_to_one_line() {
        let body = format!("line one\nline two {}", "x".repeat(5000));
        let (base, _) = serve(vec![reply("400 Bad Request", body)]);
        let message = publish(&client(&base)).unwrap_err().to_string();
        assert!(!message.contains('\n'));
        assert!(message.contains("line one line two"));
        assert!(message.len() < 1200, "{}", message.len());
    }

    #[test]
    fn rate_limits_are_retried_honouring_retry_after() {
        let limited = Reply {
            status: "429 Too Many Requests",
            headers: "Retry-After: 0\r\n",
            body: String::new(),
        };
        let (base, recorded) = serve(vec![limited, reply("200 OK", r#"{"versionNumber":3}"#)]);
        assert_eq!(publish(&client(&base)).unwrap(), 3);
        assert_eq!(received(&recorded).len(), 2);

        // Attempts are bounded, then the last status is reported.
        let (base, recorded) = serve(
            (0..3)
                .map(|_| reply("429 Too Many Requests", "slow down"))
                .collect(),
        );
        let error = publish(&client(&base)).unwrap_err();
        assert!(error.to_string().contains("429"), "{error}");
        assert!(!error.may_have_applied());
        assert_eq!(received(&recorded).len(), 3);
    }

    #[test]
    fn server_errors_are_only_retried_for_reads() {
        // A POST that failed with 5xx may have created a version.
        let (base, recorded) = serve(vec![reply("503 Service Unavailable", "down")]);
        let error = publish(&client(&base)).unwrap_err();
        assert!(error.may_have_applied());
        assert_eq!(received(&recorded).len(), 1);

        let (base, recorded) = serve(vec![
            reply(
                "200 OK",
                r#"{"path":"universes/1/places/2/versions/3/luau-execution-sessions/s/tasks/t","state":"QUEUED"}"#,
            ),
            reply("502 Bad Gateway", ""),
            reply(
                "200 OK",
                r#"{"path":"universes/1/places/2/versions/3/luau-execution-sessions/s/tasks/t","state":"COMPLETE"}"#,
            ),
            reply("200 OK", "{}"),
        ]);
        let result = client(&base)
            .run_luau(1, 2, None, "return 1", Duration::from_secs(1))
            .unwrap();
        assert_eq!(result.state, TaskState::Complete);
        assert_eq!(received(&recorded).len(), 4);
    }

    #[test]
    fn unsent_requests_are_reported_as_not_applied() {
        // Nothing listens on a port that was just released.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = publish(&client(&format!("http://{address}"))).unwrap_err();
        assert!(matches!(error, OpenCloudError::NotSent(_)), "{error:?}");
        assert!(!error.may_have_applied());
    }

    const TASK: &str = "universes/1/places/2/versions/9/luau-execution-sessions/abc-1/tasks/def-2";

    fn task(state: &str, extra: &str) -> Reply {
        reply(
            "200 OK",
            format!(r#"{{"path":"{TASK}","state":"{state}"{extra}}}"#),
        )
    }

    #[test]
    fn luau_task_is_polled_and_logs_are_paginated() {
        let (base, recorded) = serve(vec![
            task("QUEUED", ""),
            task("PROCESSING", ""),
            task("COMPLETE", r#","output":{"results":[{"answer":42},"two"]}"#),
            reply(
                "200 OK",
                r#"{"luauExecutionSessionTaskLogs":[{"structuredMessages":[{"message":"hello","messageType":"OUTPUT"},{"message":"careful","messageType":"WARNING"}]}],"nextPageToken":"next/page=="}"#,
            ),
            reply(
                "200 OK",
                r#"{"luauExecutionSessionTaskLogs":[{"messages":["flat"]}]}"#,
            ),
        ]);
        let result = client(&base)
            .run_luau(
                1,
                2,
                Some(9),
                "print('hello')",
                Duration::from_millis(30_500),
            )
            .unwrap();
        assert_eq!(result.state, TaskState::Complete);
        assert_eq!(result.path, TASK);
        assert_eq!(
            result.results,
            vec![serde_json::json!({"answer": 42}), serde_json::json!("two")]
        );
        assert_eq!(result.error_code, None);
        let logs: Vec<_> = result
            .logs
            .iter()
            .map(|log| (log.message.as_str(), log.kind.as_str()))
            .collect();
        assert_eq!(
            logs,
            [("hello", "OUTPUT"), ("careful", "WARNING"), ("flat", "")]
        );
        assert!(!result.logs_truncated);

        let requests = received(&recorded);
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].target,
            "/cloud/v2/universes/1/places/2/versions/9/luau-execution-session-tasks"
        );
        assert!(requests[0]
            .headers
            .contains("content-type: application/json"));
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        // Rounded up to whole seconds.
        assert_eq!(
            body,
            serde_json::json!({"script": "print('hello')", "timeout": "31s"})
        );
        for request in &requests[1..3] {
            assert_eq!(request.method, "GET");
            assert_eq!(request.target, format!("/cloud/v2/{TASK}"));
        }
        assert_eq!(
            requests[3].target,
            format!("/cloud/v2/{TASK}/logs?view=STRUCTURED&maxPageSize=10000")
        );
        assert_eq!(
            requests[4].target,
            format!("/cloud/v2/{TASK}/logs?view=STRUCTURED&maxPageSize=10000&pageToken=next%2Fpage%3D%3D")
        );
    }

    #[test]
    fn luau_task_without_version_uses_place_endpoint() {
        let (base, recorded) = serve(vec![task("COMPLETE", ""), reply("200 OK", "{}")]);
        let result = client(&base)
            .run_luau(1, 2, None, "return", Duration::ZERO)
            .unwrap();
        assert!(result.results.is_empty());
        let requests = received(&recorded);
        assert_eq!(
            requests[0].target,
            "/cloud/v2/universes/1/places/2/luau-execution-session-tasks"
        );
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["timeout"], "1s");
    }

    #[test]
    fn failed_luau_task_reports_its_error() {
        let (base, _) = serve(vec![
            task(
                "FAILED",
                r#","error":{"code":"SCRIPT_ERROR","message":"oops:1: boom"}"#,
            ),
            reply(
                "200 OK",
                r#"{"luauExecutionSessionTaskLogs":[{"structuredMessages":[{"message":"oops:1: boom","messageType":"ERROR"}]}]}"#,
            ),
        ]);
        let result = client(&base)
            .run_luau(1, 2, Some(9), "error('boom')", Duration::from_secs(5))
            .unwrap();
        assert_eq!(result.state, TaskState::Failed);
        assert_eq!(result.error_code.as_deref(), Some("SCRIPT_ERROR"));
        assert_eq!(result.error_message.as_deref(), Some("oops:1: boom"));
        assert_eq!(result.logs[0].kind, "ERROR");
    }

    #[test]
    fn luau_polling_stops_at_the_deadline() {
        let (base, recorded) = serve((0..200).map(|_| task("PROCESSING", "")).collect());
        let started = Instant::now();
        let error = client(&base)
            .run_luau(
                1,
                2,
                Some(9),
                "while true do end",
                Duration::from_millis(200),
            )
            .unwrap_err();
        assert!(matches!(error, OpenCloudError::TimedOut(_)), "{error:?}");
        assert!(error.to_string().contains("PROCESSING"), "{error}");
        assert!(error.may_have_applied());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(received(&recorded).len() > 2);
    }

    #[test]
    fn logs_are_capped() {
        let big = "x".repeat(MAX_LOG_BYTES / 2 + 1);
        let page = format!(
            r#"{{"luauExecutionSessionTaskLogs":[{{"messages":["{big}","{big}","{big}"]}}],"nextPageToken":"more"}}"#
        );
        let (base, recorded) = serve(vec![task("COMPLETE", ""), reply("200 OK", page)]);
        let result = client(&base)
            .run_luau(1, 2, Some(9), "", Duration::from_secs(1))
            .unwrap();
        assert!(result.logs_truncated);
        assert_eq!(result.logs.len(), 1);
        // The cap was hit on the first page, so no more were fetched.
        assert_eq!(received(&recorded).len(), 2);
    }

    #[test]
    fn unexpected_resource_paths_are_not_followed() {
        for path in [
            "universes/../../evil",
            "https://example.com/x",
            "universes/1?x=y",
            "other/1",
            "",
        ] {
            let (base, recorded) = serve(vec![reply(
                "200 OK",
                format!(r#"{{"path":"{path}","state":"QUEUED"}}"#),
            )]);
            let error = client(&base)
                .run_luau(1, 2, None, "", Duration::from_secs(1))
                .unwrap_err();
            assert!(matches!(error, OpenCloudError::Unconfirmed(_)), "{path}");
            assert_eq!(received(&recorded).len(), 1, "{path}");
        }
    }

    fn upload(client: &OpenCloud) -> Result<UploadedAsset> {
        client.upload_asset(
            AssetType::Image,
            "icon",
            "an icon",
            Creator::User(55),
            "icon.png",
            "image/png",
            b"\x89PNG-data",
        )
    }

    #[test]
    fn asset_upload_sends_multipart_and_polls_the_operation() {
        let (base, recorded) = serve(vec![
            reply("200 OK", r#"{"path":"operations/op-1","done":false}"#),
            reply("200 OK", r#"{"path":"operations/op-1","done":false}"#),
            reply(
                "200 OK",
                r#"{"path":"operations/op-1","done":true,"response":{"assetId":"123456","assetType":"Image","moderationResult":{"moderationState":"Approved"}}}"#,
            ),
        ]);
        let asset = upload(&client(&base)).unwrap();
        assert_eq!(asset.asset_id, 123456);
        assert_eq!(asset.asset_type.as_deref(), Some("Image"));
        assert_eq!(asset.moderation_state.as_deref(), Some("Approved"));

        let requests = received(&recorded);
        assert_eq!(requests.len(), 3);
        assert_eq!(
            (requests[0].method.as_str(), requests[0].target.as_str()),
            ("POST", "/assets/v1/assets")
        );
        assert_eq!(requests[1].target, "/assets/v1/operations/op-1");
        let boundary = requests[0]
            .headers
            .lines()
            .find_map(|line| line.strip_prefix("content-type: multipart/form-data; boundary="))
            .unwrap()
            .trim()
            .to_owned();
        let body = String::from_utf8_lossy(&requests[0].body).into_owned();
        let parts: Vec<_> = body.split(&format!("--{boundary}")).collect();
        assert_eq!(parts.len(), 4, "{body}");
        assert_eq!(parts[0], "");
        assert_eq!(parts[3], "--\r\n");
        let (request_head, request_json) = parts[1].split_once("\r\n\r\n").unwrap();
        assert!(request_head.contains("name=\"request\""));
        let request: Value =
            serde_json::from_str(request_json.strip_suffix("\r\n").unwrap()).unwrap();
        assert_eq!(
            request,
            serde_json::json!({
                "assetType": "Image",
                "displayName": "icon",
                "description": "an icon",
                "creationContext": {"creator": {"userId": "55"}},
            })
        );
        let (file_head, file) = parts[2].split_once("\r\n\r\n").unwrap();
        assert!(file_head.contains("name=\"fileContent\"; filename=\"icon.png\""));
        assert!(file_head.contains("Content-Type: image/png"));
        assert_eq!(file, "\u{fffd}PNG-data\r\n");
    }

    #[test]
    fn group_assets_and_numeric_ids_are_supported() {
        let (base, recorded) = serve(vec![reply(
            "200 OK",
            r#"{"path":"operations/op","done":true,"response":{"assetId":77}}"#,
        )]);
        let asset = client(&base)
            .upload_asset(
                AssetType::Audio,
                "a",
                "",
                Creator::Group(9),
                "a.ogg",
                "audio/ogg",
                b"",
            )
            .unwrap();
        assert_eq!(asset.asset_id, 77);
        let body = String::from_utf8_lossy(&received(&recorded)[0].body).into_owned();
        assert!(body.contains(r#""groupId":"9""#), "{body}");
        assert!(body.contains(r#""assetType":"Audio""#), "{body}");
    }

    #[test]
    fn failed_or_unfinished_asset_operations_are_errors() {
        let (base, _) = serve(vec![reply(
            "200 OK",
            r#"{"path":"operations/op","done":true,"error":{"code":"INVALID_ARGUMENT","message":"bad image"}}"#,
        )]);
        let error = upload(&client(&base)).unwrap_err();
        assert!(matches!(error, OpenCloudError::Failed(_)), "{error:?}");
        assert!(error.to_string().contains("bad image"), "{error}");

        let (base, _) = serve(
            (0..100)
                .map(|_| reply("200 OK", r#"{"path":"operations/op","done":false}"#))
                .collect(),
        );
        let error = upload(&client(&base)).unwrap_err();
        assert!(matches!(error, OpenCloudError::TimedOut(_)), "{error:?}");

        let (base, _) = serve(vec![reply(
            "200 OK",
            r#"{"path":"operations/op","done":true,"response":{}}"#,
        )]);
        assert!(matches!(
            upload(&client(&base)).unwrap_err(),
            OpenCloudError::Unconfirmed(_)
        ));
    }

    #[test]
    fn refused_asset_polls_still_report_that_the_asset_may_exist() {
        // A key with asset:write but not asset:read can create assets but
        // gets 403 for every poll.
        let (base, recorded) = serve(vec![
            reply("200 OK", r#"{"path":"operations/op-1","done":false}"#),
            reply("403 Forbidden", r#"{"message":"missing scope asset:read"}"#),
        ]);
        let error = upload(&client(&base)).unwrap_err();
        assert!(
            matches!(&error, OpenCloudError::Unfollowed { path, .. } if path == "operations/op-1"),
            "{error:?}"
        );
        assert!(error.may_have_applied());
        let message = error.to_string();
        assert!(message.contains("operations/op-1"), "{message}");
        assert!(message.contains("403"), "{message}");
        assert_eq!(received(&recorded).len(), 2);

        // Likewise when polling cannot reach Roblox at all.
        let (base, _) = serve(vec![reply(
            "200 OK",
            r#"{"path":"operations/op-2","done":false}"#,
        )]);
        let client = client(&base);
        let started = client
            .start_asset_upload(
                AssetType::Image,
                "icon",
                "",
                Creator::User(1),
                "icon.png",
                "image/png",
                b"x",
            )
            .unwrap();
        assert_eq!(started.operation, "operations/op-2");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let gone = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let mut client = client;
        client.base_url = gone;
        let error = client.finish_asset_upload(started).unwrap_err();
        assert!(
            matches!(&error, OpenCloudError::Unfollowed { error, .. } if matches!(**error, OpenCloudError::NotSent(_))),
            "{error:?}"
        );
        assert!(error.may_have_applied());
    }

    #[test]
    fn resumed_asset_uploads_poll_without_sending_the_file_again() {
        let (base, recorded) = serve(vec![reply(
            "200 OK",
            r#"{"path":"operations/op-9","done":true,"response":{"assetId":"99","moderationResult":{"moderationState":"Approved"}}}"#,
        )]);
        let asset = client(&base)
            .resume_asset_upload("operations/op-9")
            .unwrap();
        assert_eq!(asset.asset_id, 99);
        let requests = received(&recorded);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            (requests[0].method.as_str(), requests[0].target.as_str()),
            ("GET", "/assets/v1/operations/op-9")
        );

        let client = client("http://127.0.0.1:1");
        for path in [
            "operations/../assets",
            "https://x/operations/1",
            "other/1",
            "",
        ] {
            let error = client.resume_asset_upload(path).unwrap_err();
            assert!(matches!(error, OpenCloudError::NotSent(_)), "{path}");
        }
    }

    #[test]
    fn refused_luau_polls_still_report_that_the_task_may_have_run() {
        let (base, _) = serve(vec![
            task("QUEUED", ""),
            reply("403 Forbidden", r#"{"message":"missing read scope"}"#),
        ]);
        let error = client(&base)
            .run_luau(1, 2, Some(9), "return 1", Duration::from_secs(5))
            .unwrap_err();
        assert!(
            matches!(&error, OpenCloudError::Unfollowed { path, .. } if path == TASK),
            "{error:?}"
        );
        assert!(error.may_have_applied());
    }

    #[test]
    fn failed_log_fetches_keep_the_finished_task_result() {
        let (base, _) = serve(vec![
            task("COMPLETE", r#","output":{"results":[true]}"#),
            reply(
                "200 OK",
                r#"{"luauExecutionSessionTaskLogs":[{"messages":["first page"]}],"nextPageToken":"p2"}"#,
            ),
            reply("403 Forbidden", r#"{"message":"no logs for you"}"#),
        ]);
        let result = client(&base)
            .run_luau(1, 2, Some(9), "return true", Duration::from_secs(5))
            .unwrap();
        assert_eq!(result.state, TaskState::Complete);
        assert_eq!(result.results, vec![Value::Bool(true)]);
        assert_eq!(result.logs[0].message, "first page");
        assert!(result.logs_truncated);
        let logs_error = result.logs_error.unwrap();
        assert!(logs_error.contains("403"), "{logs_error}");
    }

    #[test]
    fn unsafe_multipart_names_are_refused_before_sending() {
        let client = client("http://127.0.0.1:1");
        for (file_name, content_type) in [
            ("a\".png", "image/png"),
            ("a\\b.png", "image/png"),
            ("a\r\nX: y", "image/png"),
            ("", "image/png"),
            ("a.png", "image/png\r\nX: y"),
        ] {
            let error = client
                .upload_asset(
                    AssetType::Image,
                    "a",
                    "",
                    Creator::User(1),
                    file_name,
                    content_type,
                    b"",
                )
                .unwrap_err();
            assert!(matches!(error, OpenCloudError::NotSent(_)), "{error:?}");
        }
    }

    #[test]
    fn key_never_appears_in_errors_or_debug_output() {
        // A server that echoes the key back in an error or success body.
        let (base, _) = serve(vec![
            reply(
                "401 Unauthorized",
                format!(r#"{{"message":"bad key {KEY}"}}"#),
            ),
            reply("200 OK", format!("not json {KEY}")),
            reply(
                "200 OK",
                format!(
                    r#"{{"path":"operations/{KEY}","done":true,"error":{{"message":"{KEY}"}}}}"#
                ),
            ),
        ]);
        let client = client(&base);
        let errors = [
            publish(&client).unwrap_err(),
            publish(&client).unwrap_err(),
            upload(&client).unwrap_err(),
        ];
        for error in &errors {
            for text in [
                error.to_string(),
                format!("{error:?}"),
                format!("{:#}", anyhow::Error::msg(error.to_string())),
            ] {
                assert!(!text.contains(KEY), "{text}");
            }
        }
        assert!(errors[0].to_string().contains("[redacted]"));
        assert!(!format!("{client:?}").contains(KEY));

        let error = OpenCloud::with_base_url("bad\nkey-secret", "http://x").unwrap_err();
        assert!(!format!("{error:?}").contains("key-secret"));
    }

    #[test]
    fn a_key_echoed_across_the_excerpt_limit_does_not_leak_in_part() {
        // Places the key 990 characters into the text that is shortened to
        // 1000, after `before` characters of JSON around it.
        let echo = |before: usize| format!("{}{KEY}", "x".repeat(990 - before));
        let (base, _) = serve(vec![
            reply("401 Unauthorized", echo(0)),
            reply("200 OK", echo(0)),
            reply("200 OK", format!(r#"{{"m":"{}"}}"#, echo(6))),
            reply(
                "200 OK",
                format!(
                    r#"{{"path":"operations/op-1","done":true,"error":{{"m":"{}"}}}}"#,
                    echo(6)
                ),
            ),
            reply(
                "200 OK",
                format!(
                    r#"{{"path":"operations/op-1","done":true,"response":{{"m":"{}"}}}}"#,
                    echo(6)
                ),
            ),
            reply(
                "200 OK",
                format!(r#"{{"path":"operations/{}","done":true}}"#, echo(11)),
            ),
        ]);
        let client = client(&base);
        let errors = [
            // Error status, unparsable body, missing versionNumber.
            publish(&client).unwrap_err(),
            publish(&client).unwrap_err(),
            publish(&client).unwrap_err(),
            // Failed operation, missing assetId, unexpected path.
            upload(&client).unwrap_err(),
            upload(&client).unwrap_err(),
            upload(&client).unwrap_err(),
        ];
        for error in &errors {
            let text = format!("{error} {error:?}");
            assert!(text.contains("xxxxx"), "{text}");
            assert!(!text.contains(&KEY[..10]), "{text}");
        }
    }

    #[test]
    fn from_env_names_the_variable_but_not_the_value() {
        let missing = "ROJO_OPEN_CLOUD_TEST_MISSING_VAR";
        let error = OpenCloud::from_env(missing).unwrap_err().to_string();
        assert!(error.contains(missing), "{error}");

        let invalid = "ROJO_OPEN_CLOUD_TEST_INVALID_VAR";
        std::env::set_var(invalid, "secret\u{1}value");
        let error = format!("{:?}", OpenCloud::from_env(invalid).unwrap_err());
        std::env::remove_var(invalid);
        assert!(error.contains(invalid), "{error}");
        assert!(!error.contains("secret"), "{error}");

        let valid = "ROJO_OPEN_CLOUD_TEST_VALID_VAR";
        std::env::set_var(valid, " padded-key \n");
        let client = OpenCloud::from_env(valid).unwrap();
        std::env::remove_var(valid);
        assert_eq!(client.key.as_bytes(), b"padded-key");
        assert!(client.key.is_sensitive());
        assert_eq!(client.base_url, DEFAULT_BASE_URL);
    }

    /// An 8x8 opaque PNG, built by hand with stored (uncompressed) deflate
    /// blocks to avoid an image dependency.
    /// A `ListGameServerLogsResponse` shaped after the documented
    /// `GameServerLog` model. No live response was available to record.
    const LOG_PAGE: &str = r#"{
        "gameServerLogs": [
            {
                "messageTimestampMs": "1794052800123",
                "universeId": 1,
                "placeId": "2",
                "placeVersion": "45",
                "jobId": "6f1c2d3e-0000-4000-8000-abcdefabcdef",
                "severity": 3,
                "message": "ServerScriptService.Main:10: attempt to index nil",
                "stackTrace": "ServerScriptService.Main, line 10",
                "messageTemplate": "attempt to index {thing}",
                "context": "{\"thing\":\"nil\"}",
                "skippedCount": 4,
                "rateLimitedCount": 0,
                "structuredStackTrace": [{"line": 10, "scriptPath": "ServerScriptService.Main"}]
            },
            {
                "messageTimestampMs": "1794052799000",
                "universeId": 1,
                "placeId": "2",
                "placeVersion": "45",
                "jobId": "6f1c2d3e-0000-4000-8000-abcdefabcdef",
                "severity": "Warning",
                "message": "Infinite yield possible",
                "stackTrace": "",
                "skippedCount": "0",
                "rateLimitedCount": 7
            }
        ],
        "nextPageToken": "page two/+="
    }"#;

    #[test]
    fn game_server_logs_request_and_parse() {
        let (base, recorded) = serve(vec![reply("200 OK", LOG_PAGE)]);
        let page = client(&base)
            .list_game_server_logs(
                1,
                2,
                "45",
                "6f1c2d3e-0000-4000-8000-abcdefabcdef",
                "search == \"nil\"",
                Some("abc/="),
            )
            .unwrap();

        let requests = received(&recorded);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].target,
            "/server-management/v1/universes/1/places/2/versions/45/game-servers/\
             6f1c2d3e-0000-4000-8000-abcdefabcdef/logs?MaxPageSize=100\
             &OrderBy=message_timestamp%20desc&Filter=search%20%3D%3D%20%22nil%22\
             &PageToken=abc%2F%3D"
        );
        assert!(requests[0].headers.contains(&format!("x-api-key: {KEY}")));

        assert_eq!(page.next_page_token.as_deref(), Some("page two/+="));
        let [error, warning] = &page.logs[..] else {
            panic!("expected two entries, got {:?}", page.logs);
        };
        assert_eq!(error.timestamp_ms, Some(1_794_052_800_123));
        assert_eq!(error.severity, LogSeverity::Error);
        assert_eq!(error.universe_id, "1");
        assert_eq!(error.place_id, "2");
        assert_eq!(error.place_version, "45");
        assert_eq!(error.skipped_count, 4);
        assert_eq!(error.stack_trace, "ServerScriptService.Main, line 10");
        assert_eq!(
            error.message_template.as_deref(),
            Some("attempt to index {thing}")
        );
        assert_eq!(error.structured_stack_trace[0]["line"], 10);
        assert_eq!(warning.severity, LogSeverity::Warning);
        assert_eq!(warning.rate_limited_count, 7);
        assert_eq!(warning.skipped_count, 0);
        assert_eq!(warning.message_template, None);
        assert_eq!(warning.structured_stack_trace, Value::Null);
    }

    #[test]
    fn empty_filters_and_tokens_are_left_out_of_log_queries() {
        assert_eq!(
            game_server_logs_query("", None),
            "?MaxPageSize=100&OrderBy=message_timestamp%20desc"
        );
        let (base, recorded) = serve(vec![reply("200 OK", "{}")]);
        let page = client(&base)
            .list_game_server_logs(1, 2, "3", "job", "", None)
            .unwrap();
        assert!(page.logs.is_empty());
        assert_eq!(page.next_page_token, None);
        assert!(received(&recorded)[0]
            .target
            .ends_with("/game-servers/job/logs?MaxPageSize=100&OrderBy=message_timestamp%20desc"));
    }

    #[test]
    fn game_servers_and_place_versions_are_listed() {
        let (base, recorded) = serve(vec![
            reply(
                "200 OK",
                r#"{"filters":{"PlaceVersion":{"field":"PlaceVersion","type":"Number","values":[44,"45",null,"../x"]}}}"#,
            ),
            reply(
                "200 OK",
                r#"{"gameServers":[
                    {"placeId":"2","placeVersion":"45","jobId":"a-1","shutDown":false,"terminationTime":null},
                    {"placeId":"2","placeVersion":45,"jobId":"b-2","shutDown":true,"terminationTime":"2026-11-07T10:00:00Z"},
                    {"placeId":"2","placeVersion":"45"}
                ],"nextPageToken":""}"#,
            ),
        ]);
        let client = client(&base);
        assert_eq!(client.place_versions(1, 2).unwrap(), ["44", "45"]);
        let page = client.list_game_servers(1, 2, "45", Some("t")).unwrap();
        assert_eq!(
            page.servers,
            [
                GameServer {
                    job_id: "a-1".into(),
                    place_version: "45".into(),
                    shut_down: false,
                    termination_time: None,
                },
                GameServer {
                    job_id: "b-2".into(),
                    place_version: "45".into(),
                    shut_down: true,
                    termination_time: Some("2026-11-07T10:00:00Z".into()),
                },
            ]
        );
        assert_eq!(page.next_page_token, None);
        let requests = received(&recorded);
        assert_eq!(
            requests[0].target,
            "/server-management/v1/universes/1/places/2/game-servers:filter-options?Filter=PlaceVersion"
        );
        assert_eq!(
            requests[1].target,
            "/server-management/v1/universes/1/places/2/versions/45/game-servers?MaxPageSize=100&PageToken=t"
        );
    }

    #[test]
    fn null_lists_read_as_empty_and_fetch_errors_mark_pages_incomplete() {
        let (base, _) = serve(vec![
            reply("200 OK", r#"{"gameServers":null,"nextPageToken":null}"#),
            reply("200 OK", r#"{"gameServerLogs":null,"nextPageToken":null}"#),
            reply(
                "200 OK",
                r#"{"gameServers":[{"jobId":"a"}],"activeServersFetchError":false,"shutdownServersFetchError":true}"#,
            ),
            reply(
                "200 OK",
                r#"{"gameServers":[],"activeServersFetchError":true,"shutdownServersFetchError":null}"#,
            ),
        ]);
        let client = client(&base);
        let page = client.list_game_servers(1, 2, "3", None).unwrap();
        assert!(page.servers.is_empty());
        assert!(!page.incomplete);
        assert_eq!(page.next_page_token, None);
        let logs = client
            .list_game_server_logs(1, 2, "3", "job", "", None)
            .unwrap();
        assert!(logs.logs.is_empty());
        let page = client.list_game_servers(1, 2, "3", None).unwrap();
        assert_eq!(page.servers.len(), 1);
        assert!(page.incomplete);
        assert!(
            client
                .list_game_servers(1, 2, "3", None)
                .unwrap()
                .incomplete
        );
    }

    #[test]
    fn unsafe_path_values_are_refused_before_sending() {
        let client = client("http://127.0.0.1:9");
        for (version, job) in [
            ("45", "../../assets"),
            ("45/x", "job"),
            ("45", "a?b"),
            ("", "j"),
        ] {
            let error = client
                .list_game_server_logs(1, 2, version, job, "", None)
                .unwrap_err();
            assert!(matches!(error, OpenCloudError::NotSent(_)), "{error}");
        }
    }

    #[test]
    fn log_severities_and_timestamps_are_read_leniently() {
        use serde_json::json;
        for (value, expected) in [
            (json!(0), LogSeverity::Output),
            (json!("1"), LogSeverity::Info),
            (json!("WARNING"), LogSeverity::Warning),
            (json!("LOG_SEVERITY_ERROR"), LogSeverity::Error),
            (json!("MessageError"), LogSeverity::Error),
            (json!(9), LogSeverity::Other("9".into())),
        ] {
            assert_eq!(LogSeverity::parse(&value), expected, "{value}");
        }
        assert_eq!(parse_timestamp_ms(&json!("12")), Some(12));
        assert_eq!(parse_timestamp_ms(&json!(12)), Some(12));
        assert_eq!(
            parse_timestamp_ms(&json!("1970-01-01T00:00:01.5Z")),
            Some(1500)
        );
        assert_eq!(parse_timestamp_ms(&json!("soon")), None);
    }

    #[test]
    fn spaced_clients_pace_every_attempt_including_retries() {
        let spacing = Duration::from_millis(150);
        let (base, recorded) = serve(vec![
            reply("503 Service Unavailable", ""),
            reply("200 OK", "{}"),
            reply("200 OK", "{}"),
        ]);
        let client = client(&base).with_request_spacing(spacing);
        // The first call is retried once after the 503; the retry's own
        // backoff (10 ms) is far shorter than the spacing.
        client
            .list_game_server_logs(1, 2, "3", "job", "", None)
            .unwrap();
        client
            .list_game_server_logs(1, 2, "3", "job", "", None)
            .unwrap();
        let requests = received(&recorded);
        assert_eq!(requests.len(), 3);
        for pair in requests.windows(2) {
            let gap = pair[1].at - pair[0].at;
            // A little slack for when the server thread records arrival.
            assert!(gap >= spacing - Duration::from_millis(20), "{gap:?}");
        }
    }

    #[test]
    fn late_wakeups_never_let_requests_through_closer_than_the_spacing() {
        let spacing = Duration::from_millis(600);
        let start = Instant::now();
        let mut clock = start;
        let mut next = None;
        let mut admitted = Vec::new();
        // A simple generator of oversleeps from 0 to 499 ms.
        let mut seed: u64 = 7;
        for _ in 0..300 {
            while let Some(wait) = admit(&mut next, clock, spacing) {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                clock += wait + Duration::from_millis((seed >> 33) % 500);
            }
            admitted.push(clock);
        }
        for pair in admitted.windows(2) {
            assert!(pair[1] - pair[0] >= spacing, "{:?}", pair[1] - pair[0]);
        }
        // So no minute holds more than 100 of them.
        for (i, first) in admitted.iter().enumerate() {
            let in_minute = admitted[i..]
                .iter()
                .take_while(|at| **at - *first < Duration::from_secs(60))
                .count();
            assert!(in_minute <= 100, "{in_minute}");
        }

        // The case that used to slip through: woken 300 ms late, the next
        // slot is 600 ms after that, not 300 ms.
        let mut next = Some(start + spacing);
        assert_eq!(admit(&mut next, start, spacing), Some(spacing));
        let late = start + spacing + Duration::from_millis(300);
        assert_eq!(admit(&mut next, late, spacing), None);
        assert_eq!(
            admit(&mut next, late + Duration::from_millis(300), spacing),
            Some(Duration::from_millis(300))
        );
    }

    #[test]
    fn refused_log_requests_report_the_status() {
        let (base, recorded) = serve(vec![reply(
            "403 Forbidden",
            r#"{"title":"Forbidden","status":403}"#,
        )]);
        let error = client(&base)
            .list_game_server_logs(1, 2, "3", "job", "", None)
            .unwrap_err();
        match &error {
            OpenCloudError::Status { status, .. } => assert_eq!(status.as_u16(), 403),
            other => panic!("unexpected error {other}"),
        }
        assert_eq!(received(&recorded).len(), 1);
    }

    fn tiny_png() -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &byte in data {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        fn chunk(png: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            png.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let start = png.len();
            png.extend_from_slice(kind);
            png.extend_from_slice(data);
            let crc = crc32(&png[start..]);
            png.extend_from_slice(&crc.to_be_bytes());
        }
        let mut raw = Vec::new();
        for y in 0..8u8 {
            raw.push(0); // No filter.
            for x in 0..8u8 {
                raw.extend_from_slice(&[x * 32, y * 32, 200]);
            }
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &raw {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        let mut zlib = vec![0x78, 0x01, 0x01];
        zlib.extend_from_slice(&(raw.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(&raw);
        zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = Vec::new();
        header.extend_from_slice(&8u32.to_be_bytes());
        header.extend_from_slice(&8u32.to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB.
        chunk(&mut png, b"IHDR", &header);
        chunk(&mut png, b"IDAT", &zlib);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn tiny_png_has_a_valid_crc() {
        let png = tiny_png();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        // The IEND chunk's CRC is fixed by the PNG specification.
        assert!(png.ends_with(&[0xAE, 0x42, 0x60, 0x82]));
    }

    fn live_env(name: &str) -> u64 {
        std::env::var(name)
            .unwrap_or_else(|_| panic!("set {name}"))
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a number"))
    }

    /// Talks to real Open Cloud. Run with `cargo test -- --ignored live_` and
    /// ROBLOX_API_KEY, ROJO_TEST_UNIVERSE_ID, ROJO_TEST_PLACE_ID,
    /// ROJO_TEST_CREATOR_USER_ID and ROJO_TEST_PLACE_FILE (a built .rbxl or
    /// .rbxlx) set. It saves (never publishes) a place version to the given
    /// test place, runs a Luau task against it and, only when
    /// ROJO_TEST_UPLOAD_ASSET=1, uploads one 8x8 image. Point it only at a
    /// disposable test experience.
    #[test]
    #[ignore = "uses real Open Cloud credentials"]
    fn live_open_cloud() {
        let universe = live_env("ROJO_TEST_UNIVERSE_ID");
        let place = live_env("ROJO_TEST_PLACE_ID");
        let client = OpenCloud::from_env(DEFAULT_API_KEY_VAR).unwrap();
        let place_file = std::env::var("ROJO_TEST_PLACE_FILE").expect("set ROJO_TEST_PLACE_FILE");
        let bytes = std::fs::read(&place_file).unwrap();
        let format = if place_file.ends_with(".rbxlx") {
            PlaceFormat::Xml
        } else {
            PlaceFormat::Binary
        };

        // A place open in a Team Create session rejects saves with 409, so a
        // failed save is reported and the rest still runs on the current
        // version before the test fails.
        let publish = client.publish_place(universe, place, &bytes, format, VersionType::Saved);
        let version = match &publish {
            Ok(version) => {
                println!(
                    "live: saved place version {version} ({} bytes)",
                    bytes.len()
                );
                Some(*version)
            }
            Err(error) => {
                println!("live: saving the place failed: {error}");
                None
            }
        };

        let script = r#"
            print("rojo-open-cloud live log line")
            warn("rojo-open-cloud live warning")
            local script = game:GetService("ServerScriptService"):FindFirstChildWhichIsA("Script", true)
            return { answer = 42, placeVersion = game.PlaceVersion, script = script and script.Name or nil }
        "#;
        let result = client
            .run_luau(universe, place, version, script, Duration::from_secs(60))
            .unwrap();
        println!("live: task {} finished {:?}", result.path, result.state);
        println!(
            "live: results {}",
            serde_json::to_string(&result.results).unwrap()
        );
        println!(
            "live: error {:?} {:?}",
            result.error_code, result.error_message
        );
        for log in &result.logs {
            println!("live: log [{}] {}", log.kind, log.message);
        }
        assert_eq!(result.state, TaskState::Complete);
        assert_eq!(result.results[0]["answer"], 42);
        assert!(result
            .logs
            .iter()
            .any(|log| log.message.contains("rojo-open-cloud live log line")));

        if std::env::var("ROJO_TEST_UPLOAD_ASSET").as_deref() == Ok("1") {
            let creator = live_env("ROJO_TEST_CREATOR_USER_ID");
            let asset = client
                .upload_asset(
                    AssetType::Image,
                    "rojo-open-cloud-test",
                    "Uploaded by Rojo's Open Cloud client test",
                    Creator::User(creator),
                    "rojo-open-cloud-test.png",
                    "image/png",
                    &tiny_png(),
                )
                .unwrap();
            println!("live: uploaded asset {asset:?}");
        }
        // Checks that an uploaded image id loads as an ImageLabel's image.
        if let Ok(id) = std::env::var("ROJO_TEST_CHECK_IMAGE_ID") {
            let id: u64 = id
                .parse()
                .expect("ROJO_TEST_CHECK_IMAGE_ID must be a number");
            let script = format!(
                r#"
                local label = Instance.new("ImageLabel")
                label.Image = "rbxassetid://{id}"
                local status
                game:GetService("ContentProvider"):PreloadAsync({{ label }}, function(_, s) status = s.Name end)
                return status, label.IsLoaded
                "#
            );
            let result = client
                .run_luau(universe, place, version, &script, Duration::from_secs(60))
                .unwrap();
            println!(
                "live: image {id} preload {:?} {} {:?}",
                result.state,
                serde_json::to_string(&result.results).unwrap(),
                result.error_message
            );
        }
        publish.unwrap();
    }
}
