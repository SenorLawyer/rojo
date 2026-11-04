# Type checking with luau-lsp

`rojo typecheck` checks a project's scripts with [luau-lsp](https://github.com/JohnnyMorganz/luau-lsp) and fails when luau-lsp reports a problem. Use it as a gate in CI or before committing.

```text
rojo typecheck [PROJECT] [--definitions PATH]... [--luau-lsp PATH] [-- LUAU_LSP_ARGS...]
```

The command:

1. Loads the project like `rojo build` does. `PROJECT` defaults to the current directory.
2. Writes a sourcemap of the whole project, including non-script instances, to a temporary file, so `require(script.Parent.Module)` and `workspace.SpawnLocation` resolve.
3. Runs `luau-lsp analyze --sourcemap=<temporary file>` on every `.lua` and `.luau` file behind a Script, LocalScript or ModuleScript in the project. Modules generated from `.json`, `.toml` or `.data.csv` files have no Luau file and are not checked.
4. Exits with status 1 when luau-lsp reports a problem, and 0 when it does not.

luau-lsp runs in the project's folder. On a very large project, the files are split across several luau-lsp runs to stay under the Windows command line limit.

## Installing luau-lsp

Add luau-lsp to the project's `rokit.toml`:

```text
rokit add JohnnyMorganz/luau-lsp
```

If luau-lsp is not on `PATH`, `rojo typecheck` says how to install it. If the Rokit shim is installed but the project's `rokit.toml` does not list luau-lsp, the command reports Rokit's error. Pass `--luau-lsp <path>` to run a specific executable.

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

This job checks a game repository whose `rokit.toml` lists this fork's Rojo and luau-lsp:

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
