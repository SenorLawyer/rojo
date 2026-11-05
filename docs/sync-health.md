# Sync health: verify, status, wipe guard and backups

Rojo keeps Studio in step with the files on disk, but some things leave them apart without any warning: Studio crashing mid-sync, a sync that stopped halfway, a change the plugin could not apply, or someone pressing Undo on a change Rojo made. These tools show when that happened and fix it. All of them except backup listing need the [Studio bridge](studio-bridge.md) (`rojo serve --studio-token-file`).

## Is each Studio still syncing?

`rojo studio sessions` lists every connected Studio. With this plugin, each session also carries a `sync` summary that the plugin sends with every bridge poll, so it is at most a few seconds old (`syncAgeMs`):

| Field | Meaning |
| --- | --- |
| `status` | `Connected` while syncing. |
| `projectName`, `serverVersion`, `pluginVersion` | What is connected. A plugin and server with different major or minor versions sync, but their features differ, and the plugin warns in Studio's output when they do. |
| `connectedAt`, `lastPatchAt` | Unix milliseconds of the connection and of the last change Rojo applied. |
| `patchesApplied` | Changes applied since connecting, including the initial sync. |
| `lastPatchUnapplied` | Instances in the last change that the plugin could not apply, even after its fallback. Not 0 means Studio is behind the files. |
| `undoneRojoPatches` | Rojo changes undone in Studio and not redone. Not 0 means Studio is behind the files. |
| `heldWipeCount`, `heldWipes` | Scripts the wipe guard is holding back, see below. Up to 20 are listed with their path and line count. |

In a terminal, `rojo studio sessions` also prints a short summary of each session to stderr, with warnings for the cases above; stdout stays JSON. Plugins older than 8.1.0 send no summary, and their sessions show `sync: unknown`.

## Verify Studio against the files

```text
rojo studio --token-file <token> verify --session <id>
rojo studio --token-file <token> verify --session <id> --fix
```

`verify` asks the plugin for a hash of every script it syncs and compares each with the server's copy of that script's file. It lists, with each script's path and file:

* **different**: the script's source in Studio is not its file's contents.
* **missing**: the server has the script but Studio does not, for example because adding it failed.
* **detached**: Studio had the script but it was removed from the place.

It exits with 0 when everything matches and 1 otherwise, printing the full report as JSON on stdout and a summary on stderr. `--fix` sets every *different* script back to its file's contents, in one undo step named `Rojo: Resync`, and exits with 0 when that made Studio match. Missing and detached scripts come back when Studio reconnects to Rojo, which syncs the whole tree again.

Only scripts are compared. Other properties and instances that are not scripts are not checked. Line endings are ignored, since Studio may keep either for a file with CRLF endings. The hash is 32-bit FNV-1a together with the length, which is plenty to tell two versions of a script apart but is not a security check.

`verify` runs from the server, not the CLI, so it compares against what `rojo serve` has loaded. The plugin hashes about 200 scripts per frame, and the command has the bridge's usual 15-second deadline. Each `--fix` batch sends at most 2 MiB of source.

## The wipe guard

When a script with 5 or more non-blank lines would become empty because its file on disk became empty, the plugin keeps the old source in Studio and prints a warning instead. An editor or agent that saves an empty file by mistake then cannot wipe the script in Studio, which used to reach the place before anyone noticed. The hold ends as soon as the file has code again, or the file is deleted. While it lasts, the script shows up in `rojo studio sessions` and as *different* (and `held`) in `verify`.

To empty the script on purpose, write at least one non-blank line, delete the file, or run `verify --fix --include-held`. A plain `--fix` leaves held scripts alone.

The guard is the plugin setting **Guard Against Script Wipes**, on by default. Agent control can turn it on but not off unless **Agent Can Skip Confirmation** is on.

## Undoing a Rojo change

Each change Rojo applies is its own undo step, named `Rojo: Patch <time>`. Undoing one puts back what Studio had before, so Studio no longer matches the files, and with two-way sync the undone values may be written back to disk. The plugin now warns in Studio's output when that happens and counts it in `undoneRojoPatches`, until the step is redone. Run `verify --fix`, or reconnect, to sync the files again.

## Backups outside the place

Before rebuilding a map or replacing a model, save a copy with `backup` instead of copying it into ServerStorage, where copies pile up and get published with the place:

```text
rojo studio --token-file <token> backup create --session <id> --instance game/Workspace/Map --note "before lighting pass"
rojo studio backup list
rojo studio --token-file <token> backup restore <backup-id> --session <id> --parent game/Workspace
rojo studio backup remove <backup-id>
```

A backup is the instance's native `.rbxm` export, the same as `rojo studio export`, saved with a record of the place, the instance's path and the Git commit. They live in `rojo-state/backups` in the repository's Git directory, shared by all of its worktrees and never synced, or in `.rojo/backups` outside Git. `list` and `remove` do not need the bridge.

Restoring inserts the copy under the given parent next to what is there now; it does not replace anything, so remove or rename the current instance first if needed. Restoring scripts needs `--allow-scripts`, and restored Script and LocalScript instances stay disabled, like any [import](studio-bridge.md). The export limits apply: services cannot be backed up directly (back up their children instead), and a backup holds at most 10,000 instances and 5 MiB.

## MCP tools

`rojo mcp` serves these as `studio_verify` (with `fix` and `includeHeld`), `studio_backup` and `studio_backup_list`. `studio_sessions` includes each session's sync summary. Restoring is CLI-only, like import.
