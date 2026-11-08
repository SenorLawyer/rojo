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
   and are an error when something already listens there. Port 0 is refused
   in both; leave the port out to have `rojo dev` pick one. Without either, a
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
   worktree's per-user state folder, and prints the status block. When the
   record cannot be written, `rojo dev` stops with an error instead of
   serving, since flagless commands would otherwise use the default port.

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
says so instead of printing the command. `dev.json` is still written and says
so too, so `rojo studio`, `rojo test` and `rojo mcp` run there without flags
stop with an error asking for `--address` and `--token-file` instead of
using the default port.

## Commands that find it without flags

`rojo studio`, `rojo test` and `rojo mcp` take `--address` and `--token-file`
as before. When either is given, nothing changes: the other keeps its old
default (`http://127.0.0.1:34872`, or no token). When both are left out, they
use the `dev.json` of the worktree they run in (`rojo mcp`: the worktree of
its `--project`, looked up on every tool call, so a restarted `rojo dev` on
another port is found; a cancellation goes to the server its command was sent
to), and only when there is no record, `http://127.0.0.1:34872` and the shared
`studio-token` if it exists, once the server there proves it holds that token
(see below); one that cannot is an error, as for a record. Outside Git,
`dev.json` is found from the
project folder or any folder below it.

A record is only used once it is confirmed: its address must be loopback,
the process it names must still run, and the server on its address must
prove it holds the Studio token and report the same serve session ID and
canonical project file that `rojo dev` recorded. The token itself is not sent
until then: the command sends a random nonce to `/api/studio/prove`, which
needs no token, and the server answers with a hash keyed with a key derived
from the token, over the nonce and its session ID, which the command checks
with its own copy. A process that took the port without the token cannot
answer, and never receives it. The session ID is random for every start, so
a server that took the port later, even another worktree's `rojo dev` with
the shared token, does not match. The check waits up to 20 seconds, longer
than the bridge holds a request while Git is busy, and never follows a
redirect.

A record that cannot be confirmed is an error (exit status 2), and so is a
`dev.json` that cannot be read or is not a valid record; only a missing file
means no `rojo dev` runs there. Neither is a reason
to try the default address: that server is often another worktree's, such as
the main checkout's, and a command meant for this worktree's Studio must not
run against it. The error says to restart `rojo dev`, pass `--address` and
`--token-file`, or delete the record.

`dev.json` is removed when `rojo dev` stops on Ctrl+C, Ctrl+Break, closing the
console window, SIGTERM or SIGHUP. A `rojo dev` that is killed leaves it
behind, so flagless commands in that worktree fail until `rojo dev` starts
there again or the file is deleted. Two `rojo dev` in one worktree share one
`dev.json`, and the last one started wins. Writing and removing it happen
under a lock file next to it (`.dev.json.lock`), and a stopping `rojo dev`
removes the record only while it is still its own, so it never deletes a
newer one's.

Requests that carry the Studio token, from `rojo studio`, `rojo test` and this
check, never follow redirects, since the HTTP client would otherwise send the
token on to wherever a redirect points.

## Ports and tokens

Before taking a port, `rojo dev` checks that nothing holds it on the listen
address, on 127.0.0.1 or ::1, or on either wildcard address, since the plugin
connects to `localhost`, which can resolve to either. It then binds the port
at once and holds it while it sets up. For as long as the server runs, it
also reserves the same port on the other IP version's loopback address (::1
for a server on 127.0.0.1 or 0.0.0.0, 127.0.0.1 for one on ::1, or on :: when that
only accepts IPv6, as it does by default on Windows) with a socket
that is bound but does not listen, so no other server can take it there and
answer `localhost` instead; a client trying it is refused and moves on. If
another launch took an automatically chosen port, on either address, in
between, it tries the next one (up to 8 ports); a port from `--port` or
`servePort` is never swapped. Several worktrees can start `rojo dev` for the
first time at once: the token is written whole to a private temporary file
and renamed into place only while no token exists, under a lock file next to
it, so readers never see part of one and they all end up with the same one.
The default token file must hold exactly what `rojo dev` writes, 64 lowercase
hex digits; one passed with `--studio-token-file` may be any 32-256 letters,
digits or hyphens. Anything else is an error.

A sourcemap write that fails, for example because an editor holds the file,
is retried every second until it succeeds.

## Stopping

Ctrl+C stops the server and drops its connections, finishes any sourcemap
write in progress, removes `dev.json`, prints `rojo dev stopped.` and exits
with status 0.
