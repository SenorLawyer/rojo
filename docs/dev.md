# rojo dev

`rojo dev` is the daily loop in one process: it runs `rojo doctor`'s checks,
serves the project like `rojo serve` with the Studio bridge on, keeps
`sourcemap.json` current for luau-lsp like `rojo sourcemap --watch`, and
prints where everything is. Stop it with Ctrl+C.

```text
rojo dev
rojo dev path/to/default.project.json
rojo dev --port 34900 --no-sourcemap
rojo dev --force
```

The project argument resolves like `rojo serve`'s: a project file, or a folder
with `default.project.json`, defaulting to the current directory.

## What it does, in order

1. Runs doctor's checks of the project, `servePlaceIds`, packages, the Studio
   plugin and Git (not the running-server check, since `rojo dev` starts the
   server itself) and prints only the warnings and errors. An error stops it
   with exit status 1, unless `--force` is given. `--plugins-dir` is passed to
   the plugin check as for `rojo doctor`.
2. Picks the port. `--port`, then the project's `servePort`, are used exactly
   and are an error when something already listens there. Without either, a
   repository's main checkout (or a project outside Git) takes 34872, the
   plugin's default, when it is free. A linked worktree, or a main checkout
   whose 34872 is taken, takes a port from 34873 to 34936: it starts at a slot
   chosen from a hash of the worktree's path and takes the next free one, so
   the same worktree tends to get the same port each time. A port counts as
   taken when it cannot be bound on the listen address or on the wildcard
   address, so `rojo dev` never shares a port with another `rojo serve`.
3. Serves like `rojo serve`. `--address` and `--allowed-hosts` work as they do
   there, and the project's `serveAddress` and `serveAllowedHosts` apply.
4. Turns on the Studio bridge. The token defaults to `studio-token` in the
   repository's per-user state folder (`%LOCALAPPDATA%\Rojo\projects\<id>` on
   Windows, `$XDG_STATE_HOME/rojo/projects/<id>` or
   `~/.local/state/rojo/projects/<id>` elsewhere, the same folder the Studio
   locks and backups use), shared by every worktree, and is created when
   missing. Outside Git the folder is the project's own state folder.
   `--studio-token-file` uses another file, created when missing.
5. Writes `sourcemap.json` next to the project file, then rewrites it when a
   change to the files changes the sourcemap: a script added, removed, renamed
   or moved, not an edit to a script's source. It is written to a temporary
   file and renamed into place, so luau-lsp never reads half a file, and only
   when its contents differ, so an unchanged sourcemap is never touched.
   `--no-sourcemap` turns this off. Paths are relative and only scripts are
   included, as `rojo sourcemap` does by default.
6. Records the server's loopback URL and token path in `dev.json` in the
   worktree's per-user state folder, and prints the status block.

## The status block

```text
rojo dev is serving Game (C:\work\game\default.project.json)
  Server:        http://localhost:34901/ (port 34901, picked for this worktree)
  servePlaceIds: 1234567
  Studio plugin: installed, the same version as this CLI
  Studio token:  C:\Users\me\AppData\Local\Rojo\projects\0123abcd4567ef89\studio-token
  Sourcemap:     C:\work\game\sourcemap.json (kept up to date)
  Studio bridge: rojo studio --address http://127.0.0.1:34901 --token-file '...' sessions
                 In this worktree, `rojo studio sessions` finds both by itself.
  In Studio, set the Rojo plugin's port to 34901 before connecting.
Press Ctrl+C to stop.
```

The plugin line comes from doctor's plugin check. The port line is only shown
for a port other than 34872, the plugin's default. When the server is bound to
a single non-loopback address, `rojo studio` cannot reach it, and the block
says so instead of printing the command.

## Commands that find it without flags

`rojo studio`, `rojo test` and `rojo mcp` take `--address` and `--token-file`
as before. When either is left out, they use the `dev.json` of the worktree
they run in (`rojo mcp`: the worktree of its `--project`, looked up on every
tool call, so a restarted `rojo dev` on another port is found), then
`http://127.0.0.1:34872` and the shared `studio-token` if it exists. Outside
Git, `dev.json` is found from the project folder or any folder below it.

`dev.json` is removed when `rojo dev` stops on Ctrl+C, Ctrl+Break, closing the
console window, SIGTERM or SIGHUP. A `rojo dev` that is killed leaves it
behind, pointing at a port nothing listens on, until the next `rojo dev` in
that worktree replaces it; commands then fail to connect as they would without
it. Two `rojo dev` in one worktree share one `dev.json`, and the last one
started wins.

## Stopping

Ctrl+C stops the server and drops its connections, finishes any sourcemap
write in progress, removes `dev.json`, prints `rojo dev stopped.` and exits
with status 0.
