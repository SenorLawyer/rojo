use std::{
    collections::BTreeSet,
    ffi::OsString,
    io::{self, BufWriter, Write},
    mem::forget,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context};
use clap::Parser;
use memofs::Vfs;

use crate::serve_session::ServeSession;

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

        let sourcemap = tempfile::Builder::new()
            .prefix("rojo-typecheck-")
            .suffix(".json")
            .tempfile()
            .context("Could not create a temporary file for the sourcemap")?;
        {
            let mut writer = BufWriter::new(sourcemap.as_file());
            // Non-scripts are included so luau-lsp knows about every instance
            // a script can index, such as `workspace.SpawnLocation`.
            write_sourcemap_json(&session, &mut writer, true)?;
            writer.flush()?;
        }

        let definitions = self
            .definitions
            .iter()
            .map(|definition| absolute_definition(definition))
            .collect::<anyhow::Result<Vec<_>>>()?;

        let invocations = analyze_invocations(
            sourcemap.path(),
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
        // Luau file to check.
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
}
