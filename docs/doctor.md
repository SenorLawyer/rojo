# rojo doctor

`rojo doctor` runs the checks that otherwise take several commands and a
failed Studio connection to piece together, and prints one report. It reads
files and asks a running `rojo serve` for its info; it never writes, installs
or starts anything.

```text
rojo doctor
rojo doctor path/to/default.project.json --json
rojo doctor . --port 34873 --plugins-dir "C:\Users\me\AppData\Local\Roblox\Plugins"
```

The project argument resolves like `rojo serve`'s: a project file, or a folder
with `default.project.json`, defaulting to the current directory.

## Checks

Each check is `ok`, `info`, `warning` or `error`, and anything that needs
attention has a one-line fix.

* Project: parses the project file and builds its first snapshot exactly as
  `rojo serve` does, so a missing `$path` or invalid field fails here the same
  way, and reports the root's class and name. A file that does not parse is
  `project-invalid`; one that parses but whose instances cannot be built is
  `snapshot-failed`, and its settings (`servePort`, `servePlaceIds`, the
  name) are still used by the other checks.
* servePlaceIds: absent is a warning, because any place open in Studio can
  connect and be synced. An empty list is an error, because the plugin then
  refuses every place. Listed IDs are shown, and 0 means unpublished places are
  allowed. An ID that is also in `blockedPlaceIds` is a warning, since it
  cannot sync. Values that are not place IDs stop the project from loading,
  as they stop `rojo serve`.
* Packages: the findings of `rojo packages doctor`, with the same codes. A
  project with no `wally.toml` or `pesde.toml` is only information here.
* Studio plugin: whether `RojoManagedPlugin.rbxm` is in Studio's plugins
  folder (or `--plugins-dir`), its version read from the installed model
  against this CLI's, and any side-by-side `Rojo-<version>.rbxm` copies, which
  Studio loads as separate plugins. A plugin that is not installed is a
  warning, since it may be installed another way. The installed file is read
  as an untrusted model, with the checks `rojo studio import` uses: at most
  5 MiB, structural checks, and decoding in a child process with a memory cap,
  so a corrupted file is reported as `plugin-unreadable`. With
  `--plugins-dir`, the suggested install and uninstall commands name that
  folder too.
* rojo serve: whether a server answers on the address and port serve would use
  (`--address` and `--port`, then the project's `serveAddress` and
  `servePort`, then `127.0.0.1:34872`). Its version against this CLI's, its
  protocol against the plugin's, and whether it serves a project with this
  project's name. It reads both this fork's MessagePack answer and the JSON
  that released upstream Rojo sends, so an upstream or older server is
  reported as a version or protocol mismatch. No server is information, not a
  problem. Doctor waits at most 1.5 seconds and reads at most 1 MiB of the
  answer.

  The API names the served project but not its folder, so a matching name
  does not show that the server runs this checkout rather than another
  worktree of the same project; doctor says so instead of claiming a match.
  The Studio bridge only shows itself to requests it trusts (from loopback,
  without an Origin, by the same user), so doctor reports "enabled" or "not
  advertised", never "disabled". Listing Studio sessions and their sync
  health needs the bridge's token, so doctor points to `rojo studio sessions
  --token-file` instead.
* Git: whether Git runs, whether it is at least 2.31, which checkpoints,
  workspaces and releases need, and whether the project is in a repository
  Git can read. It does not report the working tree's state, such as
  uncommitted changes or conflicts; use `git status` for that. Git problems
  are warnings, because serving and building do not need Git.

## Output and exit status

Text output groups the checks by area. `--json` prints a report with
`schemaVersion: 1`, `rojoVersion`, `projectFile`, `checks` (each with
`category`, `code`, `status`, `message` and an optional `hint`), `counts` and
`ok`. Scripts should match on `code`, not on messages.

Doctor exits 0 when no check is an error, warnings included, and 1 when any
check is an error. The report is printed either way, also with `--json`.
