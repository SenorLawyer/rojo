//! Keep source revisions and immutable Studio snapshots together without changing
//! the user's primary checkout. Place bytes are archived, never reconstructed.
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    io::{self, Read as _, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, ensure, Context};
use clap::Parser;
use fs_err as fs;
use memofs::Vfs;
use rbx_dom_weak::{types::Variant, WeakDom};
use serde::{Deserialize, Serialize};

use crate::{
    open_cloud::{LuauTaskResult, OpenCloud, OpenCloudError, TaskState},
    serve_session::ServeSession,
};

const MAX_PLACE_BYTES: u64 = 512 * 1024 * 1024;

/// Hashes only catch accidental corruption: whoever produced a bundle also wrote
/// the hash in its manifest, so it says nothing about who made the bundle.
const IMPORT_TRUST_WARNING: &str =
    "Bundle integrity is self-attested; import only bundles from trusted sources.";

/// Archive Studio-saved places with the Git revision whose scripts they contain.
#[derive(Debug, Parser)]
pub struct CheckpointCommand {
    /// Output is always structured JSON; accepted explicitly for agent scripts.
    #[clap(long, global = true)]
    json: bool,
    #[clap(subcommand)]
    command: CheckpointAction,
}

#[derive(Debug, Parser)]
enum CheckpointAction {
    /// Archive a Studio-saved place with the clean Git revision whose scripts it contains.
    Create {
        name: String,
        /// Native Studio-saved .rbxl or .rbxlx. Runtime Play copies are not checkpoints.
        #[clap(long)]
        place: PathBuf,
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
    /// List immutable checkpoints shared by this repository's worktrees.
    List {
        /// Also re-hash every archived place and report whether it is intact.
        #[clap(long)]
        verify: bool,
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
    /// Copy a verified checkpoint to a new file. Never overwrites an existing file.
    Export {
        name: String,
        #[clap(long)]
        output: PathBuf,
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
    /// Export a portable directory containing the exact place and its manifest.
    Pack {
        name: String,
        #[clap(long)]
        output: PathBuf,
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
    /// Import a checkpoint directory after verifying its bytes and Git revision.
    Import {
        #[clap(long)]
        input: PathBuf,
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
}

/// Create and manage sibling Git worktrees for updates and hotfixes, started from checkpoints.
#[derive(Debug, Parser)]
pub struct WorkspaceCommand {
    /// Output is always structured JSON; accepted explicitly for agent scripts.
    #[clap(long, global = true)]
    json: bool,
    #[clap(subcommand)]
    command: WorkspaceAction,
}

#[derive(Debug, Parser)]
enum WorkspaceAction {
    /// Create a sibling Git worktree and a copy of the matching checkpoint place.
    Create {
        name: String,
        #[clap(long, required_unless_present = "release", conflicts_with = "release")]
        checkpoint: Option<String>,
        /// Use a named release, or 'live' for the last publication recorded by Rojo.
        #[clap(long)]
        release: Option<String>,
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
    /// List Git worktrees and their associated Studio place files.
    List {
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
    /// Open this managed workspace's saved place in Studio.
    Open {
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
    /// Bring a source commit forward. Studio content must be merged separately with `rojo merge`.
    Apply {
        commit: String,
        /// Acknowledge that the workspace's saved place is not changed by this command.
        #[clap(long, required = true)]
        code_only: bool,
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
}

/// Prepare, publish, roll back and record releases of archived checkpoints to Roblox.
#[derive(Debug, Parser)]
pub struct ReleaseCommand {
    /// Output is always structured JSON; accepted explicitly for agent scripts.
    #[clap(long, global = true)]
    json: bool,
    #[clap(subcommand)]
    command: ReleaseAction,
}

#[derive(Debug, Parser)]
enum ReleaseAction {
    /// Associate an immutable checkpoint with an explicit Roblox deployment target.
    Prepare {
        name: String,
        #[clap(long)]
        checkpoint: String,
        #[clap(long)]
        universe_id: u64,
        #[clap(long)]
        place_id: u64,
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
    /// Publish exact archived bytes via Open Cloud. Does not restart existing servers.
    Publish(PublishOptions),
    /// Republish an earlier release's exact bytes. Does not revert player data or restart servers.
    Rollback(RollbackOptions),
    /// Publish a release's exact bytes to a separate test place, then optionally
    /// run a Luau smoke check against that version. Never touches the live target.
    Stage(StageOptions),
    /// Publish a release live, like `publish`, but only after a recent passing
    /// `release stage` of the same bytes.
    Promote(PromoteOptions),
    /// List releases and Rojo's last recorded publication, not a live Roblox status query.
    List {
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
    /// Record the outcome of a publication that Rojo could not confirm or record,
    /// after checking Roblox version history, and release its lock.
    Resolve {
        /// The attempt ID named in the error and in publication-attempts.
        attempt: String,
        /// The place version that Roblox version history shows for this attempt.
        #[clap(
            long,
            required_unless_present = "not-published",
            conflicts_with = "not-published"
        )]
        version: Option<u64>,
        /// Roblox version history shows that this attempt created no version.
        #[clap(long)]
        not_published: bool,
        /// Resolve a pending attempt even though its lock is recent enough that the
        /// publishing process may still be uploading. Check that it has exited first.
        #[clap(long)]
        force: bool,
        #[clap(long, default_value = ".")]
        project: PathBuf,
    },
}

#[derive(Debug, Parser)]
struct PublishOptions {
    name: String,
    /// Explicit UNIVERSE_ID:PLACE_ID acknowledgement. Must match the prepared release.
    #[clap(long)]
    confirm_target: String,
    /// Name of the environment variable containing an Open Cloud API key.
    #[clap(long, default_value = "ROBLOX_API_KEY")]
    api_key_env: String,
    #[clap(long, default_value = ".")]
    project: PathBuf,
}

#[derive(Debug, Parser)]
struct StageOptions {
    name: String,
    /// The test place as UNIVERSE_ID:PLACE_ID. Must not be the release's own place.
    #[clap(long)]
    staging: String,
    /// A Luau script run with Luau Execution against the staged version. It
    /// passes by returning `true` or a table with `ok = true`.
    #[clap(long)]
    smoke: Option<PathBuf>,
    /// Seconds the smoke script may run, 1 to 300.
    #[clap(long, requires = "smoke")]
    smoke_timeout: Option<u64>,
    /// Name of the environment variable containing an Open Cloud API key.
    #[clap(long, default_value = "ROBLOX_API_KEY")]
    api_key_env: String,
    #[clap(long, default_value = ".")]
    project: PathBuf,
}

#[derive(Debug, Parser)]
struct PromoteOptions {
    #[clap(flatten)]
    publish: PublishOptions,
    /// Refuse staging records older than this many hours.
    #[clap(long, default_value = "24")]
    max_age_hours: u64,
    /// Accept a staging record that ran no smoke check.
    #[clap(long)]
    allow_unsmoked: bool,
}

#[derive(Debug, Parser)]
struct RollbackOptions {
    #[clap(flatten)]
    publish: PublishOptions,
    /// Republish a release that has no successful publication receipt in this repository.
    #[clap(long)]
    force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Checkpoint {
    schema_version: u32,
    name: String,
    source_commit: String,
    /// `/`-separated path inside the repository, the form Git uses on every OS.
    project_file: String,
    place_file: String,
    place_hash: String,
    place_bytes: u64,
    source_scripts_verified: usize,
    package_files: BTreeMap<String, String>,
    rojo_version: String,
    created_unix: u64,
    /// Set when the checkpoint came from `checkpoint import` rather than a local save.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    imported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    import_provenance: Option<ImportProvenance>,
    /// Set when a project `$path` resolved outside the repository, so the
    /// recorded commit does not pin all of the checked source.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    sources_outside_repository: bool,
}

/// What this repository checked when it imported a bundle. The place hash and
/// manifest are the bundle author's own claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImportProvenance {
    imported_unix: u64,
    /// The bundle's directory name only; its local location is not recorded.
    bundle: String,
    integrity: String,
    source_commit_verified: bool,
    project_file_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Release {
    schema_version: u32,
    name: String,
    checkpoint: String,
    checkpoint_hash: String,
    universe_id: u64,
    place_id: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Workspace {
    name: String,
    root: PathBuf,
    place: PathBuf,
    checkpoint: String,
    source_commit: String,
    project_file: String,
}

struct Repository {
    root: PathBuf,
    git_dir: PathBuf,
    state: PathBuf,
    primary: PathBuf,
    /// The primary worktree is a bare repository, so `primary` is its Git directory.
    primary_bare: bool,
}

impl Repository {
    fn open(project: &Path) -> anyhow::Result<Self> {
        let input = dunce::canonicalize(project).context("Project path does not exist")?;
        let directory = if input.is_dir() {
            input.as_path()
        } else {
            input.parent().unwrap()
        };
        let root = dunce::canonicalize(git(directory, ["rev-parse", "--show-toplevel"])?)?;
        let git_dir = PathBuf::from(git(directory, ["rev-parse", "--absolute-git-dir"])?);
        let common = git_common_dir(directory)?;
        let worktrees = git(directory, ["worktree", "list", "--porcelain"])?;
        // The first block always describes the primary worktree.
        let worktrees = worktrees.replace("\r\n", "\n");
        let first: Vec<&str> = worktrees
            .split("\n\n")
            .next()
            .unwrap_or_default()
            .lines()
            .collect();
        let primary = first
            .iter()
            .find_map(|s| s.strip_prefix("worktree "))
            .map(PathBuf::from)
            .context("Cannot locate the primary Git worktree")?;
        Ok(Self {
            root,
            git_dir,
            state: common.join("rojo-state"),
            primary,
            primary_bare: first.contains(&"bare"),
        })
    }

    /// `hint` says what to do once the tree is clean.
    fn clean_head(&self, hint: &str) -> anyhow::Result<String> {
        // `submodule.<name>.ignore` and `diff.ignoreSubmodules` can hide changes
        // inside submodules from a plain status, but they still change the build.
        let status = git(
            &self.root,
            [
                "status",
                "--porcelain",
                "--untracked-files=normal",
                "--ignore-submodules=none",
            ],
        )?;
        ensure!(
            status.is_empty(),
            "Commit or move pending source changes first, including changes inside submodules. {hint}"
        );
        git(&self.root, ["rev-parse", "--verify", "HEAD"])
    }

    /// A 40-digit ID in a SHA-256 repository would be read as an abbreviation,
    /// which could match a different commit.
    fn ensure_full_commit_id(&self, id: &str) -> anyhow::Result<()> {
        let format = git(&self.root, ["rev-parse", "--show-object-format"])?;
        let length = match format.as_str() {
            "sha1" => 40,
            "sha256" => 64,
            other => bail!("Unsupported Git object format {other}"),
        };
        ensure!(
            id.len() == length && is_commit_id(id),
            "Checkpoint commit {id} is not a full {format} commit ID for this repository"
        );
        Ok(())
    }

    /// Git only keeps commits that a ref reaches. Without this ref, a checkpoint
    /// of a detached commit, or of a branch that is later amended, rebased or
    /// deleted, loses its source once the reflog expires and `git gc` prunes it.
    /// Returns whether this call created the ref.
    fn protect_checkpoint_commit(&self, name: &str, commit: &str) -> anyhow::Result<bool> {
        self.ensure_full_commit_id(commit)?;
        let reference = checkpoint_ref(name);
        // An empty old value makes Git refuse to replace an existing ref.
        let error = match git(
            &self.root,
            [
                "update-ref",
                "-m",
                "rojo checkpoint",
                &reference,
                commit,
                "",
            ],
        ) {
            Ok(_) => return Ok(true),
            Err(error) => error,
        };
        let existing = git(
            &self.root,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ],
        )
        .map_err(|_| error.context(format!("Cannot create {reference}")))?;
        ensure!(
            existing.eq_ignore_ascii_case(commit),
            "{reference} points at {existing}, not this checkpoint's commit {commit}. If that ref is stale, delete it with git update-ref -d {reference}."
        );
        Ok(false)
    }

    /// Undoes `protect_checkpoint_commit` after a failed checkpoint write, but
    /// only while the ref still points at the commit it was created for.
    fn unprotect_checkpoint_commit(&self, name: &str, commit: &str) {
        let reference = checkpoint_ref(name);
        if let Err(error) = git(&self.root, ["update-ref", "-d", &reference, commit]) {
            log::warn!("Could not remove {reference}: {error:#}");
        }
    }

    /// Protects the checkpoint's commit, then archives it at `destination`.
    /// On failure the ref is removed only if this call created it and no
    /// concurrent create or import of the same name archived the same commit
    /// there in the meantime, since that checkpoint relies on the ref.
    fn archive_protected_checkpoint(
        &self,
        destination: &Path,
        checkpoint: &Checkpoint,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        let created_ref =
            self.protect_checkpoint_commit(&checkpoint.name, &checkpoint.source_commit)?;
        let Err(error) = archive_checkpoint(destination, checkpoint, bytes) else {
            return Ok(());
        };
        let archived_by_another = read_manifest::<Checkpoint>(&destination.join("manifest.json"))
            .is_ok_and(|existing| {
                existing
                    .source_commit
                    .eq_ignore_ascii_case(&checkpoint.source_commit)
            });
        if created_ref && !archived_by_another {
            self.unprotect_checkpoint_commit(&checkpoint.name, &checkpoint.source_commit);
        }
        Err(error)
    }

    /// Where workspaces of this repository go: beside the primary checkout, or
    /// beside a bare repository under its name without `.git`.
    fn workspaces_directory(&self) -> anyhow::Result<PathBuf> {
        workspaces_directory(&self.primary, self.primary_bare)
            .context("Cannot place workspaces beside the primary worktree")
    }

    fn checkpoint(&self, name: &str) -> anyhow::Result<(Checkpoint, Vec<u8>)> {
        validate_name(name)?;
        let directory = self.state.join("checkpoints").join(name);
        let mut manifest: Checkpoint = read_json(&directory.join("manifest.json"))?;
        ensure!(manifest.schema_version == 1, "Invalid checkpoint manifest");
        ensure!(
            manifest.name == name,
            "Checkpoint manifest names {} but is stored as {name}",
            manifest.name
        );
        // The commit later reaches Git as a positional argument.
        ensure!(
            is_commit_id(&manifest.source_commit),
            "Invalid Git commit in checkpoint manifest"
        );
        manifest.project_file = normalize_repository_path(&manifest.project_file)?;
        ensure!(
            matches!(manifest.place_file.as_str(), "place.rbxl" | "place.rbxlx"),
            "Invalid checkpoint file name"
        );
        let bytes = read_place_bytes(&directory.join(&manifest.place_file))?;
        ensure!(
            bytes.len() as u64 == manifest.place_bytes && hash(&bytes) == manifest.place_hash,
            "Checkpoint integrity check failed; refusing to use modified place bytes"
        );
        Ok((manifest, bytes))
    }

    /// Resolves a release name, `live` or `live:UNIVERSE_ID:PLACE_ID`, and returns
    /// it with its verified checkpoint and place bytes so callers use exactly
    /// what was checked.
    fn release(&self, name: &str) -> anyhow::Result<(Release, Checkpoint, Vec<u8>)> {
        let live_target = live_target(name)?;
        let name = match live_target {
            Some(target) => {
                let record = self.live_record(target)?;
                record["release"]
                    .as_str()
                    .context("No release in last-publication record")?
                    .to_owned()
            }
            None => name.to_owned(),
        };
        validate_name(&name)?;
        let release: Release =
            read_json(&self.state.join("releases").join(format!("{name}.json")))?;
        ensure!(
            release.schema_version == 1 && release.name == name,
            "Invalid release manifest"
        );
        if let Some(Some((universe_id, place_id))) = live_target {
            ensure!(
                (release.universe_id, release.place_id) == (universe_id, place_id),
                "Release {name} targets {}:{}, not {universe_id}:{place_id}",
                release.universe_id,
                release.place_id
            );
        }
        let (checkpoint, bytes) = self.checkpoint(&release.checkpoint)?;
        ensure!(
            release.checkpoint_hash == checkpoint.place_hash,
            "Release checkpoint no longer matches its recorded hash"
        );
        Ok((release, checkpoint, bytes))
    }

    /// The last recorded publication per `(universe, place)` target.
    /// `last-publication.json` predates per-target records, so it only counts
    /// for a target that has no record of its own.
    fn live_records(&self) -> anyhow::Result<BTreeMap<(u64, u64), serde_json::Value>> {
        let mut records = BTreeMap::new();
        for record in list_manifests(&self.state.join("live"), false) {
            if let Some(target) = record_target(&record) {
                records.insert(target, record);
            }
        }
        // A damaged legacy record must not hide the per-target ones.
        if let Some(record) = read_legacy_publication(&self.state) {
            if let Some(target) = record_target(&record) {
                records.entry(target).or_insert(record);
            }
        }
        Ok(records)
    }

    /// `target` is `None` for a bare `live`, which is only unambiguous while a
    /// single target has publications.
    fn live_record(&self, target: Option<(u64, u64)>) -> anyhow::Result<serde_json::Value> {
        let mut records = self.live_records()?;
        match target {
            Some(target) => records.remove(&target).with_context(|| {
                format!(
                    "Rojo has no recorded publication for {}:{}",
                    target.0, target.1
                )
            }),
            None => {
                ensure!(
                    !records.is_empty(),
                    "Rojo has no recorded publication in this repository yet"
                );
                let targets: Vec<_> = records
                    .keys()
                    .map(|(universe, place)| format!("live:{universe}:{place}"))
                    .collect();
                ensure!(
                    records.len() == 1,
                    "Several places have recorded publications; choose one with {}",
                    targets.join(", ")
                );
                Ok(records.into_values().next().unwrap())
            }
        }
    }
}

/// Parses `live` (`Some(None)`) or `live:UNIVERSE_ID:PLACE_ID`; anything else
/// is a release name (`None`).
fn live_target(name: &str) -> anyhow::Result<Option<Option<(u64, u64)>>> {
    if name.eq_ignore_ascii_case("live") {
        return Ok(Some(None));
    }
    let Some(target) = name
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("live:"))
        .map(|_| &name[5..])
    else {
        return Ok(None);
    };
    let parsed = target
        .split_once(':')
        .and_then(|(universe, place)| Some((universe.parse().ok()?, place.parse().ok()?)));
    match parsed {
        Some(target) => Ok(Some(Some(target))),
        None => bail!("Use live:UNIVERSE_ID:PLACE_ID to choose a published place"),
    }
}

fn record_target(record: &serde_json::Value) -> Option<(u64, u64)> {
    Some((
        record["universe_id"].as_u64()?,
        record["place_id"].as_u64()?,
    ))
}

fn live_record_path(state: &Path, universe_id: u64, place_id: u64) -> PathBuf {
    state
        .join("live")
        .join(format!("{universe_id}-{place_id}.json"))
}

/// Reads `last-publication.json`, warning about and skipping a damaged file.
fn read_legacy_publication(state: &Path) -> Option<serde_json::Value> {
    let path = state.join("last-publication.json");
    if !path.exists() {
        return None;
    }
    read_json(&path)
        .map_err(|error| log::warn!("Skipping {}: {error:#}", path.display()))
        .ok()
}

fn checkpoint_ref(name: &str) -> String {
    format!("refs/rojo/checkpoints/{name}")
}

/// `primary` is the primary worktree, or the repository itself when it is bare.
fn workspaces_directory(primary: &Path, bare: bool) -> Option<PathBuf> {
    let name = primary.file_name()?.to_str()?;
    let (parent, name) = match (bare, name.strip_suffix(".git")) {
        // A bare repository inside a checkout-like directory, as in `game/.git`.
        (true, Some("")) => {
            let checkout = primary.parent()?;
            (checkout.parent()?, checkout.file_name()?.to_str()?)
        }
        (true, Some(stem)) => (primary.parent()?, stem),
        _ => (primary.parent()?, name),
    };
    Some(parent.join(format!("{name}.workspaces")))
}

impl CheckpointCommand {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            CheckpointAction::Create {
                name,
                place,
                project,
            } => {
                validate_name(&name)?;
                let repo = Repository::open(&project)?;
                let head = repo.clean_head(CHECKPOINT_CLEAN_HINT)?;
                let input = dunce::canonicalize(&project)?;
                let project_file = if input.is_dir() {
                    crate::project::DEFAULT_PROJECT_NAMES
                        .iter()
                        .map(|name| input.join(name))
                        .find(|path| path.is_file())
                        .context("No default.project.json or default.project.jsonc found")?
                } else {
                    input
                };
                let relative_project = repository_path(
                    project_file
                        .strip_prefix(&repo.root)
                        .context("Rojo project must be inside this Git checkout")?,
                )?;
                let bytes = read_place_bytes(&place)?;
                let extension = place_extension(&place)?;
                let dom = decode_place(&bytes, extension)?;
                let (verified, source_paths) = verify_sources(&project_file, &dom)?;
                ensure!(
                    head == repo.clean_head(CHECKPOINT_CLEAN_HINT)?,
                    "Git changed while capturing the checkpoint; retry"
                );
                let outside = paths_outside(&repo.root, &source_paths);
                if !outside.is_empty() {
                    let listed: Vec<_> = outside.iter().map(|p| p.display().to_string()).collect();
                    log::warn!(
                        "The project reads {} from outside the repository. The checkpoint's Git commit does not record that source, so restoring the checkpoint may build different code.",
                        listed.join(", ")
                    );
                }
                let package_files = package_file_hashes(&repo.root, &project_file)?;
                let checkpoint = Checkpoint {
                    schema_version: 1,
                    name: name.clone(),
                    source_commit: head,
                    project_file: relative_project,
                    place_file: format!("place.{extension}"),
                    place_hash: hash(&bytes),
                    place_bytes: bytes.len() as u64,
                    source_scripts_verified: verified,
                    package_files,
                    rojo_version: env!("CARGO_PKG_VERSION").to_owned(),
                    created_unix: now(),
                    imported: false,
                    import_provenance: None,
                    sources_outside_repository: !outside.is_empty(),
                };
                let parent = repo.state.join("checkpoints");
                fs::create_dir_all(&parent)?;
                let destination = parent.join(&name);
                // Before the existence check, which a case-insensitive file
                // system would answer for the other name.
                ensure_no_case_conflict("checkpoint", &name, existing_names(&parent, None))?;
                ensure!(
                    !destination.exists(),
                    "Checkpoint already exists; choose a new name"
                );
                repo.archive_protected_checkpoint(&destination, &checkpoint, &bytes)
                    .context("Cannot finalize immutable checkpoint")?;
                print_json(&checkpoint)
            }
            CheckpointAction::List { verify, project } => {
                let repo = Repository::open(&project)?;
                print_json(&list_checkpoints(&repo, verify))
            }
            CheckpointAction::Export {
                name,
                output,
                project,
            } => {
                let repo = Repository::open(&project)?;
                let (checkpoint, bytes) = repo.checkpoint(&name)?;
                ensure!(
                    place_extension(&output)?
                        == place_extension(Path::new(&checkpoint.place_file))?,
                    "Output extension must match the archived place format"
                );
                write_new(&output, &bytes)?;
                print_json(
                    &serde_json::json!({"output": output, "checkpoint": checkpoint.name, "hash": checkpoint.place_hash}),
                )
            }
            CheckpointAction::Pack {
                name,
                output,
                project,
            } => {
                let repo = Repository::open(&project)?;
                let (checkpoint, bytes) = repo.checkpoint(&name)?;
                archive_checkpoint(&output, &checkpoint, &bytes)?;
                print_json(&serde_json::json!({"checkpoint": name, "bundle": output}))
            }
            CheckpointAction::Import { input, project } => {
                let repo = Repository::open(&project)?;
                let checkpoint = import_checkpoint(&repo, &input)?;
                log::warn!("{IMPORT_TRUST_WARNING}");
                let mut output = serde_json::to_value(&checkpoint)?;
                output["warning"] = IMPORT_TRUST_WARNING.into();
                print_json(&output)
            }
        }
    }
}

fn import_checkpoint(repo: &Repository, input: &Path) -> anyhow::Result<Checkpoint> {
    let mut checkpoint: Checkpoint = read_manifest(&input.join("manifest.json"))?;
    validate_name(&checkpoint.name)?;
    ensure!(
        checkpoint.schema_version == 1
            && matches!(checkpoint.place_file.as_str(), "place.rbxl" | "place.rbxlx"),
        "Invalid checkpoint manifest"
    );
    ensure!(
        is_commit_id(&checkpoint.source_commit),
        "Invalid Git commit in checkpoint"
    );
    repo.ensure_full_commit_id(&checkpoint.source_commit)?;
    // Bundles packed on Windows by earlier versions store `\` separators.
    checkpoint.project_file = normalize_repository_path(&checkpoint.project_file)?;
    let parent = repo.state.join("checkpoints");
    let destination = parent.join(&checkpoint.name);
    ensure_no_case_conflict(
        "checkpoint",
        &checkpoint.name,
        existing_names(&parent, None),
    )?;
    ensure!(
        !destination.exists(),
        "Checkpoint {} already exists in this repository; it was not overwritten",
        checkpoint.name
    );
    git(
        &repo.root,
        [
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", checkpoint.source_commit),
        ],
    )
    .context("Checkpoint source commit is not available; fetch the matching Git history first")?;
    let project_path = &checkpoint.project_file;
    git(
        &repo.root,
        [
            "cat-file",
            "-e",
            &format!("{}:{project_path}", checkpoint.source_commit),
        ],
    )
    .with_context(|| {
        format!("Project file {project_path} does not exist at the checkpoint's source commit")
    })?;
    let bytes = read_place_bytes(&input.join(&checkpoint.place_file))?;
    ensure!(
        hash(&bytes) == checkpoint.place_hash && bytes.len() as u64 == checkpoint.place_bytes,
        "Checkpoint integrity check failed"
    );
    decode_place(&bytes, place_extension(Path::new(&checkpoint.place_file))?)?;
    checkpoint.imported = true;
    checkpoint.import_provenance = Some(ImportProvenance {
        imported_unix: now(),
        bundle: dunce::canonicalize(input)
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default(),
        integrity: "self-attested".to_owned(),
        source_commit_verified: true,
        project_file_verified: true,
    });
    fs::create_dir_all(&parent)?;
    repo.archive_protected_checkpoint(&destination, &checkpoint, &bytes)?;
    Ok(checkpoint)
}

/// Manifests are small; a cap keeps a damaged or hostile bundle from making
/// import read an arbitrarily large file into memory.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

fn read_manifest<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Cannot read {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "{} is larger than 1 MiB and is not a checkpoint manifest",
        path.display()
    );
    serde_json::from_slice(&bytes).with_context(|| format!("Invalid JSON in {}", path.display()))
}

/// SHA-1 repositories use 40 hex digits; SHA-256 repositories use 64.
fn is_commit_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|c| c.is_ascii_hexdigit())
}

impl WorkspaceCommand {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            WorkspaceAction::Create {
                name,
                checkpoint,
                release,
                project,
            } => {
                let repo = Repository::open(&project)?;
                let checkpoint_name = match (checkpoint, release) {
                    (Some(name), None) => name,
                    (None, Some(name)) => repo.release(&name)?.0.checkpoint,
                    _ => bail!("Choose exactly one --checkpoint or --release"),
                };
                let (workspace, warnings) = create_workspace(&repo, &name, &checkpoint_name)?;
                let mut output = serde_json::to_value(&workspace)?;
                if !warnings.is_empty() {
                    output["warnings"] = warnings.into();
                }
                print_json(&output)
            }
            WorkspaceAction::List { project } => {
                let repo = Repository::open(&project)?;
                print_json(&list_workspaces(&repo)?)
            }
            WorkspaceAction::Open { project } => {
                let repo = Repository::open(&project)?;
                let (place, project) = workspace_place(&repo)?;
                opener::open(&place).context("Could not open the workspace place in Studio")?;
                print_json(&serde_json::json!({"opened": place, "project": project}))
            }
            WorkspaceAction::Apply {
                commit,
                code_only: _,
                project,
            } => {
                let repo = Repository::open(&project)?;
                repo.clean_head("workspace apply only cherry-picks onto a clean tree.")?;
                let revision = git(
                    &repo.root,
                    [
                        "rev-parse",
                        "--verify",
                        "--end-of-options",
                        &format!("{commit}^{{commit}}"),
                    ],
                )?;
                // Cherry-pick also fails without conflicts, for example on a
                // merge commit or a change that is already applied.
                git(&repo.root, ["cherry-pick", "--no-edit", &revision]).with_context(|| format!(
                    "git cherry-pick {revision} did not complete. If it stopped on conflicts, resolve them and run git cherry-pick --continue, or run git cherry-pick --abort to undo it. The Studio place was not changed."))?;
                print_json(
                    &serde_json::json!({"applied_commit": revision, "place_changed": false,
                    "next_step": "Merge content separately with rojo merge, sync source into the matching Studio session, and save a new checkpoint."}),
                )
            }
        }
    }
}

fn create_workspace(
    repo: &Repository,
    name: &str,
    checkpoint_name: &str,
) -> anyhow::Result<(Workspace, Vec<String>)> {
    validate_name(name)?;
    let (checkpoint, bytes) = repo.checkpoint(checkpoint_name)?;
    // Also protects checkpoints made before Rojo created these refs.
    repo.protect_checkpoint_commit(&checkpoint.name, &checkpoint.source_commit)?;
    let workspaces = repo.workspaces_directory()?;
    let target = workspaces.join(name);
    let assets = workspaces.join(".rojo").join(name);
    let mut names = existing_names(&workspaces, None);
    names.extend(existing_names(&workspaces.join(".rojo"), None));
    // Branch refs can be files on a case-insensitive file system too.
    if let Ok(branches) = git(
        &repo.root,
        [
            "for-each-ref",
            "--format=%(refname:strip=3)",
            "refs/heads/rojo/",
        ],
    ) {
        names.extend(branches.lines().map(str::to_owned));
    }
    ensure_no_case_conflict("workspace", name, names)?;
    ensure!(
        !target.exists() && !assets.exists(),
        "Workspace or its saved place already exists"
    );
    let branch = format!("rojo/{name}");
    // Knowing the branch did not exist beforehand is what makes it safe to delete
    // it again if a later step fails.
    ensure!(
        git(
            &repo.root,
            [
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}")
            ]
        )
        .is_err(),
        "Branch {branch} already exists; delete it or choose another workspace name"
    );
    fs::create_dir_all(&workspaces)?;
    // Git creates the working copy from the checkpoint revision, never
    // copies uncommitted update code from the current working directory.
    if let Err(error) = git(
        &repo.root,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-b"),
            OsStr::new(&branch),
            OsStr::new("--"),
            target.as_os_str(),
            OsStr::new(&checkpoint.source_commit),
        ],
    ) {
        // Git can create the branch before a failed checkout.
        let note = match remove_new_branch(&repo.root, &branch, &checkpoint.source_commit) {
            Ok(true) => "Workspace creation failed; the branch Git created for it was removed".to_owned(),
            Ok(false) => "Workspace creation failed before Git created its worktree or branch".to_owned(),
            Err(problem) => format!("Workspace creation failed and cleanup was incomplete ({problem}); remove the leftover branch before retrying"),
        };
        return Err(error.context(note));
    }
    let finish = || -> anyhow::Result<Workspace> {
        let created = Repository::open(&target)?;
        fs::create_dir_all(&assets)?;
        let place = assets.join(&checkpoint.place_file);
        write_new(&place, &bytes)?;
        let workspace = Workspace {
            name: name.to_owned(),
            root: target.clone(),
            place,
            checkpoint: checkpoint.name.clone(),
            source_commit: checkpoint.source_commit.clone(),
            project_file: checkpoint.project_file.clone(),
        };
        let state = created.git_dir.join("rojo");
        fs::create_dir_all(&state)?;
        write_new(
            &state.join("workspace.json"),
            &serde_json::to_vec_pretty(&workspace)?,
        )?;
        Ok(workspace)
    };
    let workspace = match finish() {
        Ok(workspace) => workspace,
        Err(error) => {
            // Only undo what this call created, so a retry with the same name works.
            let mut problems = Vec::new();
            if let Err(e) = git(
                &repo.root,
                [
                    OsStr::new("worktree"),
                    OsStr::new("remove"),
                    OsStr::new("--force"),
                    target.as_os_str(),
                ],
            ) {
                problems.push(format!("remove worktree {}: {e:#}", target.display()));
            }
            if let Err(e) = remove_new_branch(&repo.root, &branch, &checkpoint.source_commit) {
                problems.push(e);
            }
            if assets.exists() {
                if let Err(e) = fs::remove_dir_all(&assets) {
                    problems.push(format!("remove {}: {e}", assets.display()));
                }
            }
            let cleanup = if problems.is_empty() {
                Ok(())
            } else {
                Err(problems.join("; "))
            };
            return Err(error.context(cleanup_note(cleanup)));
        }
    };
    let mut warnings = Vec::new();
    // Worktrees start with empty submodule directories; the project usually
    // cannot build without them.
    if target.join(".gitmodules").is_file() {
        let mut command = git_command(&target);
        if let Some(ssh) = batch_ssh_command(&target) {
            command.env("GIT_SSH_COMMAND", ssh);
        }
        if let Err(error) = run_git(command.args(["submodule", "update", "--init", "--recursive"]))
        {
            let warning = format!(
                "Submodules were not initialized: {error:#}. Run git submodule update --init --recursive in the workspace."
            );
            log::warn!("{warning}");
            warnings.push(warning);
        }
    }
    Ok((workspace, warnings))
}

fn cleanup_note(cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => "Workspace creation failed; its new worktree and branch were removed".to_owned(),
        Err(problem) => format!("Workspace creation failed and cleanup was incomplete ({problem}); remove the leftover worktree or branch before retrying"),
    }
}

/// Deletes a branch only while it still points at the commit it was created from,
/// so commits made on it in the meantime are never discarded. Returns whether
/// there was a branch to delete.
fn remove_new_branch(root: &Path, branch: &str, commit: &str) -> Result<bool, String> {
    let reference = format!("refs/heads/{branch}");
    match git(root, ["rev-parse", "--verify", "--quiet", &reference]) {
        Err(_) => Ok(false),
        Ok(head) if head.eq_ignore_ascii_case(commit) => git(root, ["branch", "-D", branch])
            .map(|_| true)
            .map_err(|e| format!("delete branch {branch}: {e:#}")),
        Ok(_) => Err(format!("branch {branch} moved, so it was kept")),
    }
}

fn list_workspaces(repo: &Repository) -> anyhow::Result<Vec<serde_json::Value>> {
    let entries = git(&repo.root, ["worktree", "list", "--porcelain"])?;
    let mut result = Vec::new();
    // Porcelain output is one blank-line-separated block per worktree.
    for block in entries.replace("\r\n", "\n").split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let Some(directory) = lines.iter().find_map(|s| s.strip_prefix("worktree ")) else {
            continue;
        };
        if lines.contains(&"bare") {
            result
                .push(serde_json::json!({"root": directory, "status": "bare", "workspace": null}));
            continue;
        }
        // A worktree whose directory was deleted or moved is reported rather than
        // failing the whole listing; `git worktree prune` cleans these up.
        let tree = match Repository::open(Path::new(directory)) {
            Ok(tree) => tree,
            Err(error) => {
                result.push(serde_json::json!({"root": directory, "status": "missing",
                    "workspace": null, "detail": format!("{error:#}")}));
                continue;
            }
        };
        let metadata = tree.git_dir.join("rojo/workspace.json");
        let entry = if !metadata.exists() {
            serde_json::json!({"root": directory, "status": "ok", "workspace": null})
        } else {
            match read_json::<Workspace>(&metadata) {
                Ok(workspace) => {
                    serde_json::json!({"root": directory, "status": "ok", "workspace": workspace})
                }
                Err(error) => serde_json::json!({"root": directory, "status": "invalid",
                    "workspace": null, "detail": format!("{error:#}")}),
            }
        };
        result.push(entry);
    }
    Ok(result)
}

/// Returns the saved place and project file of the managed workspace at `repo`.
fn workspace_place(repo: &Repository) -> anyhow::Result<(PathBuf, PathBuf)> {
    let workspace: Workspace = read_json(&repo.git_dir.join("rojo/workspace.json"))
        .context("This checkout is not a Rojo-managed workspace")?;
    // The place is handed to the OS's default handler, so it must be a place
    // file and not, say, an executable named in an edited workspace.json.
    place_extension(&workspace.place).context("Workspace place is not a .rbxl or .rbxlx file")?;
    ensure!(workspace.place.is_file(), "Workspace place is missing");
    let project = native_path(
        &repo.root,
        &normalize_repository_path(&workspace.project_file)?,
    );
    Ok((workspace.place, project))
}

impl ReleaseCommand {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            ReleaseAction::Prepare {
                name,
                checkpoint,
                universe_id,
                place_id,
                project,
            } => {
                validate_name(&name)?;
                // Case-insensitive because release files share a directory on
                // case-insensitive file systems.
                ensure!(
                    !name.eq_ignore_ascii_case("live"),
                    "The name live is reserved for the last-publication pointers"
                );
                ensure!(
                    universe_id != 0 && place_id != 0,
                    "Universe and place IDs must be nonzero"
                );
                let repo = Repository::open(&project)?;
                let (manifest, _) = repo.checkpoint(&checkpoint)?;
                // Also protects checkpoints made before Rojo created these refs.
                repo.protect_checkpoint_commit(&manifest.name, &manifest.source_commit)?;
                let release = Release {
                    schema_version: 1,
                    name: name.clone(),
                    checkpoint,
                    checkpoint_hash: manifest.place_hash,
                    universe_id,
                    place_id,
                };
                let directory = repo.state.join("releases");
                fs::create_dir_all(&directory)?;
                ensure_no_case_conflict(
                    "release",
                    &name,
                    existing_names(&directory, Some(".json")),
                )?;
                write_new(
                    &directory.join(format!("{name}.json")),
                    &serde_json::to_vec_pretty(&release)?,
                )?;
                print_json(&release)
            }
            ReleaseAction::List { project } => {
                let repo = Repository::open(&project)?;
                let live: Vec<_> = repo.live_records()?.into_values().collect();
                print_json(
                    &serde_json::json!({"releases": list_manifests(&repo.state.join("releases"), false),
                    "last_recorded_publication": read_legacy_publication(&repo.state),
                    "live": live, "staging": list_manifests(&repo.state.join("staging"), false),
                    "queries_roblox": false}),
                )
            }
            ReleaseAction::Resolve {
                attempt,
                version,
                not_published,
                force,
                project,
            } => {
                let repo = Repository::open(&project)?;
                let outcome = match (version, not_published) {
                    (Some(version), false) => Resolution::Published(version),
                    (None, true) => Resolution::NotPublished,
                    _ => bail!("Pass exactly one of --version or --not-published"),
                };
                print_json(&resolve_attempt(&repo, &attempt, outcome, force)?)
            }
            ReleaseAction::Publish(options) => publish(options, false, false),
            ReleaseAction::Rollback(options) => publish(options.publish, true, options.force),
            ReleaseAction::Stage(options) => stage(options),
            ReleaseAction::Promote(options) => promote(options),
        }
    }
}

/// Why an upload did not return a version number. The distinction decides whether
/// releasing the publication lock is safe.
#[derive(Debug)]
enum UploadFailure {
    /// Roblox cannot have created a version: the request never reached it, or
    /// Roblox refused it with a 4xx status (including 429 rate limiting).
    Rejected(String),
    /// The request may have been applied, but no valid receipt arrived.
    Unconfirmed(String),
}

/// Reads the Open Cloud key from `var`. It is only ever sent as a request header.
fn api_key(var: &str) -> anyhow::Result<String> {
    let key = match std::env::var(var) {
        Ok(key) => key,
        Err(std::env::VarError::NotPresent) => {
            bail!("Open Cloud API key environment variable {var} is not set")
        }
        Err(std::env::VarError::NotUnicode(_)) => bail!(
            "Open Cloud API key environment variable {var} is not valid UTF-8; copy the key again"
        ),
    };
    // Copying keys from dashboards often adds a trailing newline, which is not a
    // valid header value.
    let key = key.trim().to_owned();
    ensure!(!key.is_empty(), "Open Cloud API key is empty");
    Ok(key)
}

fn publish(options: PublishOptions, rollback: bool, force: bool) -> anyhow::Result<()> {
    let key = api_key(&options.api_key_env)?;
    publish_with(options, rollback, force, |url, bytes, content_type| {
        upload_bytes(url, &key, bytes, content_type)
    })
}

fn publish_with(
    options: PublishOptions,
    rollback: bool,
    force: bool,
    upload: impl FnOnce(&str, Vec<u8>, &str) -> Result<u64, UploadFailure>,
) -> anyhow::Result<()> {
    let kind = PublishKind::Live {
        confirm_target: options.confirm_target,
        rollback,
        force,
        promote: None,
    };
    print_json(&publish_release(
        &options.project,
        &options.name,
        kind,
        upload,
    )?)
}

/// Where a publication goes. Both kinds share the lock, journal and
/// `release resolve`, so a staging upload is as recoverable as a live one.
enum PublishKind {
    /// The release's own target, which `confirm_target` must name exactly.
    Live {
        confirm_target: String,
        /// Requires an earlier receipt for this target unless `force` is set.
        rollback: bool,
        force: bool,
        /// Set for `release promote`, which first needs a passing staging record.
        promote: Option<PromoteGate>,
    },
    /// A test place other than the release's target. Writes no `live` record.
    Staging { universe_id: u64, place_id: u64 },
}

/// What `release promote` accepts as evidence that staging went well.
struct PromoteGate {
    max_age: Duration,
    allow_unsmoked: bool,
}

/// Publishes a prepared release and returns its receipt.
fn publish_release(
    project: &Path,
    name: &str,
    kind: PublishKind,
    upload: impl FnOnce(&str, Vec<u8>, &str) -> Result<u64, UploadFailure>,
) -> anyhow::Result<serde_json::Value> {
    let repo = Repository::open(project)?;
    // These are the bytes that were hashed and checked; the upload sends them
    // without reading the archive again.
    let (release, checkpoint, bytes) = repo.release(name)?;
    let mut promoted_from = None;
    let (universe_id, place_id, purpose, rollback) = match &kind {
        PublishKind::Live {
            confirm_target,
            rollback,
            force,
            promote,
        } => {
            ensure!(
                *confirm_target == format!("{}:{}", release.universe_id, release.place_id),
                "--confirm-target must exactly match the prepared UNIVERSE_ID:PLACE_ID"
            );
            if *rollback && !*force {
                ensure!(
                    has_published_receipt(&repo, &release),
                    "Release {} has no successful publication receipt for {}:{} in this repository. Rollback only republishes releases Rojo has published; pass --force to publish it anyway.",
                    release.name,
                    release.universe_id,
                    release.place_id
                );
            }
            if let Some(gate) = promote {
                promoted_from = Some(passing_staging_record(&repo, &release, gate)?);
            }
            let purpose = match (promote, rollback) {
                (Some(_), _) => "promote",
                (None, true) => "rollback",
                (None, false) => "publish",
            };
            (release.universe_id, release.place_id, purpose, *rollback)
        }
        PublishKind::Staging {
            universe_id,
            place_id,
        } => {
            ensure!(
                *universe_id != 0 && *place_id != 0,
                "Staging universe and place IDs must be nonzero"
            );
            // Place IDs are unique across universes, so the place alone decides
            // whether staging would change what players join.
            ensure!(
                *place_id != release.place_id,
                "The staging place must differ from release {}'s target {}:{}",
                release.name,
                release.universe_id,
                release.place_id
            );
            // Nor may staging overwrite a place players join through another
            // release, whether it is already live or only prepared.
            if let Some((live_universe, live_place)) = repo
                .live_records()?
                .into_keys()
                .find(|(_, live_place)| live_place == place_id)
            {
                bail!("Place {live_place} is live: Rojo has published to {live_universe}:{live_place}. Stage to a test place that no release publishes to.");
            }
            if let Some(other) = list_manifests(&repo.state.join("releases"), false)
                .into_iter()
                .find(|other| other["place_id"] == *place_id)
            {
                bail!(
                    "Place {place_id} is the target of release {}. Stage to a test place that no release publishes to.",
                    other["name"].as_str().unwrap_or("with no name")
                );
            }
            (*universe_id, *place_id, "staging", false)
        }
    };
    let live = matches!(kind, PublishKind::Live { .. });
    // The decoded DOM can be several times the place size; free it before the
    // upload holds the archived bytes.
    let warnings = {
        let dom = decode_place(&bytes, place_extension(Path::new(&checkpoint.place_file))?)?;
        verify_cloud_publishable(&dom)?
    };
    for warning in &warnings {
        log::warn!("{warning}");
    }
    if checkpoint.imported {
        log::warn!(
            "Checkpoint {} was imported from a bundle. {IMPORT_TRUST_WARNING}",
            checkpoint.name
        );
    }
    let lock_path = repo.state.join("publication.lock");
    let attempt = uuid::Uuid::new_v4().to_string();
    let attempts = repo.state.join("publication-attempts");
    let lock = acquire_publication_lock(&lock_path, &attempts, &attempt)?;
    let receipt_path = attempts.join(format!("{attempt}.json"));
    let mut receipt = serde_json::json!({"attempt": attempt, "release": release.name,
        "universe_id": universe_id, "place_id": place_id, "checkpoint": checkpoint.name,
        "hash": checkpoint.place_hash, "rollback": rollback, "purpose": purpose, "status": "pending",
        "created_unix": now(), "checkpoint_imported": checkpoint.imported,
        "import_provenance": checkpoint.import_provenance, "warnings": warnings,
        "existing_servers_restarted": false, "player_data_changed": false});
    if !live {
        receipt["release_target"] = format!("{}:{}", release.universe_id, release.place_id).into();
    }
    if let Some(record) = &promoted_from {
        receipt["promoted_from_staging"] = record["attempt"].clone();
    }
    // The directory syncs make the lock and journal entries themselves durable,
    // not just their contents.
    let journal = fs::create_dir_all(&attempts)
        .map_err(anyhow::Error::from)
        .and_then(|_| write_new(&receipt_path, &serde_json::to_vec_pretty(&receipt)?))
        .and_then(|_| sync_directory(&attempts))
        .and_then(|_| sync_directory(&repo.state));
    if let Err(error) = journal {
        // Nothing was sent, so a lock without a journal would only block retries.
        drop(lock);
        let _ = fs::remove_file(&lock_path);
        return Err(error.context("Cannot write the publication journal; nothing was uploaded"));
    }
    let url = format!(
        "https://apis.roblox.com/universes/v1/{universe_id}/places/{place_id}/versions?versionType=Published"
    );
    let content_type = if checkpoint.place_file.ends_with(".rbxlx") {
        "application/xml"
    } else {
        "application/octet-stream"
    };
    match upload(&url, bytes, content_type) {
        Ok(version) => {
            // Reported before journaling so the version survives a failed write.
            if live {
                eprintln!("Roblox published place version {version}.");
            } else {
                eprintln!(
                    "Roblox published version {version} of staging place {universe_id}:{place_id}."
                );
            }
            receipt["status"] = "published".into();
            receipt["version_number"] = version.into();
            let live_path = live_record_path(&repo.state, universe_id, place_id);
            // A staging place is not where the release goes live, so only the
            // journal records it.
            let mut records = vec![receipt_path.clone()];
            if live {
                records.push(live_path.clone());
                records.push(repo.state.join("last-publication.json"));
            }
            // Counts the records written so far, so a failure can name exactly
            // the ones that still lack this version.
            let mut written = 0;
            let mut record = || -> anyhow::Result<()> {
                let data = serde_json::to_vec_pretty(&receipt)?;
                for path in &records {
                    fs::create_dir_all(path.parent().unwrap())?;
                    replace_file(path, &data)?;
                    written += 1;
                }
                sync_directory(&attempts)?;
                if live {
                    sync_directory(live_path.parent().unwrap())?;
                }
                sync_directory(&repo.state)
            };
            let recorded = record();
            drop(lock);
            recorded.with_context(|| {
                let missing: Vec<_> = records[written..]
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                let state = if missing.is_empty() {
                    "wrote its records but could not flush them to disk".to_owned()
                } else {
                    format!("could not record it in {}", missing.join(", "))
                };
                format!("Roblox published place version {version}, but Rojo {state}. {} is kept. Once the cause is fixed, run rojo release resolve {attempt} --version {version} --force to record it and remove the lock (--force because this process kept the lock).", lock_path.display())
            })?;
            fs::remove_file(&lock_path).with_context(|| {
                format!("Roblox published place version {version} and Rojo recorded it, but could not remove {}", lock_path.display())
            })?;
            Ok(receipt)
        }
        Err(UploadFailure::Rejected(error)) => {
            receipt["status"] = "failed".into();
            receipt["error"] = error.clone().into();
            let recorded = replace_file(&receipt_path, &serde_json::to_vec_pretty(&receipt)?);
            drop(lock);
            let note = match recorded {
                Ok(()) => String::new(),
                Err(journal) => format!(
                    " The attempt journal {} could not be updated: {journal:#}. That entry is stale: it still says pending, but this attempt did not publish, so never resolve it as published. Once the cause is fixed, run rojo release resolve {attempt} --not-published.",
                    receipt_path.display()
                ),
            };
            let message = format!("Publication failed: {error}. Roblox did not create a version. Existing servers were not restarted.{note}");
            fs::remove_file(&lock_path).with_context(|| {
                format!(
                    "{message} Rojo could not remove {}; remove it, fix the cause and retry.",
                    lock_path.display()
                )
            })?;
            bail!("{message} The lock was released; fix the cause and retry.")
        }
        Err(UploadFailure::Unconfirmed(error)) => {
            receipt["status"] = "unconfirmed".into();
            receipt["error"] = error.clone().into();
            // A lost response can still mean a successful publication. Retain the
            // journal and lock rather than automatically submitting it again.
            let message = format!("Publication not confirmed: {error}. Check Roblox version history before retrying, then record what it shows with rojo release resolve {attempt} --force and --version N or --not-published, which also removes {}. The attempt is journaled in {}. Existing servers were not restarted.", lock_path.display(), receipt_path.display());
            let recorded = replace_file(&receipt_path, &serde_json::to_vec_pretty(&receipt)?);
            drop(lock);
            recorded.with_context(|| {
                format!("{message} Rojo could not mark the attempt unconfirmed, so its journal still reads pending.")
            })?;
            bail!("{message}")
        }
    }
}

/// Luau Execution accepts scripts up to 4 MB.
const MAX_SMOKE_SCRIPT_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_SMOKE_TIMEOUT_SECONDS: u64 = 60;
/// Luau Execution's own maximum task timeout.
const MAX_SMOKE_TIMEOUT_SECONDS: u64 = 300;
/// How many of the smoke script's last log messages a staging record keeps.
const SMOKE_LOG_TAIL: usize = 50;
/// The largest smoke result kept as JSON in a staging record.
const SMOKE_RESULTS_BYTES: usize = 4096;

/// Runs a smoke script against a published place version. The arguments are
/// the universe, place, version, script and timeout.
type SmokeRunner<'a> =
    dyn FnOnce(u64, u64, u64, &str, Duration) -> Result<LuauTaskResult, OpenCloudError> + 'a;

fn stage(options: StageOptions) -> anyhow::Result<()> {
    let key = api_key(&options.api_key_env)?;
    // Created before publishing so that a missing or malformed key stops the
    // stage before anything is uploaded. Whether the key has the Luau
    // Execution scope only shows once the smoke check runs.
    let cloud = match options.smoke {
        Some(_) => Some(OpenCloud::from_env(&options.api_key_env)?),
        None => None,
    };
    stage_with(
        options,
        |url, bytes, content_type| upload_bytes(url, &key, bytes, content_type),
        Box::new(|universe, place, version, script, timeout| match &cloud {
            Some(cloud) => cloud.run_luau(universe, place, Some(version), script, timeout),
            None => Err(OpenCloudError::NotSent(
                "no Open Cloud client was created".to_owned(),
            )),
        }),
    )
    .map(|_| ())
}

/// Publishes to the staging place, runs the smoke check and records the
/// result in `rojo-state/staging/<attempt>.json`. Returns that record, or an
/// error after recording it when the smoke check did not pass.
fn stage_with(
    options: StageOptions,
    upload: impl FnOnce(&str, Vec<u8>, &str) -> Result<u64, UploadFailure>,
    smoke: Box<SmokeRunner<'_>>,
) -> anyhow::Result<serde_json::Value> {
    let (universe_id, place_id) = parse_target(&options.staging)
        .context("--staging must be UNIVERSE_ID:PLACE_ID, such as 123:456")?;
    let timeout = options
        .smoke_timeout
        .unwrap_or(DEFAULT_SMOKE_TIMEOUT_SECONDS);
    ensure!(
        (1..=MAX_SMOKE_TIMEOUT_SECONDS).contains(&timeout),
        "--smoke-timeout must be between 1 and {MAX_SMOKE_TIMEOUT_SECONDS} seconds, the Luau Execution limit"
    );
    // Read before publishing so a missing or unreadable script uploads nothing.
    let script = options
        .smoke
        .as_deref()
        .map(read_smoke_script)
        .transpose()?;
    let receipt = publish_release(
        &options.project,
        &options.name,
        PublishKind::Staging {
            universe_id,
            place_id,
        },
        upload,
    )?;
    let version = receipt["version_number"]
        .as_u64()
        .context("The staging receipt has no version number")?;
    let smoke_result = script.as_deref().map(|script| {
        eprintln!("Running the smoke check against staging version {version}.");
        smoke_outcome(smoke(
            universe_id,
            place_id,
            version,
            script,
            Duration::from_secs(timeout),
        ))
    });
    let status = match &smoke_result {
        Some(result) => result["status"].as_str().unwrap_or("error").to_owned(),
        None => "staged".to_owned(),
    };
    let attempt = receipt["attempt"].as_str().unwrap_or_default().to_owned();
    let record = serde_json::json!({
        "schema_version": 1, "attempt": attempt, "release": receipt["release"],
        "checkpoint": receipt["checkpoint"], "hash": receipt["hash"],
        "release_target": receipt["release_target"], "universe_id": universe_id,
        "place_id": place_id, "version_number": version, "status": status,
        "smoke": smoke_result, "staged_unix": now(),
    });
    let repo = Repository::open(&options.project)?;
    let directory = repo.state.join("staging");
    let written = fs::create_dir_all(&directory)
        .map_err(anyhow::Error::from)
        .and_then(|_| {
            write_new(
                &directory.join(format!("{attempt}.json")),
                &serde_json::to_vec_pretty(&record)?,
            )
        })
        .and_then(|_| sync_directory(&directory));
    written.with_context(|| {
        format!("Staging place {universe_id}:{place_id} now runs version {version} (smoke check: {status}), but Rojo could not record the result, so release promote will not accept it. Fix the cause and stage again.")
    })?;
    print_json(&record)?;
    match status.as_str() {
        "passed" | "staged" => Ok(record),
        _ => bail!(
            "Smoke check {status}: {}. Staging place {universe_id}:{place_id} runs version {version}; release promote {} refuses until a later stage passes. The live place was not changed.",
            record["smoke"]["reason"].as_str().unwrap_or("no reason given"),
            options.name
        ),
    }
}

/// Parses `UNIVERSE_ID:PLACE_ID`.
fn parse_target(text: &str) -> Option<(u64, u64)> {
    let (universe, place) = text.split_once(':')?;
    Some((universe.parse().ok()?, place.parse().ok()?))
}

fn read_smoke_script(path: &Path) -> anyhow::Result<String> {
    let size = fs::metadata(path)
        .with_context(|| format!("Cannot read smoke script {}", path.display()))?
        .len();
    ensure!(
        size <= MAX_SMOKE_SCRIPT_BYTES,
        "Smoke script {} is larger than the 4 MiB Luau Execution allows",
        path.display()
    );
    let script = fs::read_to_string(path)
        .with_context(|| format!("Cannot read smoke script {} as UTF-8", path.display()))?;
    ensure!(
        !script.trim().is_empty(),
        "Smoke script {} is empty",
        path.display()
    );
    Ok(script)
}

/// Decides whether a smoke task passed. Only a first return value of `true`,
/// or a table whose `ok` field is `true`, passes. Anything else fails,
/// including no return value, other truthy values and `{ ok = false }`, so a
/// script that forgets to report a result never counts as passing. Errors
/// from Open Cloud itself, such as a task that could not be created or was
/// still running when polling gave up, have status `error`.
fn smoke_outcome(result: Result<LuauTaskResult, OpenCloudError>) -> serde_json::Value {
    let task = match result {
        Ok(task) => task,
        Err(error) => {
            return serde_json::json!({
                "status": "error",
                "reason": short_reason(&error.to_string()),
                "task_may_have_run": error.may_have_applied(),
            })
        }
    };
    let first = task.results.first();
    let (status, reason) = match task.state {
        TaskState::Complete => match first {
            Some(serde_json::Value::Bool(true)) => ("passed", "returned true".to_owned()),
            Some(serde_json::Value::Object(table)) if table.get("ok") == Some(&true.into()) => {
                ("passed", "returned ok = true".to_owned())
            }
            Some(serde_json::Value::Object(table)) => {
                // Luau's nil becomes a missing field or null.
                let ok = table.get("ok").map_or("nil".to_owned(), |ok| match ok {
                    serde_json::Value::Null => "nil".to_owned(),
                    other => other.to_string(),
                });
                let reason = match table.get("message").and_then(|m| m.as_str()) {
                    Some(message) => format!("returned ok = {ok}: {message}"),
                    None => format!("returned ok = {ok}"),
                };
                ("failed", reason)
            }
            None | Some(serde_json::Value::Null) => (
                "failed",
                "returned nothing; return true or { ok = true } to pass".to_owned(),
            ),
            Some(other) => (
                "failed",
                format!(
                    "returned {}; return true or {{ ok = true }} to pass",
                    excerpt(&other.to_string()).trim_start_matches(": ")
                ),
            ),
        },
        TaskState::Failed | TaskState::Cancelled => {
            let code = task.error_code.as_deref().unwrap_or(match task.state {
                TaskState::Cancelled => "CANCELLED",
                _ => "FAILED",
            });
            let reason = match code {
                "DEADLINE_EXCEEDED" => {
                    "the script did not finish within --smoke-timeout".to_owned()
                }
                _ => format!("the script stopped with {code}"),
            };
            (
                "failed",
                match &task.error_message {
                    Some(message) => format!("{reason}: {message}"),
                    None => reason,
                },
            )
        }
    };
    let skip = task.logs.len().saturating_sub(SMOKE_LOG_TAIL);
    let logs: Vec<_> = task.logs[skip..]
        .iter()
        .map(|log| {
            serde_json::json!({
                "message": log.message.chars().take(500).collect::<String>(),
                "kind": log.kind,
            })
        })
        .collect();
    // The values the script returned, as JSON, unless they are too large to
    // keep in the record; then a shortened text form is stored instead.
    let results = serde_json::Value::from(task.results);
    let results_text = results.to_string();
    let (results, results_truncated) = if results_text.len() <= SMOKE_RESULTS_BYTES {
        (results, false)
    } else {
        let shortened = excerpt(&results_text);
        (
            serde_json::Value::from(shortened.trim_start_matches(": ")),
            true,
        )
    };
    serde_json::json!({
        "status": status, "reason": short_reason(&reason), "task": task.path,
        "results": results, "results_truncated": results_truncated,
        "error_code": task.error_code, "logs": logs,
        "logs_omitted": skip > 0 || task.logs_truncated,
        "logs_error": task.logs_error,
    })
}

/// The reason comes partly from Roblox and the script (an error message or a
/// returned `message`), so it is shortened like the other recorded fields.
fn short_reason(reason: &str) -> String {
    excerpt(reason)
        .strip_prefix(": ")
        .unwrap_or_default()
        .to_owned()
}

fn promote(options: PromoteOptions) -> anyhow::Result<()> {
    let key = api_key(&options.publish.api_key_env)?;
    promote_with(options, |url, bytes, content_type| {
        upload_bytes(url, &key, bytes, content_type)
    })
}

fn promote_with(
    options: PromoteOptions,
    upload: impl FnOnce(&str, Vec<u8>, &str) -> Result<u64, UploadFailure>,
) -> anyhow::Result<()> {
    ensure!(
        options.max_age_hours > 0,
        "--max-age-hours must be at least 1"
    );
    ensure!(
        live_target(&options.publish.name)?.is_none(),
        "Promote a release by name; live names one that is already published"
    );
    let PublishOptions {
        name,
        confirm_target,
        project,
        ..
    } = options.publish;
    let kind = PublishKind::Live {
        confirm_target,
        rollback: false,
        force: false,
        promote: Some(PromoteGate {
            max_age: Duration::from_secs(options.max_age_hours.saturating_mul(3600)),
            allow_unsmoked: options.allow_unsmoked,
        }),
    };
    print_json(&publish_release(&project, &name, kind, upload)?)
}

/// Returns the release's latest staging record if it allows promotion: same
/// place bytes, a passed smoke check (or none, when allowed) and recent enough.
/// Only the latest record counts, so a failed stage after a passing one blocks
/// promotion until a later stage passes.
fn passing_staging_record(
    repo: &Repository,
    release: &Release,
    gate: &PromoteGate,
) -> anyhow::Result<serde_json::Value> {
    let name = &release.name;
    let directory = repo.state.join("staging");
    let missing = || {
        format!("Release {name} has no staging record. Run rojo release stage {name} --staging UNIVERSE_ID:PLACE_ID --smoke <script.luau> first.")
    };
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!("{}", missing()),
        Err(error) => return Err(error.into()),
    };
    let mut latest: Option<(PathBuf, serde_json::Value)> = None;
    for entry in entries {
        let entry = entry?;
        let file = entry.file_name().to_string_lossy().into_owned();
        // Leftovers of interrupted writes start with `.`.
        if file.starts_with('.') || !file.ends_with(".json") {
            continue;
        }
        // Unlike `release list`, this does not skip unreadable records: one
        // could be the latest, failed stage.
        let record: serde_json::Value = read_json(&entry.path())
            .context("Cannot check staging records; fix or remove this file")?;
        if record["release"] != name.as_str() {
            continue;
        }
        let time = record["staged_unix"].as_u64().unwrap_or(0);
        let newer = latest.as_ref().is_none_or(|(_, current)| {
            let current_time = current["staged_unix"].as_u64().unwrap_or(0);
            // Times are whole seconds; of two records from the same second a
            // failing one wins, so a tie never hides a failure.
            time > current_time || (time == current_time && record["status"] != "passed")
        });
        if newer {
            latest = Some((entry.path(), record));
        }
    }
    let (path, record) = latest.with_context(missing)?;
    let attempt = record["attempt"].as_str().unwrap_or("with no attempt ID");
    let current_time = now();
    // A record written while the clock was ahead would outrank every later
    // stage, including failures, and never grow old, so it is not trusted.
    let staged = record["staged_unix"].as_u64().unwrap_or(0);
    ensure!(
        staged <= current_time.saturating_add(CLOCK_SLACK_SECONDS),
        "The latest staging record of release {name} ({attempt}) is dated {} minutes in the future, so the clock was probably wrong when it was written. It would outrank every later stage, so promote does not trust it. Once the clock is right, remove {} and stage the release again.",
        (staged - current_time) / 60,
        path.display()
    );
    ensure_latest_staging_attempt(repo, name, &record, current_time)?;
    ensure!(
        record["hash"] == release.checkpoint_hash.as_str(),
        "The latest staging of release {name} ({attempt}) staged different place bytes than the release now has. Stage it again."
    );
    let target = format!("{}:{}", release.universe_id, release.place_id);
    ensure!(
        record["release_target"] == target.as_str(),
        "The latest staging of release {name} ({attempt}) was made when the release targeted {}, but it now targets {target}. Stage it again.",
        record["release_target"].as_str().unwrap_or("another place")
    );
    ensure!(
        record["place_id"]
            .as_u64()
            .is_some_and(|place| place != release.place_id)
            && record["version_number"]
                .as_u64()
                .is_some_and(|version| version > 0),
        "The latest staging record of release {name} ({attempt}) is invalid. Stage it again."
    );
    match record["status"].as_str() {
        Some("passed") => {}
        Some("staged") if gate.allow_unsmoked => {}
        Some("staged") => bail!(
            "The latest staging of release {name} ({attempt}) ran no smoke check. Stage it again with --smoke, or pass --allow-unsmoked to promote it anyway."
        ),
        status => bail!(
            "The latest staging of release {name} ({attempt}) did not pass its smoke check ({}): {}. Fix the cause and stage it again; the live place was not changed.",
            status.unwrap_or("unknown"),
            record["smoke"]["reason"].as_str().unwrap_or("no reason recorded")
        ),
    }
    let age = current_time.saturating_sub(staged);
    ensure!(
        age <= gate.max_age.as_secs(),
        "The latest staging of release {name} ({attempt}) is {} hours old, older than --max-age-hours allows. Stage it again.",
        age / 3600
    );
    Ok(record)
}

/// How far ahead of the clock a staging record or journal entry may be dated
/// before promote assumes the clock was wrong when it was written.
const CLOCK_SLACK_SECONDS: u64 = 5 * 60;

/// Checks that `record` describes this release's newest staging upload.
/// A stage writes its record only after the smoke check, so a stage that was
/// killed during the check, is still running it, or was settled with
/// `release resolve` leaves an upload in the journal and no record, and the
/// staging place then runs a version no record vouches for.
fn ensure_latest_staging_attempt(
    repo: &Repository,
    name: &str,
    record: &serde_json::Value,
    current_time: u64,
) -> anyhow::Result<()> {
    let attempt = record["attempt"].as_str().unwrap_or_default();
    let attempts = repo.state.join("publication-attempts");
    // A rejected or `--not-published` attempt created no version, so it
    // cannot have replaced what the record's stage uploaded.
    let uploads: Vec<_> = list_manifest_entries(&attempts, false)
        .into_iter()
        .filter(|(_, receipt)| {
            receipt["purpose"] == "staging"
                && receipt["release"] == name
                && matches!(
                    receipt["status"].as_str(),
                    Some("pending" | "unconfirmed" | "published")
                )
        })
        .collect();
    let created = |receipt: &serde_json::Value| receipt["created_unix"].as_u64().unwrap_or(0);
    let own = uploads
        .iter()
        .find(|(_, receipt)| receipt["attempt"] == attempt)
        .with_context(|| {
            format!("The latest staging record of release {name} ({attempt}) has no published staging attempt in {}. Stage the release again.", attempts.display())
        })?;
    let own_created = created(&own.1);
    // Times are whole seconds, so another upload from the same second counts
    // as later: a tie must not let an older pass stand in for a newer upload.
    let later = uploads
        .iter()
        .filter(|(_, receipt)| receipt["attempt"] != attempt && created(receipt) >= own_created)
        .max_by_key(|(_, receipt)| created(receipt));
    if let Some((path, receipt)) = later {
        let clock = if created(receipt) > current_time.saturating_add(CLOCK_SLACK_SECONDS) {
            format!(" Its journal entry {} is dated in the future, so the clock was probably wrong when it was written; once the clock is right and that attempt is settled, move the entry out of publication-attempts.", path.display())
        } else {
            String::new()
        };
        bail!(
            "Release {name} was staged again ({}, {}) after the stage its latest staging record describes ({attempt}). Only the latest stage counts, and no record shows that one passed: its smoke check may have been interrupted or still be running, or its upload was settled with rojo release resolve. Stage the release again.{clock}",
            receipt["attempt"].as_str().unwrap_or("with no attempt ID"),
            receipt["status"].as_str().unwrap_or("unknown")
        );
    }
    Ok(())
}

/// Creates the publication lock and records which attempt holds it, so a lock
/// left behind can be traced to its journal entry, or shown to have none.
fn acquire_publication_lock(
    lock_path: &Path,
    attempts: &Path,
    attempt: &str,
) -> anyhow::Result<fs::File> {
    let holder = serde_json::to_vec_pretty(&serde_json::json!({
        "attempt": attempt, "pid": std::process::id(), "created_unix": now()
    }))?;
    let mut lock = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            bail!("{}", stale_lock_message(lock_path, attempts))
        }
        Err(error) => {
            return Err(error).context("Cannot create the publication lock; nothing was uploaded")
        }
    };
    if let Err(error) = lock.write_all(&holder).and_then(|_| lock.sync_all()) {
        drop(lock);
        let _ = fs::remove_file(lock_path);
        return Err(error).context("Cannot write the publication lock; nothing was uploaded");
    }
    Ok(lock)
}

fn stale_lock_message(lock_path: &Path, attempts: &Path) -> String {
    let holder = fs::read(lock_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    // Parsing as a UUID keeps lock contents from naming a path outside the journal.
    let attempt = holder.as_ref().and_then(|holder| {
        holder["attempt"]
            .as_str()
            .and_then(|attempt| uuid::Uuid::parse_str(attempt).ok())
    });
    let detail = match (&holder, attempt) {
        (Some(holder), Some(attempt)) => {
            let journal = attempts.join(format!("{attempt}.json"));
            if journal.is_file() {
                format!(
                    "It belongs to attempt {attempt} by process {}. Inspect {} for its status. If it is pending or unconfirmed and that process is no longer running, check Roblox version history and run rojo release resolve {attempt} with --version N or --not-published, which removes the lock.",
                    holder["pid"],
                    journal.display()
                )
            } else {
                format!(
                    "It belongs to attempt {attempt} by process {}, which has no journal entry, so that process stopped before uploading anything. If it is no longer running, remove the lock.",
                    holder["pid"]
                )
            }
        }
        // Rojo writes the holder right after creating the lock, and earlier
        // versions left it empty, so an empty lock may still have a journal.
        _ => format!(
            "It does not name its attempt. Inspect {} for a pending or unconfirmed entry before removing the lock.",
            attempts.display()
        ),
    };
    format!(
        "Another publication is active, or a previous process exited before recording its result: {} exists. {detail}",
        lock_path.display()
    )
}

/// What Roblox version history showed for an attempt Rojo could not settle.
#[derive(Debug, Clone, Copy)]
enum Resolution {
    Published(u64),
    NotPublished,
}

/// Records the outcome of an attempt that Rojo left `pending` or
/// `unconfirmed`, or published but could not finish recording, and removes
/// the publication lock if that attempt holds it.
fn resolve_attempt(
    repo: &Repository,
    attempt: &str,
    outcome: Resolution,
    force: bool,
) -> anyhow::Result<serde_json::Value> {
    // Parsing as a UUID keeps the argument from naming a path outside the journal.
    let attempt = uuid::Uuid::parse_str(attempt)
        .context("Attempt IDs are UUIDs, as named in the error and in publication-attempts")?
        .to_string();
    if let Resolution::Published(version) = outcome {
        ensure!(version > 0, "Roblox place versions start at 1");
    }
    let attempts = repo.state.join("publication-attempts");
    let receipt_path = attempts.join(format!("{attempt}.json"));
    let mut receipt: serde_json::Value = read_json(&receipt_path)?;
    ensure!(
        receipt["attempt"] == attempt.as_str(),
        "{} names a different attempt",
        receipt_path.display()
    );
    let (universe_id, place_id) =
        record_target(&receipt).context("The attempt's journal entry has no target")?;
    let status = receipt["status"].as_str().unwrap_or_default().to_owned();
    match (status.as_str(), outcome) {
        ("pending" | "unconfirmed", _) => {}
        // A publication whose journal entry was updated before a later record failed.
        ("published", Resolution::Published(version)) => ensure!(
            receipt["version_number"] == version,
            "Attempt {attempt} already recorded version {}",
            receipt["version_number"]
        ),
        _ => bail!(
            "Attempt {attempt} is {status}; only pending, unconfirmed or partly recorded attempts can be resolved"
        ),
    }
    let lock_path = repo.state.join("publication.lock");
    if status == "pending" && !force {
        ensure_publisher_finished(&lock_path, &attempt)?;
    }
    receipt["resolved_manually_unix"] = now().into();
    match outcome {
        Resolution::Published(version) => {
            receipt["status"] = "published".into();
            receipt["version_number"] = version.into();
        }
        Resolution::NotPublished => {
            receipt["status"] = "failed".into();
            receipt["resolution"] =
                "Roblox version history showed no version for this attempt".into();
        }
    }
    let data = serde_json::to_vec_pretty(&receipt)?;
    replace_file(&receipt_path, &data)?;
    let live = live_record_path(&repo.state, universe_id, place_id);
    let mut kept_newer = Vec::new();
    // A staging place is not a release's live target, so it gets no pointer.
    // Its smoke check never ran, so `release promote` still needs a new stage.
    let staging = receipt["purpose"] == "staging";
    if let (Resolution::Published(_), false) = (outcome, staging) {
        fs::create_dir_all(live.parent().unwrap())?;
        let created = receipt["created_unix"].as_u64().unwrap_or(0);
        for path in [live.clone(), repo.state.join("last-publication.json")] {
            // Resolving an old attempt must not replace a later publication's
            // record. Times are in whole seconds, so another attempt's record
            // from the same second is kept too.
            let newer = read_json::<serde_json::Value>(&path).is_ok_and(|record| {
                record["attempt"] != attempt.as_str()
                    && record["created_unix"].as_u64().unwrap_or(0) >= created
            });
            if newer {
                kept_newer.push(path);
            } else {
                replace_file(&path, &data)?;
            }
        }
    }
    sync_directory(&attempts)?;
    if live.parent().unwrap().is_dir() {
        sync_directory(live.parent().unwrap())?;
    }
    sync_directory(&repo.state)?;

    let holder = match fs::read(&lock_path) {
        Ok(bytes) => Some(
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|holder| holder["attempt"].as_str().map(str::to_owned))
                .and_then(|holder| uuid::Uuid::parse_str(&holder).ok()),
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let lock = match holder {
        None => "absent",
        Some(Some(holder)) if holder.to_string() == attempt => {
            fs::remove_file(&lock_path)?;
            "removed"
        }
        // Another attempt's lock, or an empty one from an earlier version.
        Some(_) => "kept: it does not belong to this attempt",
    };
    Ok(serde_json::json!({
        "receipt": receipt,
        "lock": lock,
        "kept_newer_records": kept_newer,
        "staging": staging,
    }))
}

fn has_published_receipt(repo: &Repository, release: &Release) -> bool {
    list_manifests(&repo.state.join("publication-attempts"), false)
        .iter()
        .any(|receipt| {
            receipt["status"] == "published"
                && receipt["purpose"] != "staging"
                && receipt["release"] == release.name.as_str()
                && receipt["hash"] == release.checkpoint_hash.as_str()
                && receipt["universe_id"] == release.universe_id
                && receipt["place_id"] == release.place_id
        })
}

/// The longest a publish holding the lock can run: connecting, the slowest
/// allowed upload of the largest place, and a minute for local work.
fn max_publication_duration() -> Duration {
    Duration::from_secs(30 + 60) + upload_timeout(MAX_PLACE_BYTES as usize)
}

/// Refuses while a `pending` attempt's own lock is young enough that its
/// process may still be uploading. Rojo has no portable way to check whether
/// the recorded PID is alive (or still the same process), so the lock's age
/// stands in for it.
fn ensure_publisher_finished(lock_path: &Path, attempt: &str) -> anyhow::Result<()> {
    let bytes = match fs::read(lock_path) {
        Ok(bytes) => bytes,
        // Without a lock no process can be publishing this attempt.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let holder: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let ours = holder["attempt"]
        .as_str()
        .and_then(|holder| uuid::Uuid::parse_str(holder).ok())
        .is_some_and(|holder| holder.to_string() == attempt);
    if !ours {
        return Ok(());
    }
    let created = holder["created_unix"].as_u64().or_else(|| {
        fs::metadata(lock_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|age| age.as_secs())
    });
    let age = created.map(|created| now().saturating_sub(created));
    let limit = max_publication_duration().as_secs();
    if age.is_some_and(|age| age >= limit) {
        return Ok(());
    }
    let age = age.map_or("of unknown age".to_owned(), |age| {
        format!("{age} seconds old")
    });
    bail!(
        "Attempt {attempt} is still pending and {} is {age}, held by process {}. A publication can take up to {limit} seconds, so it may still be uploading. Wait for it to finish, or check that the process has exited and pass --force.",
        lock_path.display(),
        holder["pid"]
    )
}

/// The whole request must finish within this time. Large places need longer on
/// slow uplinks: 120 seconds plus one second per 256 KiB.
fn upload_timeout(bytes: usize) -> Duration {
    Duration::from_secs(120 + bytes as u64 / (256 * 1024))
}

fn upload_bytes(
    url: &str,
    key: &str,
    bytes: Vec<u8>,
    content_type: &str,
) -> Result<u64, UploadFailure> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(upload_timeout(bytes.len()))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| UploadFailure::Rejected(format!("Cannot create HTTP client: {error}")))?;
    // Sensitive values are left out of reqwest's debug output and logs.
    let mut key = reqwest::header::HeaderValue::from_str(key).map_err(|_| {
        UploadFailure::Rejected(
            "Request was not sent: the API key is not a valid HTTP header value".to_owned(),
        )
    })?;
    key.set_sensitive(true);
    let response = client
        .post(url)
        .header("x-api-key", key)
        .header("Content-Type", content_type)
        .body(bytes)
        .send()
        .map_err(|error| {
            // Builder errors (such as an invalid header) and connection failures
            // (DNS, refused connections, TLS handshakes, connect timeouts) happen
            // before any request reaches Roblox. Anything later may have been applied.
            if error.is_builder() || error.is_connect() {
                UploadFailure::Rejected(format!("Request was not sent: {error}"))
            } else {
                UploadFailure::Unconfirmed(format!("Request failed after sending: {error}"))
            }
        })?;
    let status = response.status();
    let body = response.text();
    if !status.is_success() {
        let message = format!(
            "Open Cloud returned HTTP {status}{}",
            body.map(|text| excerpt(&text)).unwrap_or_default()
        );
        // 408 (request timeout), 409 (conflict) and 499 (client closed request)
        // can be reported after the server already acted on the upload.
        let may_have_published = matches!(status.as_u16(), 408 | 409 | 499);
        return Err(if status.is_client_error() && !may_have_published {
            UploadFailure::Rejected(message)
        } else {
            UploadFailure::Unconfirmed(message)
        });
    }
    let body = body.map_err(|error| {
        UploadFailure::Unconfirmed(format!("Open Cloud response could not be read: {error}"))
    })?;
    let result: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
        UploadFailure::Unconfirmed(format!(
            "Open Cloud did not return a publication receipt{}",
            excerpt(&body)
        ))
    })?;
    result["versionNumber"]
        .as_u64()
        .filter(|v| *v > 0)
        .ok_or_else(|| {
            UploadFailure::Unconfirmed(format!(
                "Open Cloud response has no valid versionNumber{}",
                excerpt(&body)
            ))
        })
}

/// A short, single-line copy of a response body for error messages and receipts.
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

/// Returns warnings for content that does not block publishing.
fn verify_cloud_publishable(dom: &WeakDom) -> anyhow::Result<Vec<String>> {
    let database = rbx_reflection_database::get()?;
    let mut warnings = Vec::new();
    let mut queue: Vec<_> = dom.root().children().iter().map(|id| (*id, true)).collect();
    while let Some((id, is_service)) = queue.pop() {
        let instance = dom.get_by_ref(id).unwrap();
        let mut class_name = instance.class;
        loop {
            ensure!(!matches!(class_name.as_str(), "EditableImage" | "EditableMesh" | "PartOperation" | "SurfaceAppearance" | "BaseWrap"),
                "{} contains {} which needs native Studio publishing. Export this checkpoint and publish it through Studio; Open Cloud is not a safe path for this content.", instance.name, instance.class);
            match database.classes.get(class_name.as_str()) {
                Some(descriptor) => match descriptor.superclass {
                    Some(parent) => class_name = parent.into(),
                    None => break,
                },
                // Newer Studio versions save services this database predates. Their
                // descendants are still checked, so only the service object itself
                // is unverified.
                None if is_service && class_name == instance.class => {
                    warnings.push(format!(
                        "Service {} has class {}, which this Rojo version does not know; its contents were still checked",
                        instance.name, instance.class
                    ));
                    break;
                }
                None => bail!(
                    "{} has unknown Roblox class {}; use native Studio publishing until its capabilities can be verified",
                    instance.name,
                    instance.class
                ),
            }
        }
        queue.extend(instance.children().iter().map(|id| (*id, false)));
    }
    Ok(warnings)
}

/// Returns how many scripts were verified and every file system path the
/// project read.
fn verify_sources(project: &Path, place: &WeakDom) -> anyhow::Result<(usize, Vec<PathBuf>)> {
    let session = ServeSession::new(Vfs::new_default_without_watch(), project)?;
    let tree = session.tree();
    let mut source_paths = Vec::new();
    let mut pending = vec![tree.get_root_id()];
    while let Some(id) = pending.pop() {
        let instance = tree.get_instance(id).unwrap();
        source_paths.extend(instance.metadata().relevant_paths.iter().cloned());
        pending.extend(instance.children().iter().copied());
    }
    ensure!(
        tree.inner().root().class.as_str() == "DataModel",
        "Checkpoints require a place project with a DataModel root"
    );
    let (source, source_duplicates) = script_sources(tree.inner())?;
    if let Some(path) = source_duplicates.iter().next() {
        bail!(
            "Ambiguous duplicate script path {} in the Rojo project",
            path.join("/")
        );
    }
    let (saved, saved_duplicates) = script_sources(place)?;
    // Studio-owned content often contains duplicates such as several copies of a
    // free model. They only block a checkpoint where Rojo needs one exact match.
    for path in &saved_duplicates {
        ensure!(
            !source.contains_key(path) && !script_is_managed(path, &tree)?,
            "Ambiguous duplicate script path {} inside a Rojo-owned subtree. Rename one of the scripts, sync and save again.",
            path.join("/")
        );
    }
    for (path, expected) in &source {
        let actual = saved
            .get(path)
            .with_context(|| format!("Saved place is missing mapped script {}", path.join("/")))?;
        ensure!(
            expected == actual,
            "Saved place has stale Source or execution settings at {}. Sync, save the Edit place, and try again.",
            path.join("/")
        );
    }
    for path in saved.keys().filter(|path| !source.contains_key(*path)) {
        ensure!(!script_is_managed(path, &tree)?, "Saved place contains unexpected executable script {} inside a Rojo-owned subtree. Sync and save the Edit place again.", path.join("/"));
    }
    Ok((source.len(), source_paths))
}

/// Existing paths that resolve outside `root`, including through symbolic
/// links. Paths that do not exist, such as optional meta files, are skipped.
fn paths_outside(root: &Path, paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    paths
        .iter()
        .filter_map(|path| dunce::canonicalize(path).ok())
        .filter(|path| !path.starts_with(root))
        .collect()
}

const CHECKPOINT_CLEAN_HINT: &str =
    "Save the place outside the checkout, then create the checkpoint.";

fn script_is_managed(path: &[String], tree: &crate::RojoTree) -> anyhow::Result<bool> {
    let mut node = tree.get_instance(tree.get_root_id()).unwrap();
    for name in path {
        let mut matching = node
            .children()
            .iter()
            .map(|id| tree.get_instance(*id).unwrap())
            .filter(|child| child.name() == name);
        match matching.next() {
            Some(child) => {
                ensure!(
                    matching.next().is_none(),
                    "Ambiguous mapped instance path {}",
                    path.join("/")
                );
                node = child;
            }
            None => return Ok(!node.metadata().ignore_unknown_instances),
        }
    }
    // An unexpected script at a known mapped non-script path is a class mismatch.
    Ok(true)
}

#[derive(Debug, PartialEq, Eq)]
struct ScriptState {
    class: String,
    source: String,
    run_context: u32,
    disabled: bool,
}

type ScriptPath = Vec<String>;

/// Returns scripts with unique paths, and separately every path shared by
/// several scripts. Duplicates are left out of the map since neither copy can be
/// matched reliably.
fn script_sources(
    dom: &WeakDom,
) -> anyhow::Result<(BTreeMap<ScriptPath, ScriptState>, BTreeSet<ScriptPath>)> {
    let mut result = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    let mut pending: Vec<_> = dom
        .root()
        .children()
        .iter()
        .map(|id| (*id, Vec::new()))
        .collect();
    while let Some((id, mut path)) = pending.pop() {
        let instance = dom.get_by_ref(id).unwrap();
        path.push(instance.name.clone());
        if matches!(
            instance.class.as_str(),
            "Script" | "LocalScript" | "ModuleScript"
        ) {
            let source = match instance.properties.get(&rbx_dom_weak::Ustr::from("Source")) {
                Some(Variant::String(value)) => value.clone(),
                None => String::new(),
                _ => bail!("Unsupported script Source at {}", path.join("/")),
            };
            let run_context = match instance
                .properties
                .get(&rbx_dom_weak::Ustr::from("RunContext"))
            {
                Some(Variant::Enum(context)) => context.to_u32(),
                None => 0,
                _ => bail!("Unsupported RunContext at {}", path.join("/")),
            };
            // Enabled is the canonical property; Disabled is its legacy inverse.
            // Either may be present depending on who wrote the instance.
            let property = |name: &str| instance.properties.get(&rbx_dom_weak::Ustr::from(name));
            let disabled = match (property("Enabled"), property("Disabled")) {
                (Some(Variant::Bool(enabled)), _) => !*enabled,
                (None, Some(Variant::Bool(disabled))) => *disabled,
                (None, None) => false,
                _ => bail!(
                    "Unsupported Enabled or Disabled property at {}",
                    path.join("/")
                ),
            };
            let state = ScriptState {
                class: instance.class.to_string(),
                source: source.replace("\r\n", "\n"),
                run_context,
                disabled,
            };
            if duplicates.contains(&path) || result.insert(path.clone(), state).is_some() {
                result.remove(&path);
                duplicates.insert(path.clone());
            }
        }
        pending.extend(instance.children().iter().map(|id| (*id, path.clone())));
    }
    Ok((result, duplicates))
}

fn decode_place(bytes: &[u8], extension: &str) -> anyhow::Result<WeakDom> {
    match extension {
        "rbxl" => rbx_binary::from_reader(bytes)
            .context("Cannot decode place; original file was not modified"),
        "rbxlx" => rbx_xml::from_reader(
            bytes,
            rbx_xml::DecodeOptions::new()
                .property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown),
        )
        .context("Cannot decode XML place; original file was not modified"),
        _ => bail!("Expected a native .rbxl or .rbxlx place"),
    }
}

/// Returns the lowercase place format, accepting any capitalization.
fn place_extension(path: &Path) -> anyhow::Result<&'static str> {
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or_default();
    if extension.eq_ignore_ascii_case("rbxl") {
        Ok("rbxl")
    } else if extension.eq_ignore_ascii_case("rbxlx") {
        Ok("rbxlx")
    } else {
        bail!("Expected a .rbxl or .rbxlx place file")
    }
}

fn read_place_bytes(path: &Path) -> anyhow::Result<Vec<u8>> {
    let message = "Place must be nonempty and at most 512 MiB";
    // The metadata check avoids reading a huge file only to reject it; the
    // capped read still holds if the file grows after that check.
    ensure!(fs::metadata(path)?.len() <= MAX_PLACE_BYTES, message);
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_PLACE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Cannot read {}", path.display()))?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_PLACE_BYTES,
        message
    );
    Ok(bytes)
}

/// Converts a relative path to Git's `/`-separated form, which manifests store
/// so that they mean the same thing on every OS.
fn repository_path(path: &Path) -> anyhow::Result<String> {
    let parts = path
        .components()
        .map(|component| match component {
            Component::Normal(name) => name
                .to_str()
                .context("Repository paths must be valid UTF-8"),
            _ => bail!("Project file must be a relative repository path"),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    normalize_repository_path(&parts.join("/"))
}

/// Validates a stored repository path. `\` is read as a separator because
/// earlier Windows builds wrote native paths; Rojo projects cannot use it in a
/// file name on Windows anyway.
fn normalize_repository_path(path: &str) -> anyhow::Result<String> {
    let normalized = path.replace('\\', "/");
    ensure!(
        !normalized.is_empty()
            && normalized
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains(':')),
        "Project file must be a relative repository path"
    );
    Ok(normalized)
}

fn native_path(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_owned(), |path, part| path.join(part))
}

/// Hashes package manifests and lockfiles from the project's directory up to
/// the repository root, where a workspace-level manifest usually lives. Keys
/// are repository paths.
fn package_file_hashes(
    root: &Path,
    project_file: &Path,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut hashes = BTreeMap::new();
    let mut directory = project_file
        .parent()
        .context("Project file has no parent")?;
    loop {
        for name in [
            "wally.toml",
            "wally.lock",
            "pesde.toml",
            "pesde.lock",
            "rokit.toml",
        ] {
            let path = directory.join(name);
            if path.is_file() {
                let key = repository_path(path.strip_prefix(root)?)?;
                hashes.insert(key, hash(&fs::read(&path)?));
            }
        }
        match directory.parent() {
            Some(parent) if directory != root && parent.starts_with(root) => directory = parent,
            _ => break,
        }
    }
    Ok(hashes)
}

/// `rev-parse --path-format` needs Git 2.31. Older versions echo the unknown
/// option back as output instead of failing, so check that the result is
/// one absolute path.
fn git_common_dir(directory: &Path) -> anyhow::Result<PathBuf> {
    let output = git(
        directory,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    match output.as_deref().ok().and_then(parse_common_dir) {
        Some(path) => Ok(path),
        None => {
            let version = git(directory, ["--version"]).unwrap_or_else(|_| "unknown".into());
            let error = output.err().map(|e| format!(": {e:#}")).unwrap_or_default();
            bail!("Cannot locate the shared Git directory{error}. Rojo needs Git 2.31 or newer (found {version}).")
        }
    }
}

fn parse_common_dir(output: &str) -> Option<PathBuf> {
    let path = PathBuf::from(output);
    (!output.contains('\n') && path.is_absolute()).then_some(path)
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "Use 1-64 letters, digits, hyphens or underscores for the name"
    );
    // A leading hyphen would be read as an option by Git (for example the branch name).
    ensure!(!name.starts_with('-'), "Names cannot start with a hyphen");
    let upper = name.to_ascii_uppercase();
    ensure!(
        !matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            && !(upper.len() == 4
                && (upper.starts_with("COM") || upper.starts_with("LPT"))
                && upper.as_bytes()[3].is_ascii_digit()),
        "Name is reserved on Windows"
    );
    Ok(())
}

/// Entry names in `directory`, minus `suffix` when given (entries without it
/// are skipped). A missing directory has none.
fn existing_names(directory: &Path, suffix: Option<&str>) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| match suffix {
            Some(suffix) => name.strip_suffix(suffix).map(str::to_owned),
            None => Some(name),
        })
        .collect()
}

/// Rejects `name` when an existing name of the same kind differs from it only
/// in case. Those share a file or ref on case-insensitive file systems, such as
/// the Windows and macOS defaults, and are easy to confuse everywhere else.
/// Exact matches are left to each caller's own "already exists" check.
fn ensure_no_case_conflict(
    kind: &str,
    name: &str,
    existing: impl IntoIterator<Item = String>,
) -> anyhow::Result<()> {
    if let Some(other) = existing
        .into_iter()
        .find(|other| other != name && other.eq_ignore_ascii_case(name))
    {
        bail!(
            "A {kind} named {other} already exists; names that differ only in case are not allowed"
        );
    }
    Ok(())
}

fn git<I, S>(directory: &Path, args: I) -> anyhow::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git(git_command(directory).args(args))
}

fn run_git(command: &mut Command) -> anyhow::Result<String> {
    let output = command
        .output()
        .context("Git is required for this command")?;
    ensure!(
        output.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)
        .context("Git returned a non-UTF-8 path")?
        .trim()
        .to_owned())
}

/// Variables that redirect Git to another repository or inject configuration.
/// Git sets several of them for hooks, where they would otherwise apply to
/// every command regardless of -C.
const INHERITED_GIT_VARIABLES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    // These change which history Git sees, so a checkpoint could record or
    // look up a commit differently from a plain clone.
    "GIT_SHALLOW_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_GRAFT_FILE",
];

/// `GIT_TERMINAL_PROMPT` does not reach SSH, which asks for passphrases and
/// host-key confirmation on the terminal itself. BatchMode makes it fail
/// instead. Returns `None` when the user's SSH program is not OpenSSH, since
/// other programs such as PuTTY's plink do not accept `-o`.
fn batch_ssh_command(directory: &Path) -> Option<String> {
    let from_env = std::env::var("GIT_SSH_COMMAND")
        .ok()
        .filter(|command| !command.trim().is_empty());
    let configured = || git(directory, ["config", "--get", "core.sshCommand"]).ok();
    batch_ssh(
        from_env,
        std::env::var_os("GIT_SSH").is_some_and(|program| !program.is_empty()),
        configured,
    )
}

/// Git uses `GIT_SSH_COMMAND`, then `GIT_SSH`, then `core.sshCommand`, then `ssh`.
fn batch_ssh(
    from_env: Option<String>,
    git_ssh_set: bool,
    configured: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let command = match from_env {
        Some(command) => command,
        // GIT_SSH names a program without a shell, so options cannot be added
        // to it without replacing it.
        None if git_ssh_set => return None,
        None => configured()
            .filter(|command| !command.trim().is_empty())
            .unwrap_or_else(|| "ssh".to_owned()),
    };
    let command = command.trim();
    // Git runs the command through a shell, so a path with spaces is quoted.
    let program = match command.chars().next()? {
        quote @ ('"' | '\'') => command[1..].split(quote).next()?,
        _ => command.split_whitespace().next()?,
    };
    let stem = Path::new(program).file_stem()?.to_str()?;
    stem.eq_ignore_ascii_case("ssh")
        .then(|| format!("{command} -o BatchMode=yes"))
}

fn git_command(directory: &Path) -> Command {
    let mut command = Command::new("git");
    for name in INHERITED_GIT_VARIABLES {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("GIT_CONFIG_KEY_") || upper.starts_with("GIT_CONFIG_VALUE_") {
            command.env_remove(&name);
        }
    }
    // Nobody can answer a credential prompt from inside Rojo, for example while
    // `submodule update` fetches, so fail instead of waiting forever.
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .arg("-C")
        .arg(directory);
    command
}

fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn archive_checkpoint(
    destination: &Path,
    checkpoint: &Checkpoint,
    bytes: &[u8],
) -> anyhow::Result<()> {
    ensure!(
        !destination.exists(),
        "Checkpoint destination already exists"
    );
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = tempfile::tempdir_in(parent)?;
    write_new(&staging.path().join(&checkpoint.place_file), bytes)?;
    write_new(
        &staging.path().join("manifest.json"),
        &serde_json::to_vec_pretty(checkpoint)?,
    )?;
    fs::rename(staging.path(), destination).context("Cannot finalize checkpoint bundle")?;
    sync_directory(parent)
}

/// Makes a rename durable. Unix only records renames once the directory itself is
/// synced; Windows cannot open directories this way and commits renames anyway.
fn sync_directory(directory: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(directory)
        .and_then(|handle| handle.sync_all())
        .with_context(|| format!("Cannot sync {}", directory.display()))?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    serde_json::from_slice(
        &fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?,
    )
    .with_context(|| format!("Invalid JSON in {}", path.display()))
}
fn write_new(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Refusing to overwrite {}", path.display()))?;
    Ok(())
}
fn replace_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("Missing parent directory")?)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}
/// Reads every manifest in `directory`. Invalid entries are skipped with a
/// warning, and leftovers of interrupted writes are skipped with a debug
/// message, so one bad file cannot hide the rest.
fn list_manifests(directory: &Path, nested: bool) -> Vec<serde_json::Value> {
    list_manifest_entries(directory, nested)
        .into_iter()
        .map(|(_, value)| value)
        .collect()
}

/// Lists checkpoints by directory. Verification looks each one up by the
/// directory it is stored in, so a manifest naming another checkpoint fails.
fn list_checkpoints(repo: &Repository, verify: bool) -> Vec<serde_json::Value> {
    let mut manifests = list_manifest_entries(&repo.state.join("checkpoints"), true);
    if verify {
        for (path, manifest) in &mut manifests {
            let name = path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let result = repo.checkpoint(&name);
            manifest["verified"] = result.is_ok().into();
            if let Err(error) = result {
                manifest["verification_error"] = format!("{error:#}").into();
            }
        }
    }
    manifests.into_iter().map(|(_, value)| value).collect()
}

fn list_manifest_entries(directory: &Path, nested: bool) -> Vec<(PathBuf, serde_json::Value)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Staging directories and temporary files from tempfile start with ".tmp".
        if entry.file_name().to_string_lossy().starts_with('.') {
            log::debug!("Skipping leftover {}", path.display());
            continue;
        }
        if nested && path.is_dir() && path.join("manifest.json").is_file() {
            paths.push(path.join("manifest.json"));
        } else if !nested && path.extension() == Some(OsStr::new("json")) {
            paths.push(path);
        }
    }
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| match read_json(&path) {
            Ok(value) => Some((path, value)),
            Err(error) => {
                log::warn!("Skipping {}: {error:#}", path.display());
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::InstanceBuilder;
    use std::io::Read;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        place: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            Self::build(&[], "native-save.rbxl").unwrap()
        }

        /// Returns `None` when the installed Git does not support `init_args`.
        fn build(init_args: &[&str], place_name: &str) -> Option<Self> {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("Game With Spaces");
            fs::create_dir(&root).unwrap();
            let mut init = vec!["init", "--initial-branch=main"];
            init.extend_from_slice(init_args);
            git(&root, init).ok()?;
            git(&root, ["config", "user.email", "fixture@example.invalid"]).unwrap();
            git(&root, ["config", "user.name", "Rojo fixture"]).unwrap();
            fs::write(root.join("default.project.json"), r#"{"name":"Fixture","tree":{"$className":"DataModel","ServerScriptService":{"Main":{"$path":"main.server.luau"}}}}"#).unwrap();
            fs::write(root.join("main.server.luau"), "print('original')\n").unwrap();
            git(&root, ["add", "."]).unwrap();
            git(&root, ["commit", "-m", "Fixture (AI-assisted)"]).unwrap();
            let place = temp.path().join(place_name);
            let session = ServeSession::new(Vfs::new_default_without_watch(), &root).unwrap();
            let tree = session.tree();
            let mut bytes = Vec::new();
            rbx_binary::to_writer(&mut bytes, tree.inner(), tree.inner().root().children())
                .unwrap();
            // The place also has Studio-only content, absent from the project.
            let mut dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();
            dom.insert(
                dom.root_ref(),
                InstanceBuilder::new("Workspace").with_child(
                    InstanceBuilder::new("Part")
                        .with_name("StudioOnlyMapPart")
                        .with_property("Anchored", true),
                ),
            );
            bytes.clear();
            if place_extension(&place).unwrap() == "rbxlx" {
                rbx_xml::to_writer_default(&mut bytes, &dom, dom.root().children()).unwrap();
            } else {
                rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
            }
            fs::write(&place, bytes).unwrap();
            Some(Self {
                _temp: temp,
                root,
                place,
            })
        }

        fn checkpoint(&self, name: &str) -> anyhow::Result<()> {
            CheckpointCommand {
                json: true,
                command: CheckpointAction::Create {
                    name: name.into(),
                    place: self.place.clone(),
                    project: self.root.clone(),
                },
            }
            .run()
        }

        fn prepare(&self, name: &str, checkpoint: &str) -> anyhow::Result<()> {
            ReleaseCommand {
                json: true,
                command: ReleaseAction::Prepare {
                    name: name.into(),
                    checkpoint: checkpoint.into(),
                    universe_id: 123,
                    place_id: 456,
                    project: self.root.clone(),
                },
            }
            .run()
        }

        fn options(&self, name: &str) -> PublishOptions {
            PublishOptions {
                name: name.into(),
                confirm_target: "123:456".into(),
                api_key_env: "FIXTURE_UNUSED".into(),
                project: self.root.clone(),
            }
        }

        fn create_workspace(
            &self,
            name: &str,
            checkpoint: Option<&str>,
            release: Option<&str>,
        ) -> anyhow::Result<()> {
            WorkspaceCommand {
                json: true,
                command: WorkspaceAction::Create {
                    name: name.into(),
                    checkpoint: checkpoint.map(Into::into),
                    release: release.map(Into::into),
                    project: self.root.clone(),
                },
            }
            .run()
        }

        fn workspaces(&self) -> PathBuf {
            self.root
                .parent()
                .unwrap()
                .join("Game With Spaces.workspaces")
        }

        fn repo(&self) -> Repository {
            Repository::open(&self.root).unwrap()
        }

        fn pack(&self, name: &str, output: &Path) {
            CheckpointCommand {
                json: true,
                command: CheckpointAction::Pack {
                    name: name.into(),
                    output: output.to_owned(),
                    project: self.root.clone(),
                },
            }
            .run()
            .unwrap();
        }
    }

    fn find_child(
        dom: &WeakDom,
        parent: rbx_dom_weak::types::Ref,
        name: &str,
    ) -> rbx_dom_weak::types::Ref {
        dom.get_by_ref(parent)
            .unwrap()
            .children()
            .iter()
            .copied()
            .find(|id| dom.get_by_ref(*id).unwrap().name == name)
            .unwrap()
    }

    fn main_script(dom: &WeakDom) -> rbx_dom_weak::types::Ref {
        let service = find_child(dom, dom.root_ref(), "ServerScriptService");
        find_child(dom, service, "Main")
    }

    #[test]
    fn checkpoint_archives_exact_bytes_and_rejects_duplicate_or_corrupt_snapshots() {
        let fixture = Fixture::new();
        fixture.checkpoint("before-update").unwrap();
        let repo = fixture.repo();
        let (manifest, bytes) = repo.checkpoint("before-update").unwrap();
        assert_eq!(bytes, fs::read(&fixture.place).unwrap());
        assert_eq!(manifest.source_scripts_verified, 1);
        assert!(!manifest.imported);
        assert!(fixture.checkpoint("before-update").is_err());
        fs::write(
            repo.state.join("checkpoints/before-update/place.rbxl"),
            b"corrupt",
        )
        .unwrap();
        assert!(repo
            .checkpoint("before-update")
            .unwrap_err()
            .to_string()
            .contains("integrity"));
    }

    #[test]
    fn checkpoint_accepts_xml_places_with_uppercase_extension() {
        let fixture = Fixture::build(&[], "native-save.RBXLX").unwrap();
        fixture.checkpoint("xml").unwrap();
        let (manifest, bytes) = fixture.repo().checkpoint("xml").unwrap();
        assert_eq!(manifest.place_file, "place.rbxlx");
        assert_eq!(bytes, fs::read(&fixture.place).unwrap());
        fixture.prepare("v1", "xml").unwrap();
        publish_with(fixture.options("v1"), false, false, |_, _, kind| {
            assert_eq!(kind, "application/xml");
            Ok(1)
        })
        .unwrap();
    }

    #[test]
    fn checkpoint_rejects_dirty_source_and_stale_studio_source() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("main.server.luau"), "print('new')\n").unwrap();
        assert!(fixture
            .checkpoint("dirty")
            .unwrap_err()
            .to_string()
            .contains("Commit"));
        git(&fixture.root, ["add", "."]).unwrap();
        git(
            &fixture.root,
            ["commit", "-m", "Update fixture (AI-assisted)"],
        )
        .unwrap();
        assert!(fixture
            .checkpoint("stale")
            .unwrap_err()
            .to_string()
            .contains("stale Source"));
    }

    #[test]
    fn checkpoint_list_skips_staging_leftovers_and_invalid_manifests() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        // A crash between staging and rename leaves a complete-looking directory.
        let leftover = repo.state.join("checkpoints/.tmpLeftover");
        fs::create_dir(&leftover).unwrap();
        fs::copy(
            repo.state.join("checkpoints/base/manifest.json"),
            leftover.join("manifest.json"),
        )
        .unwrap();
        fs::create_dir(repo.state.join("checkpoints/broken")).unwrap();
        fs::write(repo.state.join("checkpoints/broken/manifest.json"), "{").unwrap();
        fs::write(repo.state.join("releases/stray.json"), "not json").unwrap();
        let checkpoints = list_manifests(&repo.state.join("checkpoints"), true);
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0]["name"], "base");
        let releases = list_manifests(&repo.state.join("releases"), false);
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0]["name"], "v1");
        CheckpointCommand {
            json: true,
            command: CheckpointAction::List {
                verify: true,
                project: fixture.root.clone(),
            },
        }
        .run()
        .unwrap();
        ReleaseCommand {
            json: true,
            command: ReleaseAction::List {
                project: fixture.root.clone(),
            },
        }
        .run()
        .unwrap();
    }

    #[test]
    fn hotfix_workspace_uses_released_code_and_full_content_without_touching_update() {
        let fixture = Fixture::new();
        fixture.checkpoint("live-v1").unwrap();
        fixture.prepare("v1", "live-v1").unwrap();
        fs::write(
            fixture.root.join("main.server.luau"),
            "print('unfinished winter update')\n",
        )
        .unwrap();
        fixture
            .create_workspace("urgent-fix", None, Some("v1"))
            .unwrap();
        let target = fixture.workspaces().join("urgent-fix");
        // macOS exposes its temporary directory through a /var -> /private/var alias.
        let target = dunce::canonicalize(target).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("main.server.luau"))
                .unwrap()
                .replace("\r\n", "\n"),
            "print('original')\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("main.server.luau")).unwrap(),
            "print('unfinished winter update')\n"
        );
        let other = Repository::open(&target).unwrap();
        let workspace: Workspace = read_json(&other.git_dir.join("rojo/workspace.json")).unwrap();
        assert_eq!(
            fs::read(&workspace.place).unwrap(),
            fs::read(&fixture.place).unwrap()
        );
        assert!(other.checkpoint("live-v1").is_ok());
        assert!(git(&target, ["status", "--porcelain"]).unwrap().is_empty());
        assert!(workspace.place.starts_with(target.parent().unwrap()));
        assert!(!workspace.place.starts_with(&target));
    }

    #[test]
    fn workspace_can_be_created_from_the_live_release() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        // Nothing has been published yet, so there is no live release.
        assert!(fixture
            .create_workspace("early", None, Some("live"))
            .is_err());
        publish_with(fixture.options("v1"), false, false, |_, _, _| Ok(3)).unwrap();
        fixture
            .create_workspace("from-live", None, Some("live"))
            .unwrap();
        let target = dunce::canonicalize(fixture.workspaces().join("from-live")).unwrap();
        let workspace: Workspace = read_json(
            &Repository::open(&target)
                .unwrap()
                .git_dir
                .join("rojo/workspace.json"),
        )
        .unwrap();
        assert_eq!(workspace.checkpoint, "base");
        assert_eq!(fixture.repo().release("LIVE").unwrap().0.name, "v1");
    }

    #[test]
    fn workspace_apply_list_and_open_operate_on_the_workspace() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture
            .create_workspace("update", Some("base"), None)
            .unwrap();
        let target = dunce::canonicalize(fixture.workspaces().join("update")).unwrap();
        fs::write(fixture.root.join("hotfix.txt"), "fixed\n").unwrap();
        git(&fixture.root, ["add", "hotfix.txt"]).unwrap();
        git(&fixture.root, ["commit", "-m", "Hotfix (AI-assisted)"]).unwrap();
        let hotfix = git(&fixture.root, ["rev-parse", "HEAD"]).unwrap();
        WorkspaceCommand {
            json: true,
            command: WorkspaceAction::Apply {
                commit: hotfix,
                code_only: true,
                project: target.clone(),
            },
        }
        .run()
        .unwrap();
        assert_eq!(
            fs::read_to_string(target.join("hotfix.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "fixed\n"
        );

        let listed = list_workspaces(&fixture.repo()).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|entry| entry["status"] == "ok"));
        assert!(listed
            .iter()
            .any(|entry| entry["workspace"]["name"] == "update"));

        let (place, project) = workspace_place(&Repository::open(&target).unwrap()).unwrap();
        assert_eq!(fs::read(place).unwrap(), fs::read(&fixture.place).unwrap());
        assert_eq!(project, target.join("default.project.json"));
        assert!(workspace_place(&fixture.repo())
            .unwrap_err()
            .to_string()
            .contains("not a Rojo-managed workspace"));
    }

    #[test]
    fn workspace_list_reports_missing_worktrees_instead_of_failing() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture
            .create_workspace("gone", Some("base"), None)
            .unwrap();
        fs::remove_dir_all(fixture.workspaces().join("gone")).unwrap();
        let listed = list_workspaces(&fixture.repo()).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed
                .iter()
                .filter(|entry| entry["status"] == "missing")
                .count(),
            1
        );
        WorkspaceCommand {
            json: true,
            command: WorkspaceAction::List {
                project: fixture.root.clone(),
            },
        }
        .run()
        .unwrap();
    }

    #[test]
    fn failed_workspace_creation_rolls_back_only_what_it_created() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        // Blocks the saved-place directory, so creation fails after `git worktree add`.
        fs::create_dir_all(fixture.workspaces()).unwrap();
        fs::write(fixture.workspaces().join(".rojo"), "").unwrap();
        let error = fixture
            .create_workspace("retry", Some("base"), None)
            .unwrap_err();
        assert!(format!("{error:#}").contains("were removed"), "{error:#}");
        assert!(!fixture.workspaces().join("retry").exists());
        assert!(git(
            &fixture.root,
            ["show-ref", "--verify", "refs/heads/rojo/retry"]
        )
        .is_err());
        fs::remove_file(fixture.workspaces().join(".rojo")).unwrap();
        fixture
            .create_workspace("retry", Some("base"), None)
            .unwrap();

        // A branch that existed beforehand is never deleted.
        git(&fixture.root, ["branch", "rojo/taken"]).unwrap();
        assert!(fixture
            .create_workspace("taken", Some("base"), None)
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        git(
            &fixture.root,
            ["show-ref", "--verify", "refs/heads/rojo/taken"],
        )
        .unwrap();
    }

    #[test]
    fn checkpoint_export_never_overwrites_and_preserves_format() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        let output = fixture.root.parent().unwrap().join("export.rbxl");
        let export = |path| {
            CheckpointCommand {
                json: true,
                command: CheckpointAction::Export {
                    name: "base".into(),
                    output: path,
                    project: fixture.root.clone(),
                },
            }
            .run()
        };
        export(output.clone()).unwrap();
        assert_eq!(
            fs::read(&output).unwrap(),
            fs::read(&fixture.place).unwrap()
        );
        assert!(export(output).is_err());
        assert!(export(fixture.root.parent().unwrap().join("wrong.rbxlx")).is_err());
        export(fixture.root.parent().unwrap().join("upper.RBXL")).unwrap();
    }

    #[test]
    fn rejects_traversal_and_windows_reserved_names() {
        for name in [
            "",
            "..",
            "../outside",
            "a/b",
            "a\\b",
            "-flag\n",
            "-flag",
            "CON",
            "nul",
            "COM1",
            "LPT9",
            "space name",
        ] {
            assert!(validate_name(name).is_err(), "accepted {name}");
        }
        assert!(validate_name("winter-update_2").is_ok());
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        assert!(fixture
            .prepare("Live", "base")
            .unwrap_err()
            .to_string()
            .contains("reserved"));
    }

    #[test]
    fn portable_checkpoint_can_be_imported_into_a_clone_but_not_overwrite_existing_data() {
        let fixture = Fixture::new();
        fixture.checkpoint("portable").unwrap();
        let bundle = fixture.root.parent().unwrap().join("checkpoint-bundle");
        fixture.pack("portable", &bundle);
        let clone = fixture.root.parent().unwrap().join("Second clone");
        git(
            fixture.root.parent().unwrap(),
            [
                OsStr::new("clone"),
                fixture.root.as_os_str(),
                clone.as_os_str(),
            ],
        )
        .unwrap();
        let import = |project: &Path| {
            CheckpointCommand {
                json: true,
                command: CheckpointAction::Import {
                    input: bundle.clone(),
                    project: project.to_owned(),
                },
            }
            .run()
        };
        import(&clone).unwrap();
        assert!(import(&clone)
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        // The original repository still has its own checkpoint of that name.
        assert!(import(&fixture.root)
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        let (manifest, bytes) = Repository::open(&clone)
            .unwrap()
            .checkpoint("portable")
            .unwrap();
        assert_eq!(bytes, fs::read(&fixture.place).unwrap());
        assert!(manifest.imported);
        let provenance = manifest.import_provenance.unwrap();
        assert_eq!(provenance.integrity, "self-attested");
        // Only the bundle's name is kept, not where it was on this computer.
        assert_eq!(provenance.bundle, "checkpoint-bundle");
        assert!(provenance.source_commit_verified && provenance.project_file_verified);
    }

    #[test]
    fn import_rejects_bundles_with_a_bad_project_file() {
        let fixture = Fixture::new();
        fixture.checkpoint("portable").unwrap();
        let repo = fixture.repo();
        for (project_file, message) in [
            ("missing.project.json", "does not exist at"),
            ("../outside.project.json", "relative repository path"),
            ("", "relative repository path"),
        ] {
            let bundle = fixture
                .root
                .parent()
                .unwrap()
                .join(format!("bundle-{}", project_file.len()));
            fixture.pack("portable", &bundle);
            let mut manifest: serde_json::Value = read_json(&bundle.join("manifest.json")).unwrap();
            manifest["name"] = "renamed".into();
            manifest["project_file"] = project_file.into();
            fs::write(
                bundle.join("manifest.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            let error = import_checkpoint(&repo, &bundle).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
        assert!(!repo.state.join("checkpoints/renamed").exists());
    }

    #[test]
    fn import_accepts_sha256_repositories() {
        assert!(is_commit_id(&"a".repeat(40)));
        assert!(is_commit_id(&"B".repeat(64)));
        assert!(!is_commit_id(&"a".repeat(41)));
        assert!(!is_commit_id(&"g".repeat(40)));
        // Older Git versions cannot create SHA-256 repositories.
        let Some(fixture) = Fixture::build(&["--object-format=sha256"], "native-save.rbxl") else {
            return;
        };
        fixture.checkpoint("portable").unwrap();
        let repo = fixture.repo();
        let bundle = fixture.root.parent().unwrap().join("bundle");
        fixture.pack("portable", &bundle);
        fs::remove_dir_all(repo.state.join("checkpoints/portable")).unwrap();
        let imported = import_checkpoint(&repo, &bundle).unwrap();
        assert_eq!(imported.source_commit.len(), 64);
        // A SHA-1-length ID would only be an abbreviation here.
        assert!(repo
            .ensure_full_commit_id(&imported.source_commit[..40])
            .is_err());
    }

    #[test]
    fn publish_preflight_rejects_unsupported_content_and_wrong_target() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let mut options = fixture.options("v1");
        options.confirm_target = "123:999".into();
        let error = publish_with(options, false, false, |_, _, _| {
            panic!("wrong target must not publish")
        })
        .unwrap_err();
        assert!(error.to_string().contains("confirm-target"));
        for class in [
            "UnionOperation",
            "IntersectOperation",
            "SurfaceAppearance",
            "WrapLayer",
            "WrapDeformer",
            "EditableMesh",
        ] {
            let dom = WeakDom::new(
                InstanceBuilder::new("DataModel").with_child(InstanceBuilder::new(class)),
            );
            assert!(verify_cloud_publishable(&dom).is_err());
        }
        // Unknown content inside a service still blocks publishing.
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("Workspace")
                    .with_child(InstanceBuilder::new("UnknownFutureClass")),
            ),
        );
        assert!(verify_cloud_publishable(&dom)
            .unwrap_err()
            .to_string()
            .contains("unknown Roblox class"));
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(InstanceBuilder::new("Part")),
        );
        assert!(verify_cloud_publishable(&dom).unwrap().is_empty());
    }

    #[test]
    fn publish_preflight_warns_about_services_newer_than_the_database() {
        let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_child(
            InstanceBuilder::new("UnknownFutureService").with_child(InstanceBuilder::new("Part")),
        ));
        let warnings = verify_cloud_publishable(&dom).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("UnknownFutureService"));
        // Restricted content inside an unknown service is still found.
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("UnknownFutureService")
                    .with_child(InstanceBuilder::new("UnionOperation")),
            ),
        );
        assert!(verify_cloud_publishable(&dom).is_err());
    }

    #[test]
    fn source_verification_rejects_extra_owned_scripts_but_preserves_studio_owned_content() {
        let fixture = Fixture::new();
        let mut dom = decode_place(&fs::read(&fixture.place).unwrap(), "rbxl").unwrap();
        let workspace = find_child(&dom, dom.root_ref(), "Workspace");
        dom.insert(
            workspace,
            InstanceBuilder::new("Script").with_name("StudioOwned"),
        );
        assert_eq!(verify_sources(&fixture.root, &dom).unwrap().0, 1);
        let main = main_script(&dom);
        dom.insert(
            main,
            InstanceBuilder::new("Script").with_name("FutureEvent"),
        );
        assert!(verify_sources(&fixture.root, &dom)
            .unwrap_err()
            .to_string()
            .contains("unexpected executable"));
        fs::write(fixture.root.join("default.project.json"), r#"{"name":"Fixture","tree":{"$className":"DataModel","ServerScriptService":{"Main":{"$path":"main.server.luau","$ignoreUnknownInstances":true}}}}"#).unwrap();
        assert_eq!(verify_sources(&fixture.root, &dom).unwrap().0, 1);
    }

    #[test]
    fn source_verification_ignores_duplicate_scripts_in_studio_owned_subtrees() {
        let fixture = Fixture::new();
        let mut dom = decode_place(&fs::read(&fixture.place).unwrap(), "rbxl").unwrap();
        let workspace = find_child(&dom, dom.root_ref(), "Workspace");
        // Two copies of the same free model.
        for _ in 0..2 {
            dom.insert(
                workspace,
                InstanceBuilder::new("Model")
                    .with_name("NPC")
                    .with_child(InstanceBuilder::new("Script").with_name("Script")),
            );
        }
        assert_eq!(verify_sources(&fixture.root, &dom).unwrap().0, 1);
        let main = main_script(&dom);
        for _ in 0..2 {
            dom.insert(main, InstanceBuilder::new("Script").with_name("Twin"));
        }
        assert!(verify_sources(&fixture.root, &dom)
            .unwrap_err()
            .to_string()
            .contains("Ambiguous duplicate"));
    }

    #[test]
    fn source_verification_rejects_duplicates_of_mapped_scripts() {
        let fixture = Fixture::new();
        let mut dom = decode_place(&fs::read(&fixture.place).unwrap(), "rbxl").unwrap();
        let service = find_child(&dom, dom.root_ref(), "ServerScriptService");
        dom.insert(service, InstanceBuilder::new("Script").with_name("Main"));
        assert!(verify_sources(&fixture.root, &dom)
            .unwrap_err()
            .to_string()
            .contains("Ambiguous duplicate"));
        let (unique, duplicates) = script_sources(&dom).unwrap();
        assert!(unique.is_empty());
        assert_eq!(
            duplicates.into_iter().collect::<Vec<_>>(),
            [vec!["ServerScriptService".to_owned(), "Main".to_owned()]]
        );
    }

    #[test]
    fn source_verification_checks_script_execution_settings() {
        let fixture = Fixture::new();
        for (property, value) in [
            (
                "RunContext",
                Variant::Enum(rbx_dom_weak::types::Enum::from_u32(2)),
            ),
            ("Disabled", Variant::Bool(true)),
            ("Enabled", Variant::Bool(false)),
        ] {
            let mut dom = decode_place(&fs::read(&fixture.place).unwrap(), "rbxl").unwrap();
            let main = main_script(&dom);
            dom.get_by_ref_mut(main)
                .unwrap()
                .properties
                .insert(property.into(), value);
            assert!(
                verify_sources(&fixture.root, &dom)
                    .unwrap_err()
                    .to_string()
                    .contains("execution settings"),
                "{property}"
            );
        }
        // Writers that store the canonical Enabled property are not reported as stale.
        let mut dom = decode_place(&fs::read(&fixture.place).unwrap(), "rbxl").unwrap();
        let main = main_script(&dom);
        let properties = &mut dom.get_by_ref_mut(main).unwrap().properties;
        properties.remove(&rbx_dom_weak::Ustr::from("Disabled"));
        properties.insert("Enabled".into(), Variant::Bool(true));
        assert_eq!(verify_sources(&fixture.root, &dom).unwrap().0, 1);
    }

    #[test]
    fn publication_journal_records_success_and_retains_ambiguous_failure_lock() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let expected = fs::read(&fixture.place).unwrap();
        publish_with(fixture.options("v1"), false, false, |url, bytes, kind| {
            assert!(url.ends_with("/123/places/456/versions?versionType=Published"));
            assert_eq!(kind, "application/octet-stream");
            assert_eq!(bytes, expected);
            Ok(7)
        })
        .unwrap();
        let repo = fixture.repo();
        let record: serde_json::Value =
            read_json(&repo.state.join("last-publication.json")).unwrap();
        assert_eq!(record["version_number"], 7);
        assert_eq!(record["existing_servers_restarted"], false);
        assert_eq!(record["checkpoint_imported"], false);
        assert_eq!(repo.release("live").unwrap().0.name, "v1");
        assert!(!repo.state.join("publication.lock").exists());
        assert!(
            publish_with(fixture.options("v1"), true, false, |_, _, _| Err(
                UploadFailure::Unconfirmed("response lost".into())
            ))
            .is_err()
        );
        assert!(repo.state.join("publication.lock").exists());
        assert!(
            publish_with(fixture.options("v1"), true, false, |_, _, _| panic!(
                "must not retry unknown publication"
            ))
            .is_err()
        );
        let unchanged: serde_json::Value =
            read_json(&repo.state.join("last-publication.json")).unwrap();
        assert_eq!(unchanged, record);
        let receipts = list_manifests(&repo.state.join("publication-attempts"), false);
        assert_eq!(receipts.len(), 2);
        assert!(receipts.iter().any(|entry| entry["status"] == "unconfirmed"
            && entry["rollback"] == true
            && entry["error"] == "response lost"));
    }

    #[test]
    fn rejected_publication_records_failure_and_releases_the_lock() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            Err(UploadFailure::Rejected(
                "Open Cloud returned HTTP 401 Unauthorized: Invalid API key".into(),
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("did not create a version"));
        assert!(error.to_string().contains("Invalid API key"));
        assert!(!repo.state.join("publication.lock").exists());
        assert!(!repo.state.join("last-publication.json").exists());
        let receipts = list_manifests(&repo.state.join("publication-attempts"), false);
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0]["status"], "failed");
        assert!(receipts[0]["error"]
            .as_str()
            .unwrap()
            .contains("Invalid API key"));
        publish_with(fixture.options("v1"), false, false, |_, _, _| Ok(8)).unwrap();
    }

    #[test]
    fn journal_failure_before_upload_releases_the_lock() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        fs::write(repo.state.join("publication-attempts"), "").unwrap();
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            panic!("must not upload without a journal")
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("nothing was uploaded"));
        assert!(!repo.state.join("publication.lock").exists());
    }

    #[test]
    fn journal_failure_after_publication_reports_the_version() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        let attempts = repo.state.join("publication-attempts");
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            // Make the receipt impossible to update after Roblox accepted the upload.
            fs::remove_dir_all(&attempts).unwrap();
            fs::write(&attempts, "").unwrap();
            Ok(9)
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("version 9"), "{error:#}");
        assert!(format!("{error:#}").contains("--version 9"), "{error:#}");
        assert!(repo.state.join("publication.lock").exists());
    }

    #[test]
    fn journal_failure_after_the_receipt_names_the_remaining_records() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        let live = repo.state.join("live");
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            // The receipt can still be written, but the live record cannot.
            fs::write(&live, "").unwrap();
            Ok(9)
        })
        .unwrap_err();
        let message = format!("{error:#}");
        let live_record = live_record_path(&repo.state, 123, 456);
        assert!(
            message.contains(&live_record.display().to_string()),
            "{message}"
        );
        assert!(message.contains("last-publication.json"), "{message}");
        let receipt = pending_receipt(&repo);
        assert!(
            !message.contains(&receipt.display().to_string()),
            "{message}"
        );
        let attempt = receipt.file_stem().unwrap().to_str().unwrap().to_owned();
        assert!(
            message.contains(&format!("rojo release resolve {attempt} --version 9")),
            "{message}"
        );

        // Once the cause is fixed, resolve completes the records and the lock.
        fs::remove_file(&live).unwrap();
        assert!(resolve_attempt(&repo, &attempt, Resolution::Published(8), false).is_err());
        let output = resolve_attempt(&repo, &attempt, Resolution::Published(9), false).unwrap();
        assert_eq!(output["lock"], "removed");
        let record: serde_json::Value = read_json(&live_record).unwrap();
        assert_eq!(record["version_number"], 9);
        assert_eq!(repo.release("live").unwrap().0.name, "v1");
    }

    #[test]
    fn resolve_records_an_unconfirmed_publication_and_releases_its_lock() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        let lock_path = repo.state.join("publication.lock");
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            Err(UploadFailure::Unconfirmed("response lost".into()))
        })
        .unwrap_err();
        assert!(
            error.to_string().contains("rojo release resolve"),
            "{error}"
        );
        let receipt = pending_receipt(&repo);
        let attempt = receipt.file_stem().unwrap().to_str().unwrap().to_owned();

        for invalid in ["../outside", "", "not-a-uuid"] {
            assert!(resolve_attempt(&repo, invalid, Resolution::NotPublished, false).is_err());
        }
        assert!(resolve_attempt(&repo, &attempt, Resolution::Published(0), false).is_err());
        // An attempt ID that has no journal entry is refused.
        let unknown = uuid::Uuid::new_v4().to_string();
        assert!(resolve_attempt(&repo, &unknown, Resolution::Published(5), false).is_err());
        assert!(lock_path.exists());

        // Upper-case IDs name the same attempt.
        let output = resolve_attempt(
            &repo,
            &attempt.to_ascii_uppercase(),
            Resolution::Published(5),
            false,
        )
        .unwrap();
        assert_eq!(output["lock"], "removed");
        assert!(!lock_path.exists());
        let stored: serde_json::Value = read_json(&receipt).unwrap();
        assert_eq!(stored["status"], "published");
        assert_eq!(stored["version_number"], 5);
        assert_eq!(stored["error"], "response lost");
        let live: serde_json::Value = read_json(&live_record_path(&repo.state, 123, 456)).unwrap();
        assert_eq!(live["version_number"], 5);
        // Rollback only needs the resolved receipt.
        publish_with(fixture.options("v1"), true, false, |_, _, _| Ok(6)).unwrap();
        // A resolved attempt cannot be resolved differently later, and an older
        // attempt never replaces the newer record.
        assert!(resolve_attempt(&repo, &attempt, Resolution::NotPublished, false).is_err());
        let output = resolve_attempt(&repo, &attempt, Resolution::Published(5), false).unwrap();
        assert_eq!(output["kept_newer_records"].as_array().unwrap().len(), 2);
        let live: serde_json::Value = read_json(&live_record_path(&repo.state, 123, 456)).unwrap();
        assert_eq!(live["version_number"], 6);
    }

    #[test]
    fn resolve_not_published_keeps_live_and_other_attempts_locks() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        let lock_path = repo.state.join("publication.lock");
        publish_with(fixture.options("v1"), false, false, |_, _, _| {
            Err(UploadFailure::Unconfirmed("response lost".into()))
        })
        .unwrap_err();
        let receipt = pending_receipt(&repo);
        let attempt = receipt.file_stem().unwrap().to_str().unwrap().to_owned();
        // A lock that another attempt holds is never removed.
        let other = uuid::Uuid::new_v4().to_string();
        fs::write(
            &lock_path,
            serde_json::to_vec(&serde_json::json!({"attempt": other, "pid": 1})).unwrap(),
        )
        .unwrap();
        let output = resolve_attempt(&repo, &attempt, Resolution::NotPublished, false).unwrap();
        assert!(output["lock"].as_str().unwrap().starts_with("kept"));
        assert!(lock_path.exists());
        let stored: serde_json::Value = read_json(&receipt).unwrap();
        assert_eq!(stored["status"], "failed");
        assert!(!repo.state.join("live").exists());
        assert!(!repo.state.join("last-publication.json").exists());
        assert!(!has_published_receipt(
            &repo,
            &repo.release("v1").unwrap().0
        ));
    }

    #[test]
    fn damaged_legacy_publication_record_does_not_hide_live_records() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        publish_with(fixture.options("v1"), false, false, |_, _, _| Ok(1)).unwrap();
        let repo = fixture.repo();
        fs::write(repo.state.join("last-publication.json"), "{").unwrap();
        assert_eq!(repo.release("live").unwrap().0.name, "v1");
        ReleaseCommand {
            json: true,
            command: ReleaseAction::List {
                project: fixture.root.clone(),
            },
        }
        .run()
        .unwrap();
    }

    fn ref_target(root: &Path, reference: &str) -> Option<String> {
        git(root, ["rev-parse", "--verify", "--quiet", reference]).ok()
    }

    #[test]
    fn checkpoint_refs_keep_source_commits_through_history_rewrites_and_gc() {
        let fixture = Fixture::new();
        // A commit that only this checkpoint will reach once the branch is reset.
        fs::write(fixture.root.join("notes.txt"), "draft\n").unwrap();
        git(&fixture.root, ["add", "notes.txt"]).unwrap();
        git(&fixture.root, ["commit", "-m", "Draft (AI-assisted)"]).unwrap();
        let draft = git(&fixture.root, ["rev-parse", "HEAD"]).unwrap();
        fixture.checkpoint("draft").unwrap();
        assert_eq!(
            ref_target(&fixture.root, "refs/rojo/checkpoints/draft"),
            Some(draft.clone())
        );
        git(&fixture.root, ["reset", "--hard", "HEAD~1"]).unwrap();
        git(&fixture.root, ["reflog", "expire", "--expire=now", "--all"]).unwrap();
        git(&fixture.root, ["gc", "--prune=now", "--quiet"]).unwrap();
        git(
            &fixture.root,
            ["cat-file", "-e", &format!("{draft}^{{commit}}")],
        )
        .unwrap();
        fixture
            .create_workspace("restored", Some("draft"), None)
            .unwrap();
        let target = dunce::canonicalize(fixture.workspaces().join("restored")).unwrap();
        assert!(target.join("notes.txt").is_file());
    }

    #[test]
    fn checkpoint_refuses_a_ref_that_names_another_commit() {
        let fixture = Fixture::new();
        let first = git(&fixture.root, ["rev-parse", "HEAD"]).unwrap();
        fs::write(fixture.root.join("notes.txt"), "later\n").unwrap();
        git(&fixture.root, ["add", "notes.txt"]).unwrap();
        git(&fixture.root, ["commit", "-m", "Later (AI-assisted)"]).unwrap();
        git(
            &fixture.root,
            ["update-ref", "refs/rojo/checkpoints/taken", &first],
        )
        .unwrap();
        let error = fixture.checkpoint("taken").unwrap_err();
        assert!(format!("{error:#}").contains("points at"), "{error:#}");
        assert!(!fixture.repo().state.join("checkpoints/taken").exists());
        assert_eq!(
            ref_target(&fixture.root, "refs/rojo/checkpoints/taken"),
            Some(first)
        );
    }

    #[test]
    fn import_prepare_and_workspace_create_protect_checkpoint_commits() {
        let fixture = Fixture::new();
        fixture.checkpoint("portable").unwrap();
        let head = git(&fixture.root, ["rev-parse", "HEAD"]).unwrap();
        let bundle = fixture.root.parent().unwrap().join("ref-bundle");
        fixture.pack("portable", &bundle);
        let clone = fixture.root.parent().unwrap().join("Ref clone");
        git(
            fixture.root.parent().unwrap(),
            [
                OsStr::new("clone"),
                fixture.root.as_os_str(),
                clone.as_os_str(),
            ],
        )
        .unwrap();
        import_checkpoint(&Repository::open(&clone).unwrap(), &bundle).unwrap();
        assert_eq!(
            ref_target(&clone, "refs/rojo/checkpoints/portable"),
            Some(head.clone())
        );

        // Checkpoints made before these refs existed gain one when used.
        git(
            &fixture.root,
            ["update-ref", "-d", "refs/rojo/checkpoints/portable"],
        )
        .unwrap();
        fixture.prepare("v1", "portable").unwrap();
        assert_eq!(
            ref_target(&fixture.root, "refs/rojo/checkpoints/portable"),
            Some(head.clone())
        );
        git(
            &fixture.root,
            ["update-ref", "-d", "refs/rojo/checkpoints/portable"],
        )
        .unwrap();
        fixture
            .create_workspace("legacy", Some("portable"), None)
            .unwrap();
        assert_eq!(
            ref_target(&fixture.root, "refs/rojo/checkpoints/portable"),
            Some(head)
        );
    }

    #[test]
    fn commit_ids_must_match_the_repository_object_format() {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        let head = git(&fixture.root, ["rev-parse", "HEAD"]).unwrap();
        repo.ensure_full_commit_id(&head).unwrap();
        let error = repo
            .ensure_full_commit_id(&format!("{head}{}", "0".repeat(24)))
            .unwrap_err();
        assert!(error.to_string().contains("sha1"), "{error:#}");
    }

    #[test]
    fn import_rejects_oversized_manifests() {
        let fixture = Fixture::new();
        fixture.checkpoint("portable").unwrap();
        let bundle = fixture.root.parent().unwrap().join("large-bundle");
        fixture.pack("portable", &bundle);
        let mut manifest = fs::read(bundle.join("manifest.json")).unwrap();
        // Valid JSON with trailing whitespace, so only the size is wrong.
        manifest.resize(MAX_MANIFEST_BYTES as usize + 1, b' ');
        fs::write(bundle.join("manifest.json"), manifest).unwrap();
        let repo = fixture.repo();
        fs::remove_dir_all(repo.state.join("checkpoints/portable")).unwrap();
        let error = import_checkpoint(&repo, &bundle).unwrap_err();
        assert!(error.to_string().contains("1 MiB"), "{error:#}");
    }

    #[test]
    fn clean_tree_check_sees_submodule_changes_that_config_ignores() {
        let fixture = Fixture::new();
        let library = fixture.root.parent().unwrap().join("library");
        fs::create_dir(&library).unwrap();
        git(&library, ["init", "--initial-branch=main"]).unwrap();
        git(
            &library,
            ["config", "user.email", "fixture@example.invalid"],
        )
        .unwrap();
        git(&library, ["config", "user.name", "Rojo fixture"]).unwrap();
        fs::write(library.join("lib.txt"), "v1\n").unwrap();
        git(&library, ["add", "."]).unwrap();
        git(&library, ["commit", "-m", "Library (AI-assisted)"]).unwrap();
        // Recent Git refuses local submodule clones unless they are allowed.
        git(
            &fixture.root,
            [
                OsStr::new("-c"),
                OsStr::new("protocol.file.allow=always"),
                OsStr::new("submodule"),
                OsStr::new("add"),
                library.as_os_str(),
                OsStr::new("lib"),
            ],
        )
        .unwrap();
        git(&fixture.root, ["commit", "-m", "Add library (AI-assisted)"]).unwrap();
        git(&fixture.root, ["config", "submodule.lib.ignore", "all"]).unwrap();
        git(&fixture.root, ["config", "diff.ignoreSubmodules", "all"]).unwrap();
        fs::write(fixture.root.join("lib/lib.txt"), "edited\n").unwrap();
        assert!(git(&fixture.root, ["status", "--porcelain"])
            .unwrap()
            .is_empty());
        let error = fixture.checkpoint("hidden").unwrap_err();
        assert!(error.to_string().contains("submodules"), "{error:#}");
    }

    #[test]
    fn checkpoint_records_project_sources_outside_the_repository() {
        let fixture = Fixture::new();
        fixture.checkpoint("inside").unwrap();
        assert!(
            !fixture
                .repo()
                .checkpoint("inside")
                .unwrap()
                .0
                .sources_outside_repository
        );
        let shared = fixture.root.parent().unwrap().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::write(shared.join("data.txt"), "shared data").unwrap();
        fs::write(fixture.root.join("default.project.json"), r#"{"name":"Fixture","tree":{"$className":"DataModel","ServerScriptService":{"Main":{"$path":"main.server.luau"}},"ReplicatedStorage":{"Data":{"$path":"../shared/data.txt"}}}}"#).unwrap();
        git(&fixture.root, ["add", "."]).unwrap();
        git(&fixture.root, ["commit", "-m", "Share data (AI-assisted)"]).unwrap();
        fixture.checkpoint("outside").unwrap();
        let (manifest, _) = fixture.repo().checkpoint("outside").unwrap();
        assert!(manifest.sources_outside_repository);
    }

    #[test]
    fn workspace_open_only_hands_place_files_to_the_os() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture
            .create_workspace("editable", Some("base"), None)
            .unwrap();
        let target = dunce::canonicalize(fixture.workspaces().join("editable")).unwrap();
        let repo = Repository::open(&target).unwrap();
        let metadata = repo.git_dir.join("rojo/workspace.json");
        let mut workspace: serde_json::Value = read_json(&metadata).unwrap();
        let program = fixture.root.join("main.server.luau");
        workspace["place"] = program.to_str().unwrap().into();
        fs::write(&metadata, serde_json::to_vec(&workspace).unwrap()).unwrap();
        let error = workspace_place(&repo).unwrap_err();
        assert!(format!("{error:#}").contains(".rbxl"), "{error:#}");
    }

    #[test]
    fn workspaces_of_bare_repositories_drop_the_git_suffix() {
        let base = Path::new("games");
        assert_eq!(
            workspaces_directory(&base.join("game"), false),
            Some(base.join("game.workspaces"))
        );
        assert_eq!(
            workspaces_directory(&base.join("game.git"), true),
            Some(base.join("game.workspaces"))
        );
        assert_eq!(
            workspaces_directory(&base.join("game").join(".git"), true),
            Some(base.join("game.workspaces"))
        );
        // Only bare repositories are renamed; a checkout may end in .git.
        assert_eq!(
            workspaces_directory(&base.join("site.git"), false),
            Some(base.join("site.git.workspaces"))
        );
    }

    #[test]
    fn lifecycle_command_definitions_are_valid() {
        use clap::CommandFactory;
        CheckpointCommand::command().debug_assert();
        WorkspaceCommand::command().debug_assert();
        ReleaseCommand::command().debug_assert();
        let id = uuid::Uuid::new_v4().to_string();
        let parse = |args: &[&str]| {
            ReleaseCommand::try_parse_from(["release", "resolve", id.as_str()].iter().chain(args))
        };
        assert!(parse(&["--version", "3"]).is_ok());
        assert!(parse(&["--not-published"]).is_ok());
        assert!(parse(&[]).is_err());
        assert!(parse(&["--version", "3", "--not-published"]).is_err());
    }

    #[test]
    fn ssh_runs_in_batch_mode_without_replacing_other_programs() {
        let none = || None;
        assert_eq!(
            batch_ssh(None, false, none).as_deref(),
            Some("ssh -o BatchMode=yes")
        );
        assert_eq!(
            batch_ssh(Some("ssh -i key".into()), false, none).as_deref(),
            Some("ssh -i key -o BatchMode=yes")
        );
        assert_eq!(
            batch_ssh(
                Some("\"C:/Program Files/Git/usr/bin/ssh.exe\" -i key".into()),
                true,
                none
            )
            .as_deref(),
            Some("\"C:/Program Files/Git/usr/bin/ssh.exe\" -i key -o BatchMode=yes")
        );
        assert_eq!(
            batch_ssh(None, false, || Some("/usr/bin/ssh -F config".into())).as_deref(),
            Some("/usr/bin/ssh -F config -o BatchMode=yes")
        );
        // GIT_SSH programs and non-OpenSSH commands are left to the user.
        assert_eq!(batch_ssh(None, true, none), None);
        assert_eq!(batch_ssh(Some("plink -batch".into()), false, none), None);
        assert_eq!(
            batch_ssh(None, false, || Some("tortoiseplink".into())),
            None
        );
    }

    #[test]
    fn rollback_requires_an_earlier_successful_publication() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let error = publish_with(fixture.options("v1"), true, false, |_, _, _| {
            panic!("unpublished release must not be rolled back to")
        })
        .unwrap_err();
        assert!(error.to_string().contains("no successful publication"));
        publish_with(fixture.options("v1"), true, true, |_, _, _| Ok(2)).unwrap();
        publish_with(fixture.options("v1"), true, false, |_, _, _| Ok(3)).unwrap();
    }

    #[test]
    fn imported_checkpoints_are_marked_in_publication_receipts() {
        let fixture = Fixture::new();
        fixture.checkpoint("portable").unwrap();
        let bundle = fixture.root.parent().unwrap().join("bundle");
        fixture.pack("portable", &bundle);
        let repo = fixture.repo();
        fs::remove_dir_all(repo.state.join("checkpoints/portable")).unwrap();
        import_checkpoint(&repo, &bundle).unwrap();
        fixture.prepare("v1", "portable").unwrap();
        publish_with(fixture.options("v1"), false, false, |_, _, _| Ok(4)).unwrap();
        let record: serde_json::Value =
            read_json(&repo.state.join("last-publication.json")).unwrap();
        assert_eq!(record["checkpoint_imported"], true);
        assert_eq!(record["import_provenance"]["integrity"], "self-attested");
    }

    /// Serves one HTTP response on loopback and checks the request it received.
    fn serve_once(
        status: &'static str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                if request
                    .windows(4)
                    .position(|s| s == b"\r\n\r\n")
                    .is_some_and(|i| request.len() >= i + 4 + 6)
                {
                    break;
                }
            }
            assert!(request.ends_with(b"\x00place"));
            let text = String::from_utf8_lossy(&request);
            assert!(text.contains("x-api-key: fixture-key"));
            assert!(text.contains("application/octet-stream"));
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        (format!("http://{address}/versions"), handle)
    }

    fn upload_fixture(url: &str) -> Result<u64, UploadFailure> {
        upload_bytes(
            url,
            "fixture-key",
            b"\x00place".to_vec(),
            "application/octet-stream",
        )
    }

    #[test]
    fn publisher_sends_exact_artifact_and_requires_valid_receipt() {
        for (response, succeeds) in [
            (r#"{"versionNumber":42}"#, true),
            (r#"{"versionNumber":0}"#, false),
            ("{}", false),
            ("not json", false),
        ] {
            let (url, handle) = serve_once("200 OK", response);
            let result = upload_fixture(&url);
            match result {
                Ok(version) => {
                    assert!(succeeds);
                    assert_eq!(version, 42);
                }
                // A success status without a usable receipt may still have published.
                Err(failure) => {
                    assert!(!succeeds);
                    assert!(matches!(failure, UploadFailure::Unconfirmed(_)));
                }
            }
            handle.join().unwrap();
        }
    }

    #[test]
    fn publisher_classifies_http_errors_by_whether_a_version_may_exist() {
        let (url, handle) = serve_once("401 Unauthorized", r#"{"message":"Invalid API Key"}"#);
        match upload_fixture(&url) {
            Err(UploadFailure::Rejected(message)) => {
                assert!(message.contains("401"), "{message}");
                assert!(message.contains("Invalid API Key"), "{message}");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        handle.join().unwrap();

        let (url, handle) = serve_once("429 Too Many Requests", "");
        assert!(matches!(
            upload_fixture(&url),
            Err(UploadFailure::Rejected(_))
        ));
        handle.join().unwrap();

        // These client errors can arrive after Roblox acted on the upload.
        for status in [
            "408 Request Timeout",
            "409 Conflict",
            "499 Client Closed Request",
        ] {
            let (url, handle) = serve_once(status, "");
            assert!(
                matches!(upload_fixture(&url), Err(UploadFailure::Unconfirmed(_))),
                "{status}"
            );
            handle.join().unwrap();
        }

        let (url, handle) = serve_once("500 Internal Server Error", "upstream failed");
        match upload_fixture(&url) {
            Err(UploadFailure::Unconfirmed(message)) => {
                assert!(message.contains("500"), "{message}");
                assert!(message.contains("upstream failed"), "{message}");
            }
            other => panic!("expected an unconfirmed result, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn publisher_reports_unsent_requests_as_rejected() {
        // Nothing listens on a port that was just released.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        assert!(matches!(
            upload_fixture(&format!("http://{address}/versions")),
            Err(UploadFailure::Rejected(_))
        ));
        assert!(matches!(
            upload_bytes(
                "http://127.0.0.1:1/versions",
                "bad\nkey",
                Vec::new(),
                "application/octet-stream"
            ),
            Err(UploadFailure::Rejected(_))
        ));
    }

    #[test]
    fn upload_timeout_grows_with_place_size() {
        assert_eq!(upload_timeout(0), Duration::from_secs(120));
        assert_eq!(
            upload_timeout(MAX_PLACE_BYTES as usize),
            Duration::from_secs(120 + 2048)
        );
    }

    /// Rewrites fields of a packed bundle's manifest.
    fn edit_manifest(bundle: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
        let mut manifest: serde_json::Value = read_json(&bundle.join("manifest.json")).unwrap();
        edit(&mut manifest);
        fs::write(
            bundle.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn repository_paths_use_forward_slashes_on_every_os() {
        assert_eq!(
            repository_path(&Path::new("game").join("default.project.json")).unwrap(),
            "game/default.project.json"
        );
        assert_eq!(
            normalize_repository_path("game\\nested\\default.project.json").unwrap(),
            "game/nested/default.project.json"
        );
        for path in [
            "",
            "/etc/default.project.json",
            "\\\\server\\share\\default.project.json",
            "C:\\game\\default.project.json",
            "C:default.project.json",
            "game\\..\\..\\outside.project.json",
            "./default.project.json",
            "game//default.project.json",
        ] {
            assert!(normalize_repository_path(path).is_err(), "accepted {path}");
        }
        assert_eq!(
            native_path(Path::new("root"), "game/default.project.json"),
            Path::new("root").join("game").join("default.project.json")
        );
    }

    #[test]
    fn import_accepts_windows_separators_in_a_subdirectory_project_path() {
        let fixture = Fixture::new();
        fixture.checkpoint("portable").unwrap();
        let bundle = fixture.root.parent().unwrap().join("windows-bundle");
        fixture.pack("portable", &bundle);
        // A project in a subdirectory, as packed by an earlier Windows build.
        fs::create_dir(fixture.root.join("game")).unwrap();
        fs::copy(
            fixture.root.join("default.project.json"),
            fixture.root.join("game/default.project.json"),
        )
        .unwrap();
        git(&fixture.root, ["add", "."]).unwrap();
        git(
            &fixture.root,
            ["commit", "-m", "Move project (AI-assisted)"],
        )
        .unwrap();
        let head = git(&fixture.root, ["rev-parse", "HEAD"]).unwrap();
        edit_manifest(&bundle, |manifest| {
            manifest["name"] = "windows".into();
            manifest["source_commit"] = head.into();
            manifest["project_file"] = "game\\default.project.json".into();
        });
        let imported = import_checkpoint(&fixture.repo(), &bundle).unwrap();
        assert_eq!(imported.project_file, "game/default.project.json");
        let stored: serde_json::Value = read_json(
            &fixture
                .repo()
                .state
                .join("checkpoints/windows/manifest.json"),
        )
        .unwrap();
        assert_eq!(stored["project_file"], "game/default.project.json");
    }

    #[test]
    fn checkpoint_rejects_a_manifest_commit_that_is_not_an_id() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        let repo = fixture.repo();
        let directory = repo.state.join("checkpoints/base");
        edit_manifest(&directory, |manifest| {
            manifest["source_commit"] = "--orphan=evil".into();
        });
        let error = repo.checkpoint("base").unwrap_err();
        assert!(
            error.to_string().contains("Invalid Git commit"),
            "{error:#}"
        );
        assert!(fixture
            .create_workspace("evil", Some("base"), None)
            .is_err());
    }

    #[test]
    fn checkpoint_list_verify_checks_the_directory_name() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        let repo = fixture.repo();
        fs::rename(
            repo.state.join("checkpoints/base"),
            repo.state.join("checkpoints/renamed"),
        )
        .unwrap();
        let listed = list_checkpoints(&repo, true);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["verified"], false);
        assert!(listed[0]["verification_error"]
            .as_str()
            .unwrap()
            .contains("stored as renamed"));
    }

    #[test]
    fn package_hashes_include_manifests_up_to_the_repository_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("places/game")).unwrap();
        // Outside the repository, so never recorded.
        fs::write(temp.path().join("wally.toml"), "outside").unwrap();
        fs::write(root.join("wally.toml"), "root").unwrap();
        fs::write(root.join("wally.lock"), "root lock").unwrap();
        fs::write(root.join("places/pesde.toml"), "middle").unwrap();
        fs::write(root.join("places/game/rokit.toml"), "project").unwrap();
        let project = root.join("places/game/default.project.json");
        fs::write(&project, "{}").unwrap();
        let hashes = package_file_hashes(&root, &project).unwrap();
        assert_eq!(
            hashes.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "places/game/rokit.toml",
                "places/pesde.toml",
                "wally.lock",
                "wally.toml"
            ]
        );
        assert_eq!(hashes["wally.toml"], hash(b"root"));
    }

    #[test]
    fn live_is_tracked_per_target_and_must_be_explicit_when_ambiguous() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("lobby-v1", "base").unwrap();
        ReleaseCommand {
            json: true,
            command: ReleaseAction::Prepare {
                name: "arena-v1".into(),
                checkpoint: "base".into(),
                universe_id: 123,
                place_id: 789,
                project: fixture.root.clone(),
            },
        }
        .run()
        .unwrap();
        publish_with(fixture.options("lobby-v1"), false, false, |_, _, _| Ok(1)).unwrap();
        let repo = fixture.repo();
        // One published target: plain live is unambiguous.
        assert_eq!(repo.release("live").unwrap().0.name, "lobby-v1");
        let mut arena = fixture.options("arena-v1");
        arena.confirm_target = "123:789".into();
        publish_with(arena, false, false, |_, _, _| Ok(2)).unwrap();
        let error = repo.release("live").unwrap_err();
        assert!(
            error.to_string().contains("live:123:456")
                && error.to_string().contains("live:123:789"),
            "{error:#}"
        );
        assert_eq!(repo.release("live:123:456").unwrap().0.name, "lobby-v1");
        assert_eq!(repo.release("LIVE:123:789").unwrap().0.name, "arena-v1");
        assert!(repo.release("live:1:2").is_err());
        assert!(repo.release("live:123").is_err());
        fixture
            .create_workspace("arena-fix", None, Some("live:123:789"))
            .unwrap();
        assert!(fixture
            .create_workspace("ambiguous", None, Some("live"))
            .is_err());
    }

    #[test]
    fn legacy_last_publication_still_resolves_live() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        publish_with(fixture.options("v1"), false, false, |_, _, _| Ok(1)).unwrap();
        let repo = fixture.repo();
        // Publications recorded before per-target pointers existed.
        fs::remove_dir_all(repo.state.join("live")).unwrap();
        assert_eq!(repo.release("live").unwrap().0.name, "v1");
    }

    /// Finds the journal entry of the attempt currently in progress.
    fn pending_receipt(repo: &Repository) -> PathBuf {
        let mut entries: Vec<_> = fs::read_dir(repo.state.join("publication-attempts"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1);
        entries.pop().unwrap()
    }

    #[test]
    fn publication_lock_names_its_attempt_and_process() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        let lock_path = repo.state.join("publication.lock");
        publish_with(fixture.options("v1"), false, false, |_, _, _| {
            let holder: serde_json::Value = read_json(&lock_path).unwrap();
            assert_eq!(holder["pid"], std::process::id());
            let receipt = pending_receipt(&repo);
            assert_eq!(
                receipt.file_stem().unwrap().to_str().unwrap(),
                holder["attempt"].as_str().unwrap()
            );
            Err(UploadFailure::Unconfirmed("response lost".into()))
        })
        .unwrap_err();
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            panic!("must not publish while locked")
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Inspect") && error.contains(".json"),
            "{error}"
        );

        // A lock whose attempt never reached the journal.
        fs::write(
            &lock_path,
            serde_json::to_vec(
                &serde_json::json!({"attempt": uuid::Uuid::new_v4().to_string(), "pid": 1}),
            )
            .unwrap(),
        )
        .unwrap();
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            panic!("must not publish while locked")
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("no journal entry"), "{error}");

        // Locks from earlier versions are empty.
        fs::write(&lock_path, "").unwrap();
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            panic!("must not publish while locked")
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("does not name its attempt"), "{error}");
    }

    #[test]
    fn unrecorded_unconfirmed_publication_keeps_the_main_message() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            // A directory in place of the receipt makes updating it fail.
            let receipt = pending_receipt(&repo);
            fs::remove_file(&receipt).unwrap();
            fs::create_dir(&receipt).unwrap();
            Err(UploadFailure::Unconfirmed("response lost".into()))
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("Publication not confirmed: response lost"),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("still reads pending"),
            "{error:#}"
        );
        assert!(repo.state.join("publication.lock").exists());
    }

    #[test]
    fn unreleased_lock_after_rejection_keeps_the_main_message() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        let lock_path = repo.state.join("publication.lock");
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            // A non-empty directory cannot be removed as a file.
            fs::remove_file(&lock_path).unwrap();
            fs::create_dir_all(lock_path.join("blocker")).unwrap();
            Err(UploadFailure::Rejected("HTTP 403 Forbidden".into()))
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("Publication failed: HTTP 403 Forbidden"),
            "{error:#}"
        );
        assert!(error.to_string().contains("could not remove"), "{error:#}");
    }

    #[test]
    fn git_commands_ignore_inherited_repository_variables_and_never_prompt() {
        let command = git_command(Path::new("."));
        let envs: BTreeMap<_, _> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|v| v.to_owned()),
                )
            })
            .collect();
        for name in INHERITED_GIT_VARIABLES {
            assert_eq!(envs.get(*name), Some(&None), "{name}");
        }
        assert_eq!(envs["GIT_TERMINAL_PROMPT"], Some("0".into()));
        assert_eq!(envs["GCM_INTERACTIVE"], Some("never".into()));
    }

    #[test]
    fn old_git_output_for_path_format_is_rejected() {
        // Git before 2.31 echoes the unknown option instead of failing.
        assert!(parse_common_dir("--path-format=absolute\n.git").is_none());
        assert!(parse_common_dir(".git").is_none());
        let absolute = std::path::absolute("repo/.git").unwrap();
        assert_eq!(
            parse_common_dir(absolute.to_str().unwrap()),
            Some(absolute.clone())
        );
    }

    #[test]
    fn place_reads_reject_empty_files() {
        let temp = tempfile::tempdir().unwrap();
        let place = temp.path().join("empty.rbxl");
        fs::write(&place, "").unwrap();
        assert!(read_place_bytes(&place).is_err());
        fs::write(&place, "x").unwrap();
        assert_eq!(read_place_bytes(&place).unwrap(), b"x");
    }

    #[test]
    fn failed_archive_keeps_a_ref_that_a_concurrent_checkpoint_relies_on() {
        let fixture = Fixture::new();
        fixture.checkpoint("shared").unwrap();
        let repo = fixture.repo();
        let (checkpoint, bytes) = repo.checkpoint("shared").unwrap();
        let reference = "refs/rojo/checkpoints/shared";
        let destination = repo.state.join("checkpoints/shared");
        // This call creates the ref, then finds the destination already
        // archived by another writer for the same commit.
        git(&fixture.root, ["update-ref", "-d", reference]).unwrap();
        assert!(repo
            .archive_protected_checkpoint(&destination, &checkpoint, &bytes)
            .is_err());
        assert_eq!(
            ref_target(&fixture.root, reference),
            Some(checkpoint.source_commit.clone())
        );

        // A destination without a matching manifest does not need the ref.
        let mut other = checkpoint.clone();
        other.name = "blocked".into();
        let blocked = repo.state.join("checkpoints/blocked");
        fs::create_dir(&blocked).unwrap();
        assert!(repo
            .archive_protected_checkpoint(&blocked, &other, &bytes)
            .is_err());
        assert_eq!(
            ref_target(&fixture.root, "refs/rojo/checkpoints/blocked"),
            None
        );
    }

    #[test]
    fn names_that_differ_only_in_case_are_rejected() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        let error = fixture.checkpoint("BASE").unwrap_err();
        assert!(format!("{error:#}").contains("only in case"), "{error:#}");
        // `exists` would also see `base` on a case-insensitive file system.
        assert_eq!(
            existing_names(&fixture.repo().state.join("checkpoints"), None),
            ["base"]
        );

        fixture.prepare("v1", "base").unwrap();
        let error = fixture.prepare("V1", "base").unwrap_err();
        assert!(format!("{error:#}").contains("only in case"), "{error:#}");

        fixture
            .create_workspace("hotfix", Some("base"), None)
            .unwrap();
        let error = fixture
            .create_workspace("HotFix", Some("base"), None)
            .unwrap_err();
        assert!(format!("{error:#}").contains("only in case"), "{error:#}");
        assert_eq!(
            git(
                &fixture.root,
                ["for-each-ref", "--format=%(refname)", "refs/heads/rojo/"]
            )
            .unwrap(),
            "refs/heads/rojo/hotfix"
        );
    }

    #[test]
    fn failed_worktree_add_does_not_claim_to_remove_what_it_never_created() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        // A branch named `rojo` makes `rojo/<name>` impossible to create.
        git(&fixture.root, ["branch", "rojo"]).unwrap();
        let error = fixture
            .create_workspace("blocked", Some("base"), None)
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("before Git created"), "{message}");
        assert!(!message.contains("were removed"), "{message}");
        assert!(!fixture.workspaces().join("blocked").exists());
    }

    #[test]
    fn resolve_refuses_a_pending_attempt_whose_lock_may_still_be_held() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        let lock_path = repo.state.join("publication.lock");
        publish_with(fixture.options("v1"), false, false, |_, _, _| {
            Err(UploadFailure::Unconfirmed("response lost".into()))
        })
        .unwrap_err();
        let receipt = pending_receipt(&repo);
        let attempt = receipt.file_stem().unwrap().to_str().unwrap().to_owned();
        // As if the publishing process were still uploading.
        let mut stored: serde_json::Value = read_json(&receipt).unwrap();
        stored["status"] = "pending".into();
        fs::write(&receipt, serde_json::to_vec(&stored).unwrap()).unwrap();

        let error = resolve_attempt(&repo, &attempt, Resolution::NotPublished, false).unwrap_err();
        assert!(format!("{error:#}").contains("--force"), "{error:#}");
        assert!(lock_path.exists());
        let unchanged: serde_json::Value = read_json(&receipt).unwrap();
        assert_eq!(unchanged["status"], "pending");

        let output = resolve_attempt(&repo, &attempt, Resolution::NotPublished, true).unwrap();
        assert_eq!(output["lock"], "removed");

        // A lock older than any publication can run no longer blocks.
        let old = now() - max_publication_duration().as_secs();
        fs::write(
            &lock_path,
            serde_json::to_vec(
                &serde_json::json!({"attempt": attempt, "pid": 1, "created_unix": old}),
            )
            .unwrap(),
        )
        .unwrap();
        ensure_publisher_finished(&lock_path, &attempt).unwrap();
        // Another attempt's lock never blocks this one.
        let fresh = serde_json::json!({"attempt": uuid::Uuid::new_v4().to_string(), "pid": 1, "created_unix": now()});
        fs::write(&lock_path, serde_json::to_vec(&fresh).unwrap()).unwrap();
        ensure_publisher_finished(&lock_path, &attempt).unwrap();
    }

    #[test]
    fn rejected_publication_with_an_unwritable_journal_names_the_stale_entry() {
        let fixture = Fixture::new();
        fixture.checkpoint("base").unwrap();
        fixture.prepare("v1", "base").unwrap();
        let repo = fixture.repo();
        let attempts = repo.state.join("publication-attempts");
        let error = publish_with(fixture.options("v1"), false, false, |_, _, _| {
            // A directory in place of the receipt cannot be replaced by a file.
            let receipt = fs::read_dir(&attempts)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            fs::remove_file(&receipt).unwrap();
            fs::create_dir(&receipt).unwrap();
            Err(UploadFailure::Rejected("HTTP 401".into()))
        })
        .unwrap_err();
        let message = format!("{error:#}");
        let receipt = pending_receipt(&repo);
        assert!(
            message.contains(&receipt.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains("never resolve it as published"),
            "{message}"
        );
        assert!(!repo.state.join("publication.lock").exists());
    }

    /// Serves `replies` (status line and JSON body) in order on loopback, one
    /// connection each, standing in for Open Cloud's Luau Execution API.
    /// Returns its base URL and the request lines and bodies it received.
    fn serve_cloud(
        replies: Vec<(&'static str, String)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = std::sync::Arc::clone(&recorded);
        // Not joined: a client that makes fewer requests fails on the count.
        std::thread::spawn(move || {
            for (status, body) in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
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
                let request_line = head.lines().next().unwrap().to_owned();
                let request_body = String::from_utf8_lossy(&data[head_end..]).into_owned();
                log.lock()
                    .unwrap()
                    .push(format!("{request_line}\n{request_body}"));
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (format!("http://{address}"), recorded)
    }

    const TASK_PATH: &str = "universes/9/places/99/versions/7/luau-execution-sessions/s/tasks/t";

    /// A finished task as Luau Execution reports it, followed by its logs.
    fn finished_task(state: &str, results: &str, error: &str) -> Vec<(&'static str, String)> {
        vec![
            (
                "200 OK",
                format!(
                    r#"{{"path":"{TASK_PATH}","state":"{state}","output":{{"results":{results}}}{error}}}"#
                ),
            ),
            (
                "200 OK",
                r#"{"luauExecutionSessionTaskLogs":[{"structuredMessages":[{"message":"smoke ran","messageType":"OUTPUT"}]}]}"#
                    .to_owned(),
            ),
        ]
    }

    struct Staging {
        fixture: Fixture,
        script: PathBuf,
    }

    impl Staging {
        fn new() -> Self {
            let fixture = Fixture::new();
            fixture.checkpoint("base").unwrap();
            fixture.prepare("v1", "base").unwrap();
            let script = fixture.root.parent().unwrap().join("smoke.luau");
            fs::write(&script, "return { ok = workspace ~= nil }\n").unwrap();
            Self { fixture, script }
        }

        fn options(&self, staging: &str, smoke: bool) -> StageOptions {
            StageOptions {
                name: "v1".into(),
                staging: staging.into(),
                smoke: smoke.then(|| self.script.clone()),
                smoke_timeout: Some(30),
                api_key_env: "FIXTURE_UNUSED".into(),
                project: self.fixture.root.clone(),
            }
        }

        /// Stages v1 to 9:99, running the smoke check against `replies`.
        fn stage(
            &self,
            replies: Vec<(&'static str, String)>,
        ) -> (anyhow::Result<serde_json::Value>, Vec<String>) {
            let (base, recorded) = serve_cloud(replies);
            let cloud = OpenCloud::with_base_url("fixture-key", &base).unwrap();
            let result = stage_with(
                self.options("9:99", true),
                |url, _, _| {
                    assert!(
                        url.ends_with("/universes/v1/9/places/99/versions?versionType=Published")
                    );
                    Ok(7)
                },
                Box::new(|universe, place, version, script, timeout| {
                    cloud.run_luau(universe, place, Some(version), script, timeout)
                }),
            );
            let requests = recorded.lock().unwrap().clone();
            (result, requests)
        }

        fn promote(&self, allow_unsmoked: bool) -> anyhow::Result<u64> {
            let mut uploaded = None;
            promote_with(
                PromoteOptions {
                    publish: self.fixture.options("v1"),
                    max_age_hours: 24,
                    allow_unsmoked,
                },
                |url, _, _| {
                    uploaded = Some(url.to_owned());
                    Ok(8)
                },
            )?;
            let url = uploaded.expect("promote published without uploading");
            assert!(url.ends_with("/universes/v1/123/places/456/versions?versionType=Published"));
            Ok(8)
        }

        fn staging_records(&self) -> Vec<(PathBuf, serde_json::Value)> {
            list_manifest_entries(&self.fixture.repo().state.join("staging"), false)
        }

        fn edit_latest(&self, edit: impl FnOnce(&mut serde_json::Value)) {
            let (path, mut record) = self.staging_records().pop().unwrap();
            edit(&mut record);
            fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
        }
    }

    #[test]
    fn stage_publishes_to_the_test_place_and_records_a_passing_smoke_check() {
        let staging = Staging::new();
        let (result, requests) = staging.stage(finished_task("COMPLETE", r#"[{"ok":true}]"#, ""));
        let record = result.unwrap();
        assert_eq!(record["status"], "passed");
        assert_eq!(record["version_number"], 7);
        assert_eq!(record["universe_id"], 9);
        assert_eq!(record["place_id"], 99);
        assert_eq!(record["release_target"], "123:456");
        assert_eq!(record["smoke"]["logs"][0]["message"], "smoke ran");
        // The returned values are kept as JSON, not as JSON text.
        assert_eq!(
            record["smoke"]["results"],
            serde_json::json!([{"ok": true}])
        );
        assert_eq!(record["smoke"]["results_truncated"], false);
        // The smoke check runs against the version that staging just created.
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with(
            "POST /cloud/v2/universes/9/places/99/versions/7/luau-execution-session-tasks "
        ));
        assert!(requests[0].contains(r#""timeout":"30s""#));
        assert!(requests[0].contains("workspace ~= nil"));

        let repo = staging.fixture.repo();
        let receipts = list_manifests(&repo.state.join("publication-attempts"), false);
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0]["purpose"], "staging");
        assert_eq!(receipts[0]["status"], "published");
        assert_eq!(receipts[0]["place_id"], 99);
        // Staging is not a live publication: no live pointer and no lock.
        assert!(!repo.state.join("live").exists());
        assert!(!repo.state.join("last-publication.json").exists());
        assert!(!repo.state.join("publication.lock").exists());
        assert!(repo.release("live").is_err());
        assert!(!has_published_receipt(
            &repo,
            &repo.release("v1").unwrap().0
        ));
    }

    #[test]
    fn promote_after_a_passing_stage_publishes_live_like_release_publish() {
        let staging = Staging::new();
        let (result, _) = staging.stage(finished_task("COMPLETE", "[true]", ""));
        let attempt = result.unwrap()["attempt"].clone();
        staging.promote(false).unwrap();

        let repo = staging.fixture.repo();
        let record: serde_json::Value =
            read_json(&live_record_path(&repo.state, 123, 456)).unwrap();
        assert_eq!(record["purpose"], "promote");
        assert_eq!(record["version_number"], 8);
        assert_eq!(record["promoted_from_staging"], attempt);
        assert_eq!(repo.release("live").unwrap().0.name, "v1");
        assert!(!repo.state.join("publication.lock").exists());
        // The normal publish path's receipt also lets rollback find it.
        assert!(has_published_receipt(&repo, &repo.release("v1").unwrap().0));
    }

    #[test]
    fn promote_requires_the_exact_live_target() {
        let staging = Staging::new();
        staging
            .stage(finished_task("COMPLETE", "[true]", ""))
            .0
            .unwrap();
        let mut options = staging.fixture.options("v1");
        options.confirm_target = "9:99".into();
        let error = promote_with(
            PromoteOptions {
                publish: options,
                max_age_hours: 24,
                allow_unsmoked: false,
            },
            |_, _, _| panic!("must not publish"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("--confirm-target"), "{error}");
    }

    #[test]
    fn failing_smoke_check_blocks_promote() {
        let staging = Staging::new();
        let (result, _) = staging.stage(finished_task(
            "COMPLETE",
            r#"[{"ok":false,"message":"no spawn"}]"#,
            "",
        ));
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("returned ok = false: no spawn"), "{error}");
        assert!(error.contains("live place was not changed"), "{error}");
        assert_eq!(staging.staging_records()[0].1["status"], "failed");
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("did not pass its smoke check"), "{error}");
        assert!(!staging.fixture.repo().state.join("live").exists());
    }

    #[test]
    fn a_later_failing_stage_blocks_an_earlier_pass() {
        let staging = Staging::new();
        staging
            .stage(finished_task("COMPLETE", "[true]", ""))
            .0
            .unwrap();
        // Make sure the failing stage is strictly later than the pass.
        staging.edit_latest(|record| record["staged_unix"] = (now() - 10).into());
        assert!(staging
            .stage(finished_task("COMPLETE", "[false]", ""))
            .0
            .is_err());
        assert!(staging.promote(false).is_err());
    }

    #[test]
    fn smoke_script_errors_and_timeouts_block_promote() {
        for (task, expected) in [
            (
                finished_task(
                    "FAILED",
                    "[]",
                    r#","error":{"code":"SCRIPT_ERROR","message":"attempt to index nil"}"#,
                ),
                "stopped with SCRIPT_ERROR: attempt to index nil",
            ),
            (
                finished_task("FAILED", "[]", r#","error":{"code":"DEADLINE_EXCEEDED"}"#),
                "did not finish within --smoke-timeout",
            ),
        ] {
            let staging = Staging::new();
            let error = format!("{:#}", staging.stage(task).0.unwrap_err());
            assert!(error.contains(expected), "{error}");
            assert_eq!(staging.staging_records()[0].1["status"], "failed");
            assert!(staging.promote(false).is_err());
        }
    }

    #[test]
    fn open_cloud_errors_during_the_smoke_check_block_promote() {
        let staging = Staging::new();
        let (result, requests) = staging.stage(vec![(
            "500 Internal Server Error",
            r#"{"code":"INTERNAL"}"#.into(),
        )]);
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("Smoke check error"), "{error}");
        assert!(!error.contains("fixture-key"), "{error}");
        // A POST that may have created a task is not repeated.
        assert_eq!(requests.len(), 1);
        let (_, record) = staging.staging_records().pop().unwrap();
        assert_eq!(record["status"], "error");
        assert_eq!(record["smoke"]["task_may_have_run"], true);
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("(error)"), "{error}");
    }

    #[test]
    fn promote_refuses_missing_old_or_mismatched_staging_records() {
        let staging = Staging::new();
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("has no staging record"), "{error}");

        staging
            .stage(finished_task("COMPLETE", "[true]", ""))
            .0
            .unwrap();
        staging.edit_latest(|record| record["staged_unix"] = (now() - 25 * 3600).into());
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("25 hours old"), "{error}");

        staging.edit_latest(|record| {
            record["staged_unix"] = now().into();
            record["hash"] = "0".repeat(64).into();
        });
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("different place bytes"), "{error}");

        // An unreadable record could be the latest failure, so it is not skipped.
        let (path, _) = staging.staging_records().pop().unwrap();
        fs::write(&path, b"not json").unwrap();
        let error = format!("{:#}", staging.promote(false).unwrap_err());
        assert!(error.contains("Cannot check staging records"), "{error}");

        // A record for another release never counts.
        fs::remove_file(&path).unwrap();
        staging
            .stage(finished_task("COMPLETE", "[true]", ""))
            .0
            .unwrap();
        staging.edit_latest(|record| record["release"] = "v2".into());
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("has no staging record"), "{error}");
        assert!(!staging.fixture.repo().state.join("live").exists());
    }

    #[test]
    fn promote_accepts_a_stage_without_smoke_check_only_when_allowed() {
        let staging = Staging::new();
        let record = stage_with(
            staging.options("9:99", false),
            |_, _, _| Ok(7),
            Box::new(|_, _, _, _, _| panic!("no smoke script was given")),
        )
        .unwrap();
        assert_eq!(record["status"], "staged");
        assert!(record["smoke"].is_null());
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("ran no smoke check"), "{error}");
        staging.promote(true).unwrap();
    }

    #[test]
    fn stage_refuses_the_live_target_and_bad_input_before_uploading() {
        let staging = Staging::new();
        let never = || -> Box<SmokeRunner<'static>> {
            Box::new(|_, _, _, _, _| panic!("must not run a smoke check"))
        };
        // The release's own place, also under another universe ID.
        for target in ["123:456", "9:456"] {
            let error = stage_with(
                staging.options(target, true),
                |_, _, _| panic!("must not upload"),
                never(),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("must differ from release v1"),
                "{error}"
            );
        }
        for target in ["99", "9:x", "0:99"] {
            assert!(stage_with(
                staging.options(target, true),
                |_, _, _| panic!("must not upload"),
                never()
            )
            .is_err());
        }
        let mut options = staging.options("9:99", true);
        options.smoke_timeout = Some(301);
        assert!(stage_with(options, |_, _, _| panic!("must not upload"), never()).is_err());
        fs::remove_file(&staging.script).unwrap();
        let error = stage_with(
            staging.options("9:99", true),
            |_, _, _| panic!("must not upload"),
            never(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("smoke script"), "{error}");
        let repo = staging.fixture.repo();
        assert!(!repo.state.join("publication-attempts").exists());
        assert!(!repo.state.join("publication.lock").exists());
    }

    #[test]
    fn unconfirmed_staging_upload_keeps_the_lock_and_runs_no_smoke_check() {
        let staging = Staging::new();
        staging
            .stage(finished_task("COMPLETE", "[true]", ""))
            .0
            .unwrap();
        let error = stage_with(
            staging.options("9:99", true),
            |_, _, _| Err(UploadFailure::Unconfirmed("response lost".into())),
            Box::new(|_, _, _, _, _| panic!("must not run a smoke check")),
        )
        .unwrap_err();
        assert!(error.to_string().contains("not confirmed"), "{error}");
        let repo = staging.fixture.repo();
        assert!(repo.state.join("publication.lock").exists());
        assert_eq!(staging.staging_records().len(), 1);
        // The unsettled upload may have replaced the passing version, so
        // promote refuses before it even reaches the lock.
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("was staged again"), "{error}");

        // Resolving the staging attempt records no live pointer for the test place.
        let attempt = pending_or_unconfirmed_attempt(&repo);
        let resolved = resolve_attempt(&repo, &attempt, Resolution::Published(7), false).unwrap();
        assert_eq!(resolved["staging"], true);
        assert_eq!(resolved["lock"], "removed");
        assert!(!repo.state.join("live").exists());
        assert!(!repo.state.join("last-publication.json").exists());
        // The resolved upload replaced the passing stage's version on the
        // test place and never ran a smoke check, so the old pass is void.
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("was staged again"), "{error}");
        assert!(!repo.state.join("live").exists());
    }

    /// Moves every journal entry of this fixture back by `seconds`, so a
    /// later stage is strictly newer than these in whole seconds.
    fn age_attempts(repo: &Repository, seconds: u64) {
        for (path, mut receipt) in
            list_manifest_entries(&repo.state.join("publication-attempts"), false)
        {
            receipt["created_unix"] = (receipt["created_unix"].as_u64().unwrap() - seconds).into();
            fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        }
    }

    #[test]
    fn a_stage_cut_off_after_upload_voids_an_earlier_pass() {
        let staging = Staging::new();
        staging
            .stage(finished_task("COMPLETE", "[true]", ""))
            .0
            .unwrap();
        let repo = staging.fixture.repo();
        age_attempts(&repo, 20);
        staging.edit_latest(|record| record["staged_unix"] = (now() - 20).into());
        // A stage that uploaded and then was killed during its smoke check,
        // or is still running it, leaves this: a published staging receipt
        // and no staging record.
        publish_release(
            &staging.fixture.root,
            "v1",
            PublishKind::Staging {
                universe_id: 9,
                place_id: 99,
            },
            |_, _, _| Ok(8),
        )
        .unwrap();
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("was staged again"), "{error}");
        assert!(!repo.state.join("live").exists());

        // A rejected upload created no version, so it does not void a pass.
        age_attempts(&repo, 20);
        staging.edit_latest(|record| record["staged_unix"] = (now() - 30).into());
        staging
            .stage(finished_task("COMPLETE", "[true]", ""))
            .0
            .unwrap();
        age_attempts(&repo, 10);
        assert!(stage_with(
            staging.options("9:99", true),
            |_, _, _| Err(UploadFailure::Rejected("HTTP 400".into())),
            Box::new(|_, _, _, _, _| panic!("must not run a smoke check")),
        )
        .is_err());
        staging.promote(false).unwrap();
    }

    #[test]
    fn promote_refuses_records_without_a_journal_entry() {
        let staging = Staging::new();
        staging
            .stage(finished_task("COMPLETE", "[true]", ""))
            .0
            .unwrap();
        let repo = staging.fixture.repo();
        fs::remove_dir_all(repo.state.join("publication-attempts")).unwrap();
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("no published staging attempt"), "{error}");
    }

    #[test]
    fn promote_refuses_a_staging_record_dated_in_the_future() {
        let staging = Staging::new();
        staging
            .stage(finished_task("COMPLETE", "[true]", ""))
            .0
            .unwrap();
        staging.edit_latest(|record| record["staged_unix"] = (now() + 3600).into());
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("in the future"), "{error}");
        // A few minutes of clock difference is tolerated.
        staging.edit_latest(|record| record["staged_unix"] = (now() + 60).into());
        staging.promote(false).unwrap();
    }

    #[test]
    fn promote_refuses_a_stage_made_for_another_target() {
        let staging = Staging::new();
        staging
            .stage(finished_task("COMPLETE", "[true]", ""))
            .0
            .unwrap();
        staging.edit_latest(|record| record["release_target"] = "123:789".into());
        let error = staging.promote(false).unwrap_err().to_string();
        assert!(error.contains("targeted 123:789"), "{error}");
    }

    #[test]
    fn stage_refuses_places_that_are_live_or_another_releases_target() {
        let staging = Staging::new();
        let repo = staging.fixture.repo();
        let releases = repo.state.join("releases");
        let mut other: serde_json::Value = read_json(&releases.join("v1.json")).unwrap();
        other["name"] = "other".into();
        other["place_id"] = 99.into();
        fs::write(
            releases.join("other.json"),
            serde_json::to_vec(&other).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(repo.state.join("live")).unwrap();
        fs::write(
            live_record_path(&repo.state, 5, 77),
            serde_json::to_vec(&serde_json::json!({"universe_id": 5, "place_id": 77})).unwrap(),
        )
        .unwrap();
        for (target, expected) in [
            ("9:99", "target of release other"),
            ("9:77", "Place 77 is live"),
        ] {
            let error = stage_with(
                staging.options(target, true),
                |_, _, _| panic!("must not upload"),
                Box::new(|_, _, _, _, _| panic!("must not run a smoke check")),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(expected), "{error}");
        }
        assert!(!repo.state.join("publication-attempts").exists());
    }

    #[test]
    fn smoke_reasons_are_shortened() {
        let outcome = smoke_outcome(Ok(LuauTaskResult {
            path: TASK_PATH.into(),
            state: TaskState::Failed,
            results: Vec::new(),
            error_code: Some("SCRIPT_ERROR".into()),
            error_message: Some("x".repeat(100_000)),
            logs: Vec::new(),
            logs_truncated: false,
        }));
        assert!(outcome["reason"].as_str().unwrap().len() <= 1000);
    }

    fn pending_or_unconfirmed_attempt(repo: &Repository) -> String {
        list_manifests(&repo.state.join("publication-attempts"), false)
            .into_iter()
            .find(|receipt| receipt["status"] == "unconfirmed")
            .unwrap()["attempt"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn smoke_checks_pass_only_on_true_or_ok_true() {
        let task = |state, results: serde_json::Value| {
            Ok(LuauTaskResult {
                path: TASK_PATH.into(),
                state,
                results: results.as_array().unwrap().clone(),
                error_code: None,
                error_message: None,
                logs: Vec::new(),
                logs_truncated: false,
                logs_error: None,
            })
        };
        let status = |result| smoke_outcome(result)["status"].clone();
        use serde_json::json;
        assert_eq!(status(task(TaskState::Complete, json!([true]))), "passed");
        assert_eq!(
            status(task(
                TaskState::Complete,
                json!([{"ok": true, "message": "fine"}])
            )),
            "passed"
        );
        for failing in [
            json!([]),
            json!([null]),
            json!([false]),
            json!([1]),
            json!(["ok"]),
            json!([{}]),
            json!([{"ok": false}]),
            json!([{"ok": "true"}]),
            json!([[true]]),
            json!([false, true]),
        ] {
            assert_eq!(
                status(task(TaskState::Complete, failing.clone())),
                "failed",
                "{failing}"
            );
        }
        assert_eq!(status(task(TaskState::Cancelled, json!([true]))), "failed");
        let error = smoke_outcome(Err(OpenCloudError::TimedOut("still PROCESSING".into())));
        assert_eq!(error["status"], "error");
        assert_eq!(error["task_may_have_run"], true);
        // A poll refused after the task was created still means it may run.
        let error = smoke_outcome(Err(OpenCloudError::Unfollowed {
            path: TASK_PATH.into(),
            error: Box::new(OpenCloudError::NotSent("connection refused".into())),
        }));
        assert_eq!(error["task_may_have_run"], true);
        let logs = smoke_outcome(Ok(LuauTaskResult {
            path: TASK_PATH.into(),
            state: TaskState::Complete,
            results: vec![true.into()],
            error_code: None,
            error_message: None,
            logs: (0..60)
                .map(|i| crate::open_cloud::LogMessage {
                    message: format!("line {i}"),
                    kind: "OUTPUT".into(),
                })
                .collect(),
            logs_truncated: false,
            logs_error: None,
        }));
        assert_eq!(logs["logs"].as_array().unwrap().len(), SMOKE_LOG_TAIL);
        assert_eq!(logs["logs"][0]["message"], "line 10");
        assert_eq!(logs["logs_omitted"], true);
    }

    #[test]
    fn stage_and_promote_command_lines_parse() {
        let parse = |args: &[&str]| ReleaseCommand::try_parse_from(args.iter());
        assert!(parse(&["release", "stage", "v1", "--staging", "9:99"]).is_ok());
        assert!(parse(&[
            "release",
            "stage",
            "v1",
            "--staging",
            "9:99",
            "--smoke",
            "smoke.luau",
            "--smoke-timeout",
            "30"
        ])
        .is_ok());
        // A timeout without a script would silently do nothing.
        assert!(parse(&[
            "release",
            "stage",
            "v1",
            "--staging",
            "9:99",
            "--smoke-timeout",
            "30"
        ])
        .is_err());
        assert!(parse(&["release", "stage", "v1"]).is_err());
        assert!(parse(&["release", "promote", "v1"]).is_err());
        assert!(parse(&[
            "release",
            "promote",
            "v1",
            "--confirm-target",
            "123:456",
            "--max-age-hours",
            "2"
        ])
        .is_ok());
    }

    /// Runs one Luau Execution task against the latest version of the test
    /// place named by `ROJO_TEST_UNIVERSE_ID` and `ROJO_TEST_PLACE_ID`, with
    /// the key in `ROBLOX_API_KEY`, through the same evaluation as `release
    /// stage --smoke`. Publishes nothing.
    #[test]
    #[ignore = "needs a Roblox Open Cloud key and a test place"]
    fn live_smoke_check_against_the_test_place() {
        let id = |var: &str| -> u64 { std::env::var(var).unwrap().parse().unwrap() };
        let cloud = OpenCloud::from_env("ROBLOX_API_KEY").unwrap();
        let script = "print('rojo smoke check')\nreturn { ok = game:GetService('Workspace') ~= nil, message = 'workspace exists' }";
        let outcome = smoke_outcome(cloud.run_luau(
            id("ROJO_TEST_UNIVERSE_ID"),
            id("ROJO_TEST_PLACE_ID"),
            None,
            script,
            Duration::from_secs(60),
        ));
        println!("{}", serde_json::to_string_pretty(&outcome).unwrap());
        assert_eq!(outcome["status"], "passed");
    }
}
