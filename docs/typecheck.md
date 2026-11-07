# Type checking with luau-lsp

`rojo typecheck` checks a project's scripts with [luau-lsp](https://github.com/JohnnyMorganz/luau-lsp) and fails when luau-lsp reports a problem. Use it as a gate in CI or before committing.

```text
rojo typecheck [PROJECT] [--definitions PATH]... [--luau-lsp PATH] [-- LUAU_LSP_ARGS...]
```

The command:

1. Loads the project like `rojo build` does. `PROJECT` defaults to the current directory.
2. Writes a sourcemap of the whole project, including non-script instances, to a temporary folder, so `require(script.Parent.Module)` and `workspace.SpawnLocation` resolve.
3. Writes the Luau that Rojo generates for each [data module](data-modules.md) into the same temporary folder, and points the sourcemap at it, because luau-lsp cannot read `.data.csv` files. Scripts that require a data module, or use its `Row` type, are checked against the generated code. These copies start with `--!nocheck` instead of `--!strict`, so luau-lsp does not check the generated code itself, which it gives up on for data modules with tens of thousands of rows. The `Row` type and the type of the returned table still reach the scripts that require the module.
4. Runs `luau-lsp analyze --sourcemap=<temporary file>` on every `.lua` and `.luau` file behind a Script, LocalScript or ModuleScript in the project. The generated data module code is not on that list and is not checked, so luau-lsp does not report problems in it. Modules generated from `.json`, `.toml` and `.yaml` files are left to luau-lsp, which reads those files itself.
5. Exits with a status that says how the check went:

   | Status | Meaning |
   | --- | --- |
   | 0 | luau-lsp found no problems. |
   | 1 | luau-lsp reported a problem, or the project could not be loaded. |
   | 2 | The check could not run: luau-lsp was not found, could not be run or `luau-lsp --version` failed, or the project has no `.lua` or `.luau` scripts. |

The temporary folder is removed when the command finishes. If the command is stopped with Ctrl+C, the folder (named `rojo-typecheck-*`, in the system's temporary directory) can be left behind.

luau-lsp runs in the project's folder. On a very large project, the files are split across several luau-lsp runs to stay under the Windows command line limit.

## Installing luau-lsp

Add luau-lsp to the project's `rokit.toml`:

```text
rokit add JohnnyMorganz/luau-lsp
```

If luau-lsp is not on `PATH`, `rojo typecheck` says how to install it. If the Rokit shim is installed but the project's `rokit.toml` does not list luau-lsp, the command reports Rokit's error. Pass `--luau-lsp <path>` to run a specific executable. A relative path, such as `./tools/luau-lsp`, is relative to the current directory, even though luau-lsp runs in the project's folder. A bare name, such as `luau-lsp`, is looked up on `PATH`.

## Roblox definitions

`luau-lsp analyze` does not download Roblox's type definitions. Without them, it warns that no definitions file was provided, and Roblox globals like `game`, `Instance` and `Vector3` are unknown. Download `globalTypes.d.luau` from the luau-lsp repository and pass it with `--definitions`:

```text
curl -fsSL -o globalTypes.d.luau https://raw.githubusercontent.com/JohnnyMorganz/luau-lsp/main/scripts/globalTypes.d.luau
rojo typecheck --definitions globalTypes.d.luau
```

`--definitions` can be repeated and accepts luau-lsp's `@name=path` form. Its paths are relative to the current directory.

## Passing options to luau-lsp

Everything after `--` is passed to `luau-lsp analyze` before the file list. These paths are relative to the project's folder. Common options:

| Option | Use |
| --- | --- |
| `--ignore=**/Packages/**` | Hide problems in files matching the glob, such as Wally packages. The files are still analyzed. |
| `--settings=.vscode/settings.json` | Read `luau-lsp.*` settings, such as require aliases, from an editor settings file. |
| `--base-luaurc=.luaurc` | Use this `.luaurc` as the default configuration. |
| `--formatter=gnu` | Print problems in the GNU `file:line.column` style instead of luau-lsp's default `file(line,column)`. |
| `--platform=standard` | Check code that does not run in Roblox, such as Lune scripts. |

luau-lsp finds `.luaurc` files next to the checked files on its own, so `--!strict` defaults and aliases set there apply without extra options.

```text
rojo typecheck default.project.json --definitions globalTypes.d.luau -- "--ignore=**/Packages/**" --formatter=gnu
```

## GitHub Actions

`rojo init` creates `.github/workflows/rojo.yml` for new projects (pass `--no-ci` to leave it out), or `.github/workflows/rojo-<project path>-<hash>.yml` at the root of the Git repository the project is created in, set to run in the project's folder. It downloads pinned releases of this fork's Rojo and of luau-lsp, and Roblox's definitions from the same luau-lsp release (the PluginSecurity ones for plugins), then runs `rojo packages doctor` and `rojo typecheck`. It does not use Rokit or the project's `rokit.toml`.

In a repository that uses Rokit, this job checks a game repository whose `rokit.toml` lists this fork's Rojo and luau-lsp:

```toml
[tools]
rojo = "SenorLawyer/rojo@<version>"
luau-lsp = "JohnnyMorganz/luau-lsp@<version>"
```

```yaml
jobs:
  typecheck:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: CompeyDev/setup-rokit@v0.1.2

      - name: Download Roblox type definitions
        run: curl -fsSL -o globalTypes.d.luau https://raw.githubusercontent.com/JohnnyMorganz/luau-lsp/main/scripts/globalTypes.d.luau

      - name: Type check
        run: rojo typecheck default.project.json --definitions globalTypes.d.luau -- "--ignore=**/Packages/**" --formatter=gnu
```

If the project uses Wally packages, install them before the type check step so their modules are in the sourcemap.
