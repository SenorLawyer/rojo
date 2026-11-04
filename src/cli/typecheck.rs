use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    io::{self, BufWriter, Write},
    mem::forget,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context};
use clap::Parser;
use memofs::Vfs;
use rbx_dom_weak::{types::Ref, types::Variant, ustr};

use crate::{serve_session::ServeSession, snapshot_middleware::Middleware};

use super::{resolve_path, sourcemap::write_sourcemap_json};

/// The install command suggested when luau-lsp cannot be found.
const INSTALL_HINT: &str = "rokit add JohnnyMorganz/luau-lsp";

/// Windows limits a whole command line to 32767 characters. Staying well
/// under it leaves room for the executable path and the other arguments.
const MAX_FILE_ARGS_LEN: usize = 24_000;

/// Type checks the project's scripts with luau-lsp.
///
/// Rojo generates a sourcemap of the project into a temporary file and runs
/// `luau-lsp analyze` on every Luau script in the project. The command fails
/// when luau-lsp reports a problem.
#[derive(Debug, Parser)]
pub struct TypecheckCommand {
    /// Path to the project to check. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// A Luau definitions file to load, such as Roblox's `globalTypes.d.luau`.
    /// Passed to luau-lsp as `--definitions`. Can be given more than once, and
    /// accepts luau-lsp's `@name=path` form.
    #[clap(long = "definitions", alias = "defs", value_name = "PATH")]
    pub definitions: Vec<String>,

    /// The luau-lsp executable to run.
    #[clap(long, default_value = "luau-lsp", value_name = "PATH")]
    pub luau_lsp: PathBuf,

    /// Extra arguments for `luau-lsp analyze`, given after `--`. luau-lsp runs
    /// in the project's folder, so relative paths are relative to it.
    #[clap(last = true, value_name = "LUAU_LSP_ARGS")]
    pub extra_args: Vec<OsString>,
}

impl TypecheckCommand {
    pub fn run(self) -> anyhow::Result<()> {
        // Check for luau-lsp first, so a missing install does not wait for
        // the whole project to load.
        let version = luau_lsp_version(&self.luau_lsp)?;
        log::info!("Using luau-lsp {version}");

        let project_path = dunce::canonicalize(resolve_path(&self.project)?)?;
        let session = ServeSession::new(Vfs::new_default_without_watch(), project_path)?;
        let project_dir = session.root_dir().to_path_buf();

        let files = script_files(&session);
        if files.is_empty() {
            bail!(
                "The project at {} has no .lua or .luau scripts to check",
                project_dir.display()
            );
        }

        // Holds the sourcemap and the Luau of generated modules. It is removed
        // when it is dropped at the end of the run.
        let temp_dir = tempfile::Builder::new()
            .prefix("rojo-typecheck-")
            .tempdir()
            .context("Could not create a temporary folder for the sourcemap")?;

        let generated = write_generated_modules(&session, temp_dir.path())?;

        let sourcemap_path = temp_dir.path().join("sourcemap.json");
        {
            let file = fs_err::File::create(&sourcemap_path)?;
            let mut writer = BufWriter::new(file);
            // Non-scripts are included so luau-lsp knows about every instance
            // a script can index, such as `workspace.SpawnLocation`.
            write_sourcemap_json(&session, &mut writer, true, &generated)?;
            writer.flush()?;
        }

        let definitions = self
            .definitions
            .iter()
            .map(|definition| absolute_definition(definition))
            .collect::<anyhow::Result<Vec<_>>>()?;

        let invocations = analyze_invocations(
            &sourcemap_path,
            &definitions,
            &self.extra_args,
            &files,
            MAX_FILE_ARGS_LEN,
        );

        let mut failed = false;
        for args in invocations {
            log::debug!("Running {} {:?}", self.luau_lsp.display(), args);
            let status = Command::new(&self.luau_lsp)
                .args(&args)
                .current_dir(&project_dir)
                .status()
                .with_context(|| format!("Could not run {}", self.luau_lsp.display()))?;
            failed |= !status.success();
        }

        // Dropping a ServeSession can be slow and we are about to exit.
        forget(session);

        if failed {
            bail!(
                "luau-lsp reported problems while checking {} script(s)",
                files.len()
            );
        }

        println!("luau-lsp found no problems in {} script(s)", files.len());
        Ok(())
    }
}

/// Runs `luau-lsp --version`, turning the ways it can be missing into errors
/// that say how to install it.
fn luau_lsp_version(luau_lsp: &Path) -> anyhow::Result<String> {
    let output = match Command::new(luau_lsp).arg("--version").output() {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!(
                "Could not find luau-lsp ({}). Install it with `{INSTALL_HINT}`, \
                 or pass its location with --luau-lsp.",
                luau_lsp.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Could not run {}", luau_lsp.display()))
        }
    };

    // A Rokit shim exists even when the current project does not list the
    // tool, in which case it fails with an explanation on stderr.
    if !output.status.success() {
        bail!(
            "{} --version failed: {}\nInstall luau-lsp with `{INSTALL_HINT}`.",
            luau_lsp.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Resolves the path in a `--definitions` value against the current
/// directory, since luau-lsp runs in the project's folder instead.
fn absolute_definition(definition: &str) -> anyhow::Result<String> {
    let (prefix, path) = match definition.strip_prefix('@').and_then(|d| d.split_once('=')) {
        Some((name, path)) => (format!("@{name}="), path),
        None => (String::new(), definition),
    };

    let path = resolve_path(Path::new(path))?;
    Ok(format!("{prefix}{}", path.display()))
}

/// Writes the Luau source of each ModuleScript generated from a data CSV
/// file into `dir`, and returns where each instance's file went.
///
/// luau-lsp cannot read CSV, so without these files a script that requires
/// a data module gets `Unknown require`, and types like `Items.Row` are
/// unknown. JSON, TOML and YAML modules are left alone because luau-lsp
/// reads those files itself. The files are only given to luau-lsp through the
/// sourcemap and are not on the list of files to check: their code is
/// generated by Rojo, so a problem in it is not something the user can fix
/// in that file.
fn write_generated_modules(
    session: &ServeSession,
    dir: &Path,
) -> anyhow::Result<HashMap<Ref, PathBuf>> {
    let tree = session.tree();
    let mut files = HashMap::new();

    for instance in tree.descendants(tree.get_root_id()) {
        if instance.class_name() != "ModuleScript"
            || !matches!(
                instance.metadata().middleware,
                Some(Middleware::DataCsv | Middleware::DataCsvDir)
            )
        {
            continue;
        }

        let Some(Variant::String(source)) = instance.properties().get(&ustr("Source")) else {
            continue;
        };

        // Numbered so modules with the same name in different places do not
        // overwrite each other.
        let path = dir.join(format!(
            "{}-{}.luau",
            files.len(),
            file_safe(instance.name())
        ));
        fs_err::write(&path, source)?;
        files.insert(instance.id(), path);
    }

    Ok(files)
}

/// Keeps the characters of an instance name that are safe in a file name on
/// every platform, so the file is still recognisable when debugging.
fn file_safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Lists the Luau files behind the project's scripts, relative to the project
/// folder and in a stable order.
fn script_files(session: &ServeSession) -> Vec<PathBuf> {
    let tree = session.tree();
    let project_dir = session.root_dir();
    let mut files = BTreeSet::new();

    for instance in tree.descendants(tree.get_root_id()) {
        if !matches!(
            instance.class_name().as_str(),
            "Script" | "LocalScript" | "ModuleScript"
        ) {
            continue;
        }

        // Scripts generated from other files, like JSON or data CSVs, have no
        // Luau file of the user's to check. See `write_generated_modules`.
        for path in &instance.metadata().relevant_paths {
            let is_luau = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| matches!(extension, "lua" | "luau"));
            if is_luau && path.is_file() {
                let path = pathdiff::diff_paths(path, project_dir).unwrap_or_else(|| path.clone());
                files.insert(path);
            }
        }
    }

    files.into_iter().collect()
}

/// Builds the arguments for each `luau-lsp analyze` run. Files are split
/// across several runs when they would not fit on one command line.
fn analyze_invocations(
    sourcemap: &Path,
    definitions: &[String],
    extra_args: &[OsString],
    files: &[PathBuf],
    max_file_args_len: usize,
) -> Vec<Vec<OsString>> {
    let mut base: Vec<OsString> = vec!["analyze".into()];

    let mut sourcemap_arg = OsString::from("--sourcemap=");
    sourcemap_arg.push(sourcemap);
    base.push(sourcemap_arg);

    for definition in definitions {
        base.push(format!("--definitions={definition}").into());
    }

    base.extend(extra_args.iter().cloned());

    let mut invocations = Vec::new();
    let mut current = base.clone();
    let mut current_len = 0;

    for file in files {
        // One extra for the separating space.
        let len = file.as_os_str().len() + 1;
        if current_len > 0 && current_len + len > max_file_args_len {
            invocations.push(std::mem::replace(&mut current, base.clone()));
            current_len = 0;
        }
        current.push(file.into());
        current_len += len;
    }

    invocations.push(current);
    invocations
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn builds_one_invocation_with_every_argument() {
        let invocations = analyze_invocations(
            Path::new("/tmp/map.json"),
            &[
                "/defs/globalTypes.d.luau".into(),
                "@lune=/defs/lune.d.luau".into(),
            ],
            &["--ignore=**/Packages/**".into(), "--formatter=gnu".into()],
            &[PathBuf::from("src/a.luau"), PathBuf::from("src/b.lua")],
            MAX_FILE_ARGS_LEN,
        );

        assert_eq!(
            invocations,
            vec![vec![
                OsString::from("analyze"),
                "--sourcemap=/tmp/map.json".into(),
                "--definitions=/defs/globalTypes.d.luau".into(),
                "--definitions=@lune=/defs/lune.d.luau".into(),
                "--ignore=**/Packages/**".into(),
                "--formatter=gnu".into(),
                "src/a.luau".into(),
                "src/b.lua".into(),
            ]]
        );
    }

    #[test]
    fn splits_files_that_do_not_fit_on_one_command_line() {
        let files: Vec<PathBuf> = ["aaaa.luau", "bbbb.luau", "cccc.luau"]
            .into_iter()
            .map(PathBuf::from)
            .collect();

        // Each file takes 10 bytes with its separator, so two fit in 20.
        let invocations = analyze_invocations(Path::new("map.json"), &[], &[], &files, 20);

        assert_eq!(
            invocations,
            vec![
                vec![
                    OsString::from("analyze"),
                    "--sourcemap=map.json".into(),
                    "aaaa.luau".into(),
                    "bbbb.luau".into(),
                ],
                vec![
                    OsString::from("analyze"),
                    "--sourcemap=map.json".into(),
                    "cccc.luau".into(),
                ],
            ]
        );
    }

    #[test]
    fn a_file_longer_than_the_limit_still_runs() {
        let invocations = analyze_invocations(
            Path::new("map.json"),
            &[],
            &[],
            &[PathBuf::from("a-very-long-file-name.luau")],
            5,
        );
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].last().unwrap(), "a-very-long-file-name.luau");
    }

    #[test]
    fn definitions_are_resolved_against_the_current_directory() {
        let current_dir = std::env::current_dir().unwrap();

        assert_eq!(
            absolute_definition("types/globalTypes.d.luau").unwrap(),
            current_dir
                .join("types/globalTypes.d.luau")
                .display()
                .to_string()
        );
        assert_eq!(
            absolute_definition("@roblox=types/globalTypes.d.luau").unwrap(),
            format!(
                "@roblox={}",
                current_dir.join("types/globalTypes.d.luau").display()
            )
        );
    }

    #[test]
    fn missing_luau_lsp_suggests_installing_it() {
        let error = luau_lsp_version(Path::new("rojo-test-luau-lsp-that-does-not-exist"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("Could not find luau-lsp"), "{error}");
        assert!(
            error.contains("rokit add JohnnyMorganz/luau-lsp"),
            "{error}"
        );
    }

    #[test]
    fn lists_only_luau_script_files() {
        let project = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("rojo-test")
            .join("build-tests")
            .join("data_csv");
        let session = ServeSession::new(
            Vfs::new_default_without_watch(),
            dunce::canonicalize(project).unwrap(),
        )
        .unwrap();

        // The data CSV modules are scripts too, but have no Luau file.
        assert_eq!(
            script_files(&session),
            vec![Path::new("src").join("Levels").join("Helper.luau")]
        );
    }

    fn data_modules_session() -> ServeSession {
        let project = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-projects")
            .join("typecheck_data_modules");
        ServeSession::new(
            Vfs::new_default_without_watch(),
            dunce::canonicalize(project).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn sourcemap_points_data_modules_at_generated_luau() {
        let session = data_modules_session();
        let dir = tempfile::tempdir().unwrap();

        let generated = write_generated_modules(&session, dir.path()).unwrap();

        let tree = session.tree();
        let mut names: Vec<_> = generated
            .iter()
            .map(|(id, path)| {
                let instance = tree.get_instance(*id).unwrap();
                let Some(Variant::String(source)) = instance.properties().get(&ustr("Source"))
                else {
                    panic!("{} has no Source", instance.name());
                };
                assert_eq!(&fs_err::read_to_string(path).unwrap(), source);
                instance.name().to_owned()
            })
            .collect();
        names.sort();
        // Settings.json is left out, since luau-lsp reads JSON itself.
        assert_eq!(names, ["Items", "Levels"]);
        drop(tree);

        let mut json = Vec::new();
        write_sourcemap_json(&session, &mut json, true, &generated).unwrap();
        let sourcemap: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let child = |name: &str| {
            sourcemap["children"]
                .as_array()
                .unwrap()
                .iter()
                .find(|child| child["name"] == name)
                .unwrap()
                .clone()
        };

        let items_path = child("Items")["filePaths"][0].as_str().unwrap().to_owned();
        assert!(
            items_path.ends_with("Items.luau") && Path::new(&items_path).starts_with(dir.path()),
            "{items_path}"
        );

        // The init.data.csv module keeps its children.
        let levels = child("Levels");
        assert_eq!(levels["filePaths"].as_array().unwrap().len(), 1);
        assert_eq!(levels["children"][0]["name"], "Helper");

        assert_eq!(child("Settings")["filePaths"][0], "src/Settings.json");
    }

    /// Runs the real luau-lsp when `ROJO_TEST_LUAU_LSP` names it, since CI
    /// does not install it.
    #[test]
    fn scripts_can_require_data_modules() {
        let Some(luau_lsp) = std::env::var_os("ROJO_TEST_LUAU_LSP") else {
            eprintln!("Skipping: set ROJO_TEST_LUAU_LSP to a luau-lsp executable to run");
            return;
        };

        TypecheckCommand {
            project: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-projects")
                .join("typecheck_data_modules"),
            definitions: Vec::new(),
            luau_lsp: luau_lsp.into(),
            extra_args: Vec::new(),
        }
        .run()
        .unwrap();
    }
}
