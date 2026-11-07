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

### Automatic backups

Rojo also takes a backup on its own before two things that overwrite or remove Studio content:

* `verify --fix` exports a copy of each script it is about to overwrite, with the text its editor shows and without its children, and saves them before writing any (one backup per 4 MiB of source or 2,000 scripts). A script is exported only while its editor text can be read and is exactly the source verify saw (not just the same hash), since the fix is checked against that text; one edited in between, or whose editor text Studio cannot give, is left out. Each backup is first put through exactly what `backup restore` does to a model (the isolated check, then re-encoding it within the 5 MiB limit), so one that could never be restored is not saved, and it is pinned so no prune removes it meanwhile. The pins are released when the fix ends only if Studio reported every write done; after a fix that failed, stopped or was cancelled they stay until they expire, and a cancelled fix stops its pending writes in Studio. The report lists a summary of each under `backups` (`id`, `storeDir`, `rootCount`, `bytes`; `backup list` has the full record), and the summary on stderr prints each ID. Only scripts in a backup are fixed: one Studio did not export, because it was removed from the place or edited after the check, or whose backup is gone from the folder by the time the fix starts, is counted in `notBackedUp` and listed in `notBackedUpScripts`. If the export or the save fails, `--fix` changes nothing and says why. `--no-backup` fixes without a backup; `studio_verify` in `rojo mcp` always takes one.
* The plugin's first sync after connecting, which follows a branch switch or a restart of `rojo serve`, can remove instances that only Studio had and change others. Before applying it, the plugin saves every instance it removes or changes, each with everything inside it, in one model. Serializing them together keeps references between them, such as an ObjectValue pointing at another backed-up instance; references to instances outside the backup are not kept. A script whose open editor shows unsaved text also gets a copy with that text in a `Rojo editor drafts` folder in the backup, with a `RojoDraftOf` attribute naming the script, since the model only holds what was saved. When Studio cannot give a script's editor text, the plugin warns, naming up to 10 such scripts, that their unsaved text could not be backed up, and the backup holds their saved source. When a script in the backup was edited after the backup captured it, for example while it was being sent, the plugin names it in one warning (up to 10 scripts) as the sync overwrites or removes it, checked right before the changes are made, and syncs anyway; so is a script added under a backed-up instance while the backup was prepared, and anything the sync changes that was moved out of what was serialized. Only what the sync removes, replaces or writes is checked. If by then the backup's 5-minute protection window has passed, nothing is applied and the sync ends with a message to reconnect, which takes a fresh backup. An affected instance Studio does not let the plugin read is skipped and named in the warning about instances that could not be backed up. Services cannot be saved as models, so when the sync changes one, such as Lighting's properties, the plugin warns in Studio's output and names them. The backup goes to the server over the Studio bridge with the bridge's plugin token, and the server saves it in the folder `backup list` reads, pinned for 10 minutes so no other save prunes it while Studio applies the sync. A script source change from that sync that is still waiting to be written 5 minutes after the backup was saved is skipped with a warning instead, since its backup may be gone by the time it lands. So is a sync fallback replacement then, which stays unapplied, along with its reference repairs; repairs for a replacement that did not happen are never applied. A repair to an instance outside the backup is applied and named in the same warning, with the properties it changes. Otherwise syncing overwrites Studio as it always does; when it overwrites, or the sync fallback replaces, a script edited after the backup captured it, with text Rojo did not write itself, the plugin warns, naming up to 10 such scripts, that the backup does not hold those edits. The model is read back after serializing to check it holds exactly the roots recorded, in order; roots that are not Archivable are left out and named. Studio does not save instances that are not Archivable, nor what is inside them, so the plugin names any such instance the sync changes or removes, as not in the backup. Syncing never depends on it: without a bridge, when the backup would hold over 10,000 instances (counted before anything is serialized), over 16 MiB of script source and unsaved editor text or 16 MiB of string values, `Text` and attributes (counted before anything is serialized), when preparing it, serializing included, takes over 10 seconds or a model over 5 MiB, when the server refuses it (for example because its checkout changed), or when the server finds it could not be restored, the plugin warns that no backup was taken and syncs anyway. Turn this off with Back Up Before Resync in Rojo's settings. Changes that arrive while connected are not backed up.

Automatic backups are named `auto-verify-fix` or `auto-sync`. Their note is a short summary, and `roots` in the record lists every root of the model in order with its full path (a backup whose record would be over 4 MiB is refused instead), which `list --json` shows (`--no-roots` gives only `rootCount`); each script copy in a `verify --fix` backup also has a `RojoBackupOf` attribute with its script's path. `list` marks automatic backups `[automatic]` (`"automatic": true` in JSON). Every automatic save prunes. If old automatic backups cannot be removed, for example because Windows will not let a file be deleted, the new one is not kept, so the folder never grows past its limits; what a failed removal left behind still counts and is removed by the next prune. A model without a record is removed only if the claim file written before an automatic backup's model says it is automatic; others stay and `backup list` shows them. Backups made on purpose whose name starts with `auto-` get IDs starting `manual-auto-`, so they never look automatic. The 20 newest automatic backups are always kept; older ones are kept while they are under an hour old, which is far longer than any operation that relies on its backups runs, so a prune by another worktree or server does not remove backups a running `verify --fix` just saved. On top of that there is a hard cap: never more than 100 automatic backups or 512 MiB of their models, however young, the oldest removed first. A backup a running `verify --fix` is pinned to (a `.pin` file beside it, removed when the fix ends and expiring after 10 minutes in case it never does) is never removed, even over the cap; a fix with more groups than the cap keeps them all until it ends, and the folder is pruned back within the cap as it ends. A backup just saved is never the one a save removes, even when others are dated in the future, as after the clock was set back; older ones make room instead. When backups pinned by other running fixes fill the cap, a new automatic backup is refused: `verify --fix` then changes nothing, and the sync warns and goes ahead. Automatic backup IDs end in a few random characters, so a new one never takes the ID of one just removed. Backups made with `backup create` are never removed automatically. A backup with several instances restores them all under the given parent.

Restoring inserts the copy under the given parent next to what is there now; it does not replace anything, so remove or rename the current instance first if needed. Restoring scripts needs `--allow-scripts`, and restored Script and LocalScript instances stay disabled, like any [import](studio-bridge.md). The export limits apply: services cannot be backed up directly (back up their children instead), and a backup holds at most 10,000 instances and 5 MiB.

## MCP tools

`rojo mcp` serves these as `studio_verify` (with `fix` and `includeHeld`), `studio_backup` and `studio_backup_list`. `studio_sessions` includes each session's sync summary. Restoring is CLI-only, like import.
