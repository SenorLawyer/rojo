# Wally and pesde packages

Rojo builds the files produced by the package manager. The manager owns dependency resolution, downloads, generated link scripts and lockfiles. `rojo packages install` delegates to a native manager executable; it does not translate manifests or replace the manager's resolver.

## Check a project

```text
rojo packages doctor .
rojo packages doctor default.project.json --json
```

The JSON report has `schemaVersion: 1`, `directory`, `projectFile`, `managers`, `findings` and `ok`. It reports manifest parsing, dependency counts, lockfile presence, package directory presence and declared Rojo `$path` mappings. It checks both managers independently when both manifests are present. A reported error produces a nonzero exit status; warnings alone do not. A project path that does not exist, or a file that is not a `.project.json`/`.project.jsonc` project, fails with an ordinary error message before any report is produced, even with `--json`.

Wally uses `Packages` and `ServerPackages`. pesde uses `roblox_packages` and `roblox_server_packages`. For pesde's `lune` and `luau` targets, doctor checks that `lune_packages` or `luau_packages` is installed but does not expect a Rojo mapping for it. Map shared packages under ReplicatedStorage and server-only packages under ServerScriptService or ServerStorage. Doctor flags an existing server package directory included by a mapping outside those server services. Only children of a `DataModel` project root count as services; in a model project, a child named `ServerStorage` is an ordinary instance. It also catches directories covered by a parent `$path`, not only exact directory matches. When the project has `globIgnorePaths` and only such a parent `$path` covers a server package directory, doctor reports the `server-packages-possibly-exposed` warning instead of an error, because it does not evaluate those globs.

These are static checks of the selected project's declared mappings. Doctor does not execute either manager or dependency code, validate the manager's full manifest schema, prove a lockfile is current, expand nested project files, or apply ignore/sync rules. Its `ok` field means that the reported checks found no errors, not that every dependency can build. Use an actual Rojo build to verify the installed result.

## Install with a native manager

```text
rojo packages install . --manager wally
rojo packages install . --manager pesde
rojo packages install . --manager pesde --locked
rojo packages install . --manager pesde -- --prod
```

Choose `--manager` explicitly. The wrapper runs `wally install` or `pesde install` in the selected directory, inherits its input and output, and returns an error if the manager fails. Additional arguments after `--` are passed as separate native arguments. Manager hooks retain their normal behavior. No command shell is used, no global tool is installed, and Rojo does not edit the manifest itself.

The executable normally resolves through PATH, so a project toolchain shim can select its configured version. To use an existing native binary directly, pass `--executable C:\Tools\pesde.exe`. Explicit relative binary paths are resolved before switching to the project directory. Windows batch scripts are not accepted as native executables.

pesde's native [`install --locked`](https://docs.pesde.dev/reference/cli/#pesde-install) rejects an outdated lockfile. Wally 0.3.2 has no equivalent flag. Rojo rejects `--locked` for Wally rather than claim a frozen install. Commit `wally.lock` and verify it remains unchanged during CI installation. A second install preserving a lockfile is useful evidence for that fixture; it is not a guarantee for every possible Wally manifest.

## pesde Roblox setup

Follow pesde's [Roblox guide](https://docs.pesde.dev/guides/roblox/): initialize a Roblox project and select `pesde/scripts_rojo`. Its scripts generate the per-package Rojo configurations. A typical initializer uses the dependency alias `scripts` and writes:

```toml
[scripts]
roblox_sync_config_generator = ".pesde/scripts/roblox_sync_config_generator.luau"
sourcemap_generator = ".pesde/scripts/sourcemap_generator.luau"

[dev_dependencies]
scripts = { name = "pesde/scripts_rojo", version = "^0.2.0", target = "lune" }
```

The alias is part of the generated path. If you use a different alias, update the paths to match. Doctor requires the generator setting for Roblox targets and warns when the configured file does not exist. A fresh installation can legitimately have this warning before the scripts package is installed.

The generator needs Lune. Use the engine versions required by the scripts package. Native pesde Roblox packages describe their exported files through `target.build_files`; the generator builds Rojo configuration for those files. Do not replace the generated dependency configuration with a Wally-style root `$path: "src"`. See the official [manifest reference](https://docs.pesde.dev/reference/manifest/) for `build_files`, scripts and dependency targets.

## Reproduce package compatibility checks

```text
python scripts/package-smoke.py --rojo target/debug/rojo.exe --wally C:/Tools/wally.exe --pesde C:/Tools/pesde.exe --lune C:/Tools/lune.exe
```

The script requires network access and existing native binaries. It creates disposable projects and an isolated pesde data directory. It installs Promise 4.0.0 through Wally, then installs that Wally dependency and a local native Roblox dependency through pesde with `pesde/scripts_rojo` 0.2.0. It verifies repeat-install lockfile hashes, diagnostics, pesde's generated configuration and actual Rojo XML builds. The generated place must contain the native fixture's source.

The compatibility check passed on Windows x64 with Rojo 8.0.0-dev.1, built from PR #2 branch commit `ca36245`, and Wally 0.3.2, pesde 0.7.4 and Lune 0.10.4. It was repeated for 8.0.0-dev.4 (branch commit `039f6cf`), as recorded in the [verification notes](verification-8.0.md). These SHAs are historical references that will not exist on `master` after PR #2 is squash merged. The check has not been re-run for later commits, and CI does not run it. A changed pesde dependency was also rejected by `--locked` without changing the lockfile.

On November 4, 2026, with the 8.0.0-dev.1 build (branch commit `ca36245`), both package models were imported through the native Studio bridge into a disposable Edit fixture in Studio `0.745.0.7451137`. Requiring the Wally consumer resolved Promise to `42`. The pesde consumer resolved Promise to `42` and required the native package's expected value, including through pesde's generated relative require path. Other open places were untouched. This verifies these fixtures; it does not establish every registry package's behavior.
