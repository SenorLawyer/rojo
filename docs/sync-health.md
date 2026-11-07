# Sync health: verify, status, wipe guard and backups

Rojo keeps Studio in step with the files on disk, but some things leave them apart without any warning: Studio crashing mid-sync, a sync that stopped halfway, a change the plugin could not apply, or someone pressing Undo on a change Rojo made. These tools show when that happened and fix it. All of them except backup listing need the [Studio bridge](studio-bridge.md) (`rojo serve --studio-token-file`).

## Is each Studio still syncing?

`rojo studio sessions` lists every connected Studio. With this plugin, each session also carries a `sync` summary that the plugin sends with every bridge poll, so it is usually a few seconds old, longer while Studio is busy with a command or in Run mode (`syncAgeMs` says how old):

| Field | Meaning |
| --- | --- |
| `status` | `Connected` while syncing. |
| `projectName`, `serverVersion`, `pluginVersion` | What is connected. A plugin and server with different major or minor versions sync, but their features differ, and the plugin warns in Studio's output when they do. |
| `connectedAt`, `lastPatchAt` | Unix milliseconds of the connection and of the last change Rojo applied. |
| `patchesApplied` | Changes applied since connecting, including the initial sync. |
| `pendingSourceWrites`, `failedSourceWrites` | Script sources Studio is still writing, and those it could not write since connecting. Not 0 failed means Studio is behind the files. |
| `lastPatchUnapplied` | Instances in the last change that the plugin could not apply, even after its fallback. Not 0 means Studio is behind the files. |
| `undoneRojoPatches` | Rojo changes undone in Studio and not redone. Not 0 means Studio is behind the files. |
| `heldWipeCount`, `heldWipes` | Scripts the wipe guard is holding back, see below. Up to 20 are listed with their path and line count. |

In a terminal, `rojo studio sessions` also prints a short summary of each session to stderr, with warnings for the cases above; stdout stays JSON. Plugins older than 8.1.0 send no summary, and their sessions show `sync: unknown`.

## Verify Studio against the files

```text
rojo studio --token-file <token> verify --session <id>
rojo studio --token-file <token> verify --session <id> --fix
```

`verify` asks the plugin for a hash of every script it syncs and compares each with the server's copy of that script's file. It lists, with each script's path and file (the first 200 of each kind within 4 MiB of JSON; `--limit N` and `--max-bytes N` change that, `--all` lists up to 10,000, and the counts always cover every script):

* **different**: the script's source in Studio is not its file's contents.
* **missing**: the server has the script but Studio does not, for example because adding it failed.
* **detached**: Studio had the script but it was removed from the place.

It exits with 0 when everything matches and 1 otherwise, printing the full report as JSON on stdout and a summary on stderr. `--fix` sets every *different* script back to its file's contents and exits with 0 when that made Studio match. Each batch of up to 2 MiB of source is one undo step named `Rojo: Resync <time>`, so a fix of a small place undoes in one step. A script larger than 2 MiB is sent in a batch of its own, and one too large for a single bridge command (about 4 MiB of source, less when much of it must be escaped in JSON) is reported as failed; reconnect Studio to sync it. A script counts as fixed only once Studio has written its new source. A script that changed in Studio after the check, for example because a newer change from disk arrived first, is skipped rather than overwritten and counted in `skippedChanged`; run `verify` again. A file that became empty after the check is held by the wipe guard like any other change, unless `--include-held` is passed. `--fix` starts no new batch after 20 seconds, so it ends before the CLI gives up on it, and cancelling the request (`rojo studio cancel` with its `--request-id`, or the MCP call) stops the batch that is waiting for Studio and those after it; a batch Studio already started finishes its current script and stops at the command's deadline or when sync disconnects. Missing and detached scripts come back when Studio reconnects to Rojo, which syncs the whole tree again.

Scripts are read as the reconciler reads them: from the script editor when a script is open in Studio, since unsaved editor changes are what Studio runs next. Only scripts are compared. Other properties and instances that are not scripts are not checked. Line endings are ignored, since Studio may keep either for a file with CRLF endings. Scripts match when two independent 32-bit hashes (FNV-1a and djb2) and the length all match, which tells apart any two versions of a script that were not made to collide on purpose. They are not a security check; `--fix` also compares the exact source verify saw before writing.

`verify` runs from the server, not the CLI, so it compares against what `rojo serve` has loaded. The plugin hashes about 200 scripts per frame, and the command has the bridge's usual 15-second deadline. Each `--fix` batch sends at most 2 MiB of source.

## The wipe guard (experimental)

The wipe guard is experimental and **off by default**. Turn it on with **Guard Against Script Wipes** in Rojo's settings. It covers the common case below, but some rarer sequences can still empty a held script or hide a hold; they are listed under [Known gaps](#known-gaps). `verify` works the same either way.

When a script with 5 or more non-blank lines would become empty because its file on disk became empty, the plugin keeps the old source in Studio and prints a warning instead. An editor or agent that saves an empty file by mistake then cannot wipe the script in Studio, which used to reach the place before anyone noticed. The hold ends as soon as the file has code again, or the file (or a folder containing it) is deleted. The sync fallback, which replaces an instance with the server's copy when a change cannot be applied, leaves held scripts alone. Holds last while Studio stays open, also across reconnects: a reconnecting session pairs each held script with its file by name and place in the tree. A held script that cannot be paired that way, for example because it was renamed or moved in Studio, is kept in Studio rather than removed, with a warning, but is no longer synced until it is put back or its file has code again. While it lasts, the script shows up in `rojo studio sessions` and as *different* (and `held`) in `verify`.

To empty the script on purpose, write at least one non-blank line, delete the file, or run `verify --fix --include-held`. When the script's class changed on disk too while it was held, `--include-held` cannot empty it, since the class change replaces the instance; it is reported as skipped, and writing code to its file or deleting the file ends the hold as usual. A plain `--fix` leaves held scripts alone.

The guard checks each change from disk when it arrives. Studio writes a script's new source a moment later, so a change that emptied a small script and is still being written when a later change makes the script long and a hold starts can still finish; this needs several changes to the same script within a fraction of a second.

The guard is the plugin setting **Guard Against Script Wipes**, off by default. Agent control can turn it on but not off unless **Agent Can Skip Confirmation** is on.

### Known gaps

These are why the guard is experimental. Each needs a specific sequence of changes; `verify` still shows the result.

* A folder (or other container) whose class changes on disk, such as a Folder becoming a Model, is replaced with its contents. Held scripts inside it are not carried over, so the replacement has their empty source.
* A class change from disk that arrives while `verify --fix --include-held` is writing a held script can still let that write empty it.
* After a reconnect, a held script that could not be paired with its file (renamed or moved in Studio) is kept, with a warning, but is not counted in `rojo studio sessions`.
* Studio writes sources a moment after a change arrives, and the guard checks each change when it arrives, so several changes to the same script within a fraction of a second can still end with it empty.
* A script whose class changes on disk (for example from LocalScript to Script, inside a model file) in the same save that empties it is replaced by an empty script, unless it was already held.
* When several sibling scripts share one name and are replaced by new ones, a held script's code is not carried over, since it is not clear which replacement is which.

Turning the guard off ends every hold. Studio keeps the code it has until each file changes again; run `verify --fix` to sync them right away.

## Undoing a Rojo change

`verify` compares the scripts Rojo tracks. A script that is in Studio only because Undo brought it back after its file was deleted is no longer tracked, so `verify` does not list it; reconnect to remove it.

Each change Rojo applies is normally its own undo step, named `Rojo: Patch <time>`. When another plugin or tool is recording a change at that moment, Rojo's change becomes part of that recording instead, and undoing it is not detected; `verify` still finds the difference. Undoing one puts back what Studio had before, so Studio no longer matches the files, and with two-way sync the undone values may be written back to disk. The plugin now warns in Studio's output when that happens and counts it in `undoneRojoPatches`, until the step is redone. Run `verify --fix`, or reconnect, to sync the files again.

## Backups outside the place

Before rebuilding a map or replacing a model, save a copy with `backup` instead of copying it into ServerStorage, where copies pile up and get published with the place:

```text
rojo studio --token-file <token> backup create --session <id> --instance game/Workspace/Map --note "before lighting pass"
rojo studio backup list
rojo studio --token-file <token> backup restore <backup-id> --session <id> --parent game/Workspace
rojo studio backup remove <backup-id>
```

A backup is the instance's native `.rbxm` export, the same as `rojo studio export`, saved with a record of the place, the instance's path and the Git commit. They live in a per-user folder (`%LOCALAPPDATA%\Rojo\projects\<id>\backups` on Windows), chosen by the repository's common Git directory so every worktree shares it, or by the current directory outside Git. `backup` uses the current directory's folder unless `--project <path>` names a project file or folder; `rojo serve` saves its automatic backups in the folder of the project it serves, so pass `--project` with that project when running `backup` from elsewhere (the server logs the folder and this command when it saves one, and `verify --fix` reports it as `storeDir`). It is never inside the project, so `rojo serve` never syncs them. `list` and `remove` do not need the bridge.

### Automatic backups before `verify --fix`

`verify --fix` exports a copy of each script it is about to overwrite, with the text its editor shows and without its children, and saves them before writing any (one backup per 4 MiB of source or 2,000 scripts). A script is exported only while its editor text can be read, is valid UTF-8 (a model cannot hold other bytes as they are) and is exactly the source verify saw (not just the same hash), since the fix is checked against that text; one edited in between, or whose editor text Studio cannot give, is left out. Each backup is first put through exactly what `backup restore` does to a model (the isolated check, re-encoding it within the 5 MiB limit, then the import check the restored model meets), and every script in it must come back with the source verify saw, all in the same bounded check process, so one that could not be restored as it is is not saved, and it is pinned so no prune removes it meanwhile. The pins are released when the fix ends only if Studio reported every write done; after a fix that failed, stopped or was cancelled they stay until they expire, and a cancelled fix stops its pending writes in Studio. The report lists a summary of each under `backups` (`id`, `storeDir`, `rootCount`, `bytes`; `backup list` has the full record), and the summary on stderr prints each ID. Only scripts in a backup are fixed: one Studio did not export, because it was removed from the place or edited after the check, or whose backup is gone from the folder by the time the fix starts, is counted in `notBackedUp` and listed in `notBackedUpScripts`. If the export or the save fails, `--fix` changes nothing and says why. `--no-backup` fixes without a backup; `studio_verify` in `rojo mcp` always takes one. The CLI first checks that `rojo serve` lists the `verifyBackup` capability with its sessions, and refuses `--fix` against an older one, which would fix without a backup. Saving a backup waits a few seconds at most for the backup folder's lock, so a folder another process keeps locked fails the fix before anything is overwritten.

These backups are named `auto-verify-fix`. Their note is a short summary, and `roots` in the record lists every script in the model in order with its full path (a backup whose record would be over 4 MiB is refused instead), which `list --json` shows (`--no-roots` gives only `rootCount`); each script copy also has a `RojoBackupOf` attribute with its script's path. `list` marks them `[automatic]` (`"automatic": true` in JSON).

Every automatic save prunes, and room is made before the new backup is written: if old automatic backups that must go cannot be removed, for example because Windows will not let a file be deleted, the new one is not written at all, so the folder never grows past its limits; what a failed removal left behind still counts and is removed by the next prune. A model without a record is removed only if the claim file written before an automatic backup's model says it is automatic; others stay and `backup list` shows them. Backups made on purpose whose name starts with `auto-` get IDs starting `manual-auto-`, so they never look automatic. The 20 newest automatic backups are always kept; older ones are kept while they are under an hour old, which is far longer than a fix runs, so a prune by another worktree or server does not remove backups a running `verify --fix` just saved. On top of that there is a hard cap: never more than 100 automatic backups or 512 MiB of their models, however young, the oldest removed first. A backup a running `verify --fix` is pinned to (a `.pin` file beside it, expiring after 10 minutes) is never removed, nor is one whose pin cannot be read. A fix that would need more backups than the cap of 100 is refused before it exports or changes anything (fix fewer scripts at once, or pass `--no-backup`), and no pinned backups, a fix's own included, may go over the cap; releasing a fix's pins prunes other backups but never the ones that fix just made. Temporary files an interrupted automatic save left behind are removed once they are 10 minutes old, and count towards the limits until then. A backup about to be saved is never the one a save removes, even when others are dated in the future, as after the clock was set back; older ones make room instead. When pinned backups and the temporary files of saves still running leave no room for a new automatic backup, it is refused, and `verify --fix` then changes nothing. Releasing a fix's pins happens after its answer is sent, and gives up after a few seconds if another process holds the folder's lock, leaving the pins to expire. Automatic backup IDs end in a few random characters, so a new one never takes the ID of one just removed. Backups made with `backup create` are never removed automatically. A record that cannot be read is never removed either, but counts towards the limits as if it were an automatic backup. A backup with several scripts restores them all under the given parent.

Restoring inserts the copy under the given parent next to what is there now; it does not replace anything, so remove or rename the current instance first if needed. Restoring scripts needs `--allow-scripts`, and restored Script and LocalScript instances stay disabled, like any [import](studio-bridge.md). The export limits apply: services cannot be backed up directly (back up their children instead), and a backup holds at most 10,000 instances and 5 MiB.

## MCP tools

`rojo mcp` serves these as `studio_verify` (with `fix` and `includeHeld`), `studio_backup` and `studio_backup_list`. `studio_sessions` includes each session's sync summary. Restoring is CLI-only, like import.
