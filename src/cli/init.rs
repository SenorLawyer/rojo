use std::process::{Command, Stdio};
use std::str::FromStr;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};
use std::{
    ffi::OsStr,
    io::{self, Write},
};

use anyhow::{bail, format_err, Context};
use clap::Parser;
use fs_err as fs;
use fs_err::OpenOptions;
use memofs::{InMemoryFs, Vfs, VfsSnapshot};

use super::resolve_path;

const GIT_IGNORE_PLACEHOLDER: &str = "gitignore.txt";

/// Each template's GitHub Actions workflow. The build script leaves out files
/// whose names start with `.git`, which includes `.github`, so the template
/// stores the workflow under this name and init moves it into place.
const CI_WORKFLOW_PLACEHOLDER: &str = "rojo-workflow.yml";
const CI_WORKFLOW_PATH: &str = ".github/workflows/rojo.yml";

/// How much of a subdirectory project's path goes into its workflow's name.
const MAX_WORKFLOW_NAME_CHARS: usize = 60;

static TEMPLATE_BINCODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/templates.bincode"));

/// Initializes a new Rojo project.
///
/// By default, this will attempt to initialize a 'git' repository in the
/// project directory if `git` is installed. To avoid this, pass `--skip-git`.
///
/// It also adds a GitHub Actions workflow at .github/workflows/rojo.yml that
/// runs `rojo packages doctor`, `rojo typecheck` and, for places once its
/// secrets are set, `rojo test --cloud`. Inside an existing Git repository it
/// goes at the repository root, as .github/workflows/rojo-<project path>-<hash>.yml,
/// and runs in the project's folder. To leave it out, pass `--no-ci`.
#[derive(Debug, Parser)]
pub struct InitCommand {
    /// Path to the place to create the project. Defaults to the current directory.
    #[clap(default_value = "")]
    pub path: PathBuf,

    /// The kind of project to create, 'place', 'plugin', or 'model'.
    #[clap(long, default_value = "place")]
    pub kind: InitKind,

    /// Skips the initialization of a git repository.
    #[clap(long)]
    pub skip_git: bool,

    /// Does not add the GitHub Actions workflow.
    #[clap(long)]
    pub no_ci: bool,
}

impl InitCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let template = self.kind.template()?;

        let base_path = resolve_path(&self.path)?;
        fs::create_dir_all(&base_path)?;

        let canonical = fs::canonicalize(&base_path)?;
        let project_name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("new-project");

        let project_params = ProjectParams {
            name: project_name.to_owned(),
        };

        println!(
            "Creating new {:?} project '{}'",
            self.kind, project_params.name
        );

        let vfs = Vfs::new(template);
        vfs.set_watch_enabled(false);

        let mut queue = VecDeque::with_capacity(8);
        for entry in vfs.read_dir("")? {
            queue.push_back(entry?.path().to_path_buf())
        }

        let mut workflow_template = None;
        while let Some(mut path) = queue.pop_front() {
            let metadata = vfs.metadata(&path)?;
            if metadata.is_dir() {
                fs_err::create_dir(base_path.join(&path))?;
                for entry in vfs.read_dir(&path)? {
                    queue.push_back(entry?.path().to_path_buf());
                }
            } else {
                let content = vfs.read_to_string_lf_normalized(&path)?;
                if let Some(file_stem) = path.file_name().and_then(OsStr::to_str) {
                    if file_stem == GIT_IGNORE_PLACEHOLDER {
                        if self.skip_git {
                            continue;
                        } else {
                            path.set_file_name(".gitignore");
                        }
                    } else if file_stem == CI_WORKFLOW_PLACEHOLDER {
                        if !self.no_ci {
                            workflow_template = Some(project_params.render_template(&content));
                        }
                        continue;
                    }
                }
                write_if_not_exists(
                    &base_path.join(&path),
                    &project_params.render_template(&content),
                )?;
            }
        }

        // Where the workflow goes is only worked out when it is wanted, so
        // that --no-ci never depends on Git or on the project's path.
        let workflow_note = match workflow_template {
            Some(workflow) => {
                let target = WorkflowTarget::for_project(&base_path)?;
                let file = target.file();
                fs::create_dir_all(file.parent().unwrap())?;
                let path = target.display_path();
                Some(if write_if_not_exists(&file, &target.adapt(&workflow))? {
                    format!(
                        "Added the GitHub Actions workflow {path}. The comments at its top list \
                         the secrets it needs, if any."
                    )
                } else {
                    format!("Left the existing GitHub Actions workflow {path} untouched.")
                })
            }
            None => None,
        };

        write_if_not_exists(&base_path.join("ROJO_AGENT.md"), super::doc::AGENT_GUIDE)?;
        write_if_not_exists(&base_path.join("AGENTS.md"), super::doc::AGENTS_POINTER)?;

        if !self.skip_git && should_git_init(&base_path) {
            log::debug!("Initializing Git repository...");

            let status = Command::new("git")
                .arg("init")
                .current_dir(&base_path)
                .status()?;

            if !status.success() {
                bail!("git init failed: status code {:?}", status.code());
            }
        }

        println!("Created project successfully.");
        if let Some(note) = workflow_note {
            println!("{note}");
        }

        Ok(())
    }
}

/// Where the GitHub Actions workflow goes. GitHub only reads workflows from
/// the repository root's .github/workflows, so a project created inside an
/// existing repository gets its workflow there, set to run in the project.
#[derive(Debug)]
struct WorkflowTarget {
    /// The repository root, or the project when it is the root or not in a
    /// repository yet.
    root: PathBuf,
    /// The project's path from the repository root, with `/` separators, or
    /// `None` when the project is the root.
    project_dir: Option<String>,
}

impl WorkflowTarget {
    fn for_project(project: &Path) -> anyhow::Result<Self> {
        let project = dunce::canonicalize(project)?;
        let Some(root) = git_toplevel(&project) else {
            return Ok(Self {
                root: project,
                project_dir: None,
            });
        };
        let relative = match project.strip_prefix(&root) {
            Ok(relative) if !relative.as_os_str().is_empty() => relative,
            // The project is the root, or Git named a root that does not
            // contain the project (such as through a symlink Rojo resolved
            // differently). Keep the workflow in the project as before.
            _ => {
                return Ok(Self {
                    root: project,
                    project_dir: None,
                })
            }
        };
        let components = relative
            .components()
            .map(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format_err!("{} is not valid Unicode", relative.display()))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            root,
            project_dir: Some(components.join("/")),
        })
    }

    /// The workflow's path from the repository root, with `/` separators.
    fn display_path(&self) -> String {
        match &self.project_dir {
            None => CI_WORKFLOW_PATH.to_owned(),
            // The whole path, not only the folder's name, so that projects
            // such as games/racer and tools/racer do not share a file. The
            // hash of the exact path keeps apart paths that sanitize to the
            // same name, such as games/racer and games-racer.
            Some(dir) => {
                let name: String = dir
                    .chars()
                    .map(|c| match c {
                        'a'..='z' | '0'..='9' | '-' | '_' => c,
                        'A'..='Z' => c.to_ascii_lowercase(),
                        _ => '-',
                    })
                    // File names are limited to 255 bytes. The hash still
                    // covers the whole path, so a cut name stays unique.
                    .take(MAX_WORKFLOW_NAME_CHARS)
                    .collect();
                let name = match name.trim_end_matches('-') {
                    "" => "project",
                    name => name,
                };
                let hash = blake3::hash(dir.as_bytes()).to_hex();
                format!(".github/workflows/rojo-{name}-{}.yml", &hash[..8])
            }
        }
    }

    fn file(&self) -> PathBuf {
        self.root.join(self.display_path())
    }

    /// Points a rendered workflow at the project when it is not the root:
    /// each job runs its commands in the project's folder, and the workflow
    /// is named after it so several projects' workflows can be told apart.
    fn adapt(&self, workflow: &str) -> String {
        let Some(dir) = &self.project_dir else {
            return workflow.to_owned();
        };
        let quoted = single_quoted(dir);
        let mut output = String::with_capacity(workflow.len() + 256);
        for line in workflow.lines() {
            if line == "name: Rojo" {
                output.push_str(&format!(
                    "name: {}\n",
                    single_quoted(&format!("Rojo ({dir})"))
                ));
                continue;
            }
            output.push_str(&line.replace(CI_WORKFLOW_PATH, &self.display_path()));
            output.push('\n');
            // Every job in the templates has one `runs-on` line at job level.
            if line == "    runs-on: ubuntu-latest" {
                output.push_str("    defaults:\n      run:\n        working-directory: ");
                output.push_str(&quoted);
                output.push('\n');
            }
        }
        output
    }
}

/// A YAML single-quoted string, in which only quotes need escaping.
fn single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The top level of the Git repository that contains `path`, or `None` when
/// it is not in one.
fn git_toplevel(path: &Path) -> Option<PathBuf> {
    let output = match Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .stderr(Stdio::null())
        .current_dir(path)
        .output()
    {
        Ok(output) => output,
        // Without Git, a checkout is still recognizable by its marker, and
        // GitHub still only reads workflows at its root.
        Err(_) => return find_git_marker(path),
    };
    // Git also fails inside a checkout it refuses, for example one owned by
    // another user, so the marker decides whether this really is outside a
    // repository.
    if !output.status.success() {
        return find_git_marker(path);
    }
    let toplevel = String::from_utf8(output.stdout).ok()?;
    dunce::canonicalize(toplevel.trim_end_matches(['\r', '\n'])).ok()
}

/// The nearest folder at or above `path` that has a `.git` entry: a
/// directory with a HEAD file in a normal checkout, or a file naming the Git
/// directory with `gitdir:` in worktrees and submodules. Other `.git` entries,
/// which Git itself would not accept, are skipped.
fn find_git_marker(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|dir| {
            let marker = dir.join(".git");
            if marker.is_dir() {
                marker.join("HEAD").is_file()
            } else {
                fs::read_to_string(&marker).is_ok_and(|contents| contents.starts_with("gitdir:"))
            }
        })
        .map(Path::to_path_buf)
}

/// The templates we support for initializing a Rojo project.
#[derive(Debug, Clone, Copy)]
pub enum InitKind {
    /// A place that contains a baseplate.
    Place,

    /// An empty model, suitable for a library.
    Model,

    /// An empty plugin.
    Plugin,
}

impl InitKind {
    fn template(&self) -> anyhow::Result<InMemoryFs> {
        let template_path = match self {
            Self::Place => "place",
            Self::Model => "model",
            Self::Plugin => "plugin",
        };

        let snapshot: VfsSnapshot = bincode::deserialize(TEMPLATE_BINCODE)
            .context("Rojo's templates were not properly packed into Rojo's binary. This is a bug in Rojo; please file an issue.")?;

        let VfsSnapshot::Dir { mut children } = snapshot else {
            bail!("Rojo's templates were packed as a file instead of a directory. This is a bug in Rojo; please file an issue.");
        };

        let template = children.remove(template_path).ok_or_else(|| {
            format_err!(
                "The template for project type {:?} is missing. This is a bug in Rojo; please file an issue.",
                self
            )
        })?;

        let mut fs = InMemoryFs::new();
        fs.load_snapshot("", template)
            .context("Failed to load Rojo's bundled template into memory")?;

        Ok(fs)
    }
}

impl FromStr for InitKind {
    type Err = anyhow::Error;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        match source {
            "place" => Ok(InitKind::Place),
            "model" => Ok(InitKind::Model),
            "plugin" => Ok(InitKind::Plugin),
            _ => Err(format_err!(
                "Invalid init kind '{}'. Valid kinds are: place, model, plugin",
                source
            )),
        }
    }
}

/// Contains parameters used in templates to create a project.
struct ProjectParams {
    name: String,
}

impl ProjectParams {
    /// Render a template by replacing variables with project parameters.
    fn render_template(&self, template: &str) -> String {
        template
            .replace("{project_name}", &self.name)
            .replace("{rojo_version}", env!("CARGO_PKG_VERSION"))
    }
}

/// Tells whether we should initialize a Git repository inside the given path.
///
/// Will return false if the user doesn't have Git installed or if the path is
/// already inside a Git repository.
fn should_git_init(path: &Path) -> bool {
    let result = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir(path)
        .status();

    match result {
        // If the command ran, but returned a non-zero exit code, we are not in
        // a Git repo and we should initialize one.
        Ok(status) => !status.success(),

        // If the command failed to run, we probably don't have Git installed.
        Err(_) => false,
    }
}

/// Write a file if it does not exist yet, otherwise, leave it alone. Returns
/// whether the file was written.
fn write_if_not_exists(path: &Path, contents: &str) -> Result<bool, anyhow::Error> {
    let file_res = OpenOptions::new().write(true).create_new(true).open(path);

    let mut file = match file_res {
        Ok(file) => file,
        Err(err) => {
            return match err.kind() {
                io::ErrorKind::AlreadyExists => return Ok(false),
                _ => Err(err.into()),
            }
        }
    };

    file.write_all(contents.as_bytes())?;

    Ok(true)
}

#[test]
fn init_ships_agent_guide_without_replacing_existing_instructions() {
    for kind in [InitKind::Place, InitKind::Model, InitKind::Plugin] {
        let dir = tempfile::tempdir().unwrap();
        InitCommand {
            path: dir.path().to_owned(),
            kind,
            skip_git: true,
            no_ci: false,
        }
        .run()
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("ROJO_AGENT.md")).unwrap(),
            super::doc::AGENT_GUIDE
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
            super::doc::AGENTS_POINTER
        );
        let preserved = dir.path().join("custom");
        fs::create_dir(&preserved).unwrap();
        fs::write(preserved.join("ROJO_AGENT.md"), "project instructions").unwrap();
        InitCommand {
            path: preserved.clone(),
            kind,
            skip_git: true,
            no_ci: false,
        }
        .run()
        .unwrap();
        assert_eq!(
            fs::read_to_string(preserved.join("ROJO_AGENT.md")).unwrap(),
            "project instructions"
        );
    }
}

#[cfg(test)]
fn init_for_test(path: &Path, kind: InitKind, no_ci: bool) {
    InitCommand {
        path: path.to_owned(),
        kind,
        skip_git: true,
        no_ci,
    }
    .run()
    .unwrap();
}

#[cfg(test)]
fn step_runs(job: &serde_yaml::Value) -> Vec<String> {
    job["steps"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(|step| step["run"].as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
fn step_named<'a>(job: &'a serde_yaml::Value, name: &str) -> &'a serde_yaml::Value {
    job["steps"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|step| step["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("no step named {name}"))
}

#[test]
fn init_creates_a_ci_workflow_that_runs_the_fork_checks() {
    for kind in [InitKind::Place, InitKind::Model, InitKind::Plugin] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("my-game");
        init_for_test(&project, kind, false);

        let workflow = fs::read_to_string(project.join(CI_WORKFLOW_PATH)).unwrap();
        assert!(!project.join(CI_WORKFLOW_PLACEHOLDER).exists(), "{kind:?}");

        // Rendered like the other template files.
        assert!(!workflow.contains("{project_name}"), "{kind:?}");
        assert!(!workflow.contains("{rojo_version}"), "{kind:?}");
        assert!(
            workflow.contains("Checks my-game on GitHub Actions"),
            "{kind:?}"
        );

        let yaml: serde_yaml::Value = serde_yaml::from_str(&workflow)
            .unwrap_or_else(|error| panic!("{kind:?} workflow is not YAML: {error}"));
        assert_eq!(
            yaml["env"]["ROJO_VERSION"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "{kind:?}"
        );

        let check_runs = step_runs(&yaml["jobs"]["check"]);
        assert!(
            check_runs.iter().any(|run| run == "rojo packages doctor"),
            "{kind:?}"
        );
        assert!(
            check_runs
                .iter()
                .any(|run| run.starts_with("rojo typecheck --definitions ")),
            "{kind:?}"
        );

        // Every action is pinned to a full commit SHA.
        for (_, job) in yaml["jobs"].as_mapping().unwrap() {
            for step in job["steps"].as_sequence().unwrap() {
                if let Some(uses) = step["uses"].as_str() {
                    let (_, version) = uses.split_once('@').unwrap();
                    assert!(
                        version.len() == 40 && version.chars().all(|c| c.is_ascii_hexdigit()),
                        "{kind:?} uses an unpinned action: {uses}"
                    );
                }
            }
        }

        let test_job = &yaml["jobs"]["test"];
        match kind {
            InitKind::Place => {
                // A fresh repository has no secrets, so the job must be
                // skipped rather than fail until they are set.
                assert_eq!(
                    test_job["if"].as_str(),
                    Some(
                        "needs.check.outputs.cloud-tests == 'true' && github.event_name != 'pull_request'"
                    )
                );
                assert_eq!(
                    yaml["jobs"]["check"]["outputs"]["cloud-tests"].as_str(),
                    Some("${{ steps.cloud.outputs.configured }}")
                );
                let gate = step_named(&yaml["jobs"]["check"], "Look for cloud test settings");
                assert_eq!(
                    gate["env"]["CONFIGURED"].as_str(),
                    Some(
                        "${{ secrets.ROBLOX_API_KEY != '' && vars.TEST_UNIVERSE_ID != '' && vars.TEST_PLACE_ID != '' }}"
                    )
                );
                // Per workflow, so several projects' workflows in one
                // repository do not replace each other's waiting runs.
                assert_eq!(
                    test_job["concurrency"]["group"].as_str(),
                    Some("rojo-cloud-tests-${{ github.workflow }}")
                );
                assert_eq!(
                    test_job["concurrency"]["cancel-in-progress"].as_bool(),
                    Some(false)
                );
                let run = step_named(test_job, "Run specs");
                assert_eq!(
                    run["run"].as_str(),
                    Some(
                        "rojo test --cloud --universe-id \"$TEST_UNIVERSE_ID\" --place-id \"$TEST_PLACE_ID\""
                    )
                );
                assert_eq!(
                    run["env"]["ROBLOX_API_KEY"].as_str(),
                    Some("${{ secrets.ROBLOX_API_KEY }}")
                );
                // The names the workflow reads are documented at its top.
                let header: String = workflow
                    .lines()
                    .take_while(|line| line.starts_with('#'))
                    .collect();
                for name in ["ROBLOX_API_KEY", "TEST_UNIVERSE_ID", "TEST_PLACE_ID"] {
                    assert!(header.contains(name), "{name}");
                }
            }
            // `rojo test --cloud` needs a DataModel root, which these lack.
            InitKind::Model | InitKind::Plugin => {
                assert!(test_job.is_null(), "{kind:?}");
                assert!(
                    !check_runs.iter().any(|run| run.contains("rojo test")),
                    "{kind:?}"
                );
                assert!(!workflow.contains("${{ secrets."), "{kind:?}");
            }
        }
    }
}

#[test]
fn init_no_ci_skips_the_workflow() {
    for kind in [InitKind::Place, InitKind::Model, InitKind::Plugin] {
        let dir = tempfile::tempdir().unwrap();
        init_for_test(dir.path(), kind, true);
        assert!(!dir.path().join(".github").exists(), "{kind:?}");
        assert!(
            !dir.path().join(CI_WORKFLOW_PLACEHOLDER).exists(),
            "{kind:?}"
        );
        assert!(
            dir.path().join("default.project.json").is_file(),
            "{kind:?}"
        );
    }
}

#[test]
fn init_keeps_an_existing_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let workflows = dir.path().join(".github").join("workflows");
    fs::create_dir_all(&workflows).unwrap();
    fs::write(workflows.join("rojo.yml"), "name: Mine\n").unwrap();
    fs::write(workflows.join("other.yml"), "name: Other\n").unwrap();

    init_for_test(dir.path(), InitKind::Place, false);

    assert_eq!(
        fs::read_to_string(workflows.join("rojo.yml")).unwrap(),
        "name: Mine\n"
    );
    assert_eq!(
        fs::read_to_string(workflows.join("other.yml")).unwrap(),
        "name: Other\n"
    );
}

#[cfg(test)]
fn git_init_for_test(path: &Path) {
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// The file name a project at `dir` (relative to the repository root) gets.
#[cfg(test)]
fn subdirectory_workflow_name(sanitized: &str, dir: &str) -> String {
    let hash = blake3::hash(dir.as_bytes()).to_hex();
    format!("rojo-{sanitized}-{}.yml", &hash[..8])
}

#[test]
fn init_inside_a_repository_puts_the_workflow_at_its_root() {
    for kind in [InitKind::Place, InitKind::Model, InitKind::Plugin] {
        let repo = tempfile::tempdir().unwrap();
        git_init_for_test(repo.path());
        let project = repo.path().join("games").join("Racer Game");
        init_for_test(&project, kind, false);

        // GitHub only reads workflows at the repository root.
        assert!(!project.join(".github").exists(), "{kind:?}");
        assert!(!repo.path().join(CI_WORKFLOW_PATH).exists(), "{kind:?}");
        let workflow = fs::read_to_string(repo.path().join(".github").join("workflows").join(
            subdirectory_workflow_name("games-racer-game", "games/Racer Game"),
        ))
        .unwrap();
        assert!(
            workflow.contains("Checks Racer Game on GitHub Actions"),
            "{kind:?}"
        );
        assert!(!workflow.contains(CI_WORKFLOW_PATH), "{kind:?}");

        let yaml: serde_yaml::Value = serde_yaml::from_str(&workflow)
            .unwrap_or_else(|error| panic!("{kind:?} workflow is not YAML: {error}"));
        assert_eq!(
            yaml["name"].as_str(),
            Some("Rojo (games/Racer Game)"),
            "{kind:?}"
        );
        let jobs = yaml["jobs"].as_mapping().unwrap();
        let expected_jobs = match kind {
            InitKind::Place => 2,
            InitKind::Model | InitKind::Plugin => 1,
        };
        assert_eq!(jobs.len(), expected_jobs, "{kind:?}");
        for (name, job) in jobs {
            assert_eq!(
                job["defaults"]["run"]["working-directory"].as_str(),
                Some("games/Racer Game"),
                "{kind:?} job {name:?}"
            );
        }
        assert!(
            step_runs(&yaml["jobs"]["check"])
                .iter()
                .any(|run| run == "rojo packages doctor"),
            "{kind:?}"
        );
    }
}

#[test]
fn init_inside_a_repository_keeps_an_existing_root_workflow() {
    let repo = tempfile::tempdir().unwrap();
    git_init_for_test(repo.path());
    let workflows = repo.path().join(".github").join("workflows");
    fs::create_dir_all(&workflows).unwrap();
    let existing = workflows.join(subdirectory_workflow_name("racer", "racer"));
    fs::write(&existing, "name: Mine\n").unwrap();

    init_for_test(&repo.path().join("racer"), InitKind::Place, false);

    assert_eq!(fs::read_to_string(&existing).unwrap(), "name: Mine\n");
    assert_eq!(fs::read_dir(&workflows).unwrap().count(), 1);
    assert!(!repo.path().join("racer").join(".github").exists());
}

#[test]
fn init_gives_paths_that_sanitize_alike_their_own_workflows() {
    let repo = tempfile::tempdir().unwrap();
    git_init_for_test(repo.path());
    init_for_test(
        &repo.path().join("games").join("racer"),
        InitKind::Place,
        false,
    );
    init_for_test(&repo.path().join("games-racer"), InitKind::Place, false);

    let workflows = repo.path().join(".github").join("workflows");
    let first = subdirectory_workflow_name("games-racer", "games/racer");
    let second = subdirectory_workflow_name("games-racer", "games-racer");
    assert_ne!(first, second);
    for (file, dir) in [(first, "games/racer"), (second, "games-racer")] {
        let workflow = fs::read_to_string(workflows.join(&file)).unwrap();
        let yaml: serde_yaml::Value = serde_yaml::from_str(&workflow).unwrap();
        assert_eq!(
            yaml["jobs"]["check"]["defaults"]["run"]["working-directory"].as_str(),
            Some(dir),
            "{file}"
        );
    }
    assert_eq!(fs::read_dir(&workflows).unwrap().count(), 2);
}

#[test]
fn init_no_ci_inside_a_repository_writes_no_workflow() {
    let repo = tempfile::tempdir().unwrap();
    git_init_for_test(repo.path());
    init_for_test(&repo.path().join("racer"), InitKind::Place, true);
    assert!(!repo.path().join(".github").exists());
    assert!(!repo.path().join("racer").join(".github").exists());
}

#[test]
fn init_at_a_repository_root_keeps_the_workflow_unchanged() {
    let repo = tempfile::tempdir().unwrap();
    git_init_for_test(repo.path());
    init_for_test(repo.path(), InitKind::Place, false);

    let outside = tempfile::tempdir().unwrap();
    init_for_test(outside.path(), InitKind::Place, false);

    let at_root = fs::read_to_string(repo.path().join(CI_WORKFLOW_PATH)).unwrap();
    let not_in_repo = fs::read_to_string(outside.path().join(CI_WORKFLOW_PATH)).unwrap();
    // Only the project name in the header comment differs.
    let body = |workflow: &str| workflow.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert_eq!(body(&at_root), body(&not_in_repo));
    assert!(!at_root.contains("working-directory"));
    assert!(at_root.contains("\nname: Rojo\n"));
}

#[test]
fn a_checkout_git_refuses_still_puts_the_workflow_at_its_root() {
    // A `.git` folder with only HEAD looks like a checkout but makes
    // `git rev-parse` fail, standing in for a checkout Git refuses to read.
    let tree = tempfile::tempdir().unwrap();
    let checkout = tree.path().join("checkout");
    let project = checkout.join("games").join("racer");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir(checkout.join(".git")).unwrap();
    fs::write(checkout.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();

    assert_eq!(
        git_toplevel(&project).map(|root| dunce::canonicalize(root).unwrap()),
        Some(dunce::canonicalize(&checkout).unwrap())
    );
}

#[test]
fn git_marker_is_found_as_a_directory_or_a_file() {
    let tree = tempfile::tempdir().unwrap();
    let checkout = tree.path().join("checkout");
    let worktree = checkout.join("worktrees").join("feature");
    let project = worktree.join("games").join("racer");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir(checkout.join(".git")).unwrap();
    fs::write(checkout.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    // Worktrees and submodules have a `.git` file pointing elsewhere.
    fs::write(worktree.join(".git"), "gitdir: elsewhere\n").unwrap();
    // Entries Git would not accept are passed over.
    let games = worktree.join("games");
    fs::create_dir(games.join(".git")).unwrap();
    fs::write(project.join(".git"), "not a checkout\n").unwrap();

    assert_eq!(find_git_marker(&project), Some(worktree.clone()));
    assert_eq!(find_git_marker(&worktree), Some(worktree.clone()));
    assert_eq!(
        find_git_marker(&checkout.join("worktrees")),
        Some(checkout.clone())
    );
    // A sibling folder is not taken to be in either checkout. Whether
    // something above the temporary folder has a `.git` depends on the
    // machine, so only the folders in this tree are checked.
    let outside = tree.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let found = find_git_marker(&outside);
    assert!(
        found
            .as_deref()
            .is_none_or(|dir| !dir.starts_with(tree.path())),
        "{found:?}"
    );
}

#[test]
fn long_project_paths_give_workflow_names_within_the_file_name_limit() {
    let long = "a".repeat(80);
    let target = |dir: String| WorkflowTarget {
        root: PathBuf::from("repo"),
        project_dir: Some(dir),
    };
    let first = target(format!("{long}/{long}/{long}")).display_path();
    let second = target(format!("{long}/{long}/{long}b")).display_path();

    for path in [&first, &second] {
        let name = path.strip_prefix(".github/workflows/").unwrap();
        assert!(name.len() <= 255, "{name}");
        assert_eq!(
            name.len(),
            "rojo-".len() + MAX_WORKFLOW_NAME_CHARS + "-12345678.yml".len()
        );
    }
    // The names are cut alike, but the hash of the whole path differs.
    assert_ne!(first, second);

    // A cut that ends on a separator does not leave a dangling '-'.
    let cut = target(format!("{}/x", "a".repeat(MAX_WORKFLOW_NAME_CHARS - 1))).display_path();
    let expected = format!("rojo-{}-", "a".repeat(MAX_WORKFLOW_NAME_CHARS - 1));
    assert!(cut.contains(&expected), "{cut}");
    assert!(!cut.contains("--"), "{cut}");
}
