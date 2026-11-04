# Checkpoints, workspaces and releases

Your existing checkout stays where it is. For a project at `Documents/Development/Roblox/Games/DriveForEggs`, the default layout is:

```text
Games/
├── DriveForEggs/                         existing Git checkout
│   ├── default.project.json
│   ├── src/
│   └── wally.toml or pesde.toml
└── DriveForEggs.workspaces/
    ├── winter-update/                   separate Git worktree
    ├── trading-hotfix/                  separate Git worktree
    └── .rojo/
        ├── winter-update/place.rbxl     working Studio place
        └── trading-hotfix/place.rbxl    working Studio place
```

Workspaces go beside the repository's primary worktree, the checkout `git worktree list` shows first. When that is a bare repository, the directory is named after it without `.git`, so `Games/DriveForEggs.git` gets `Games/DriveForEggs.workspaces`, and so does `Games/DriveForEggs/.git`. When the project is a Git submodule, its own checkout is the primary worktree, so the workspaces directory lands inside the superproject's checkout. Add it to the superproject's `.gitignore` there. Worktrees share Git's history, but have independent source files and branches. Each needs its own package installation and `rojo serve` process on a different port. The working place is outside the source checkout so saving it does not make the source revision dirty. Immutable checkpoints and release receipts live under the shared Git directory's `rojo-state` folder, not inside either source branch. Back up that folder as well as Git; pushing commits does not upload these local archives.

## Capture a complete version

1. Commit the source changes you want to capture.
2. Connect the matching Rojo project to Studio and wait for syncing to finish.
3. Stop Play mode and save the Edit place using Studio's native save command, outside the source checkout.
4. Create the checkpoint:

```powershell
rojo checkpoint create live-v1 --place C:/PlaceBackups/DriveForEggs.rbxl --project .
```

Rojo verifies mapped script paths, classes, Sources, RunContext and Enabled/Disabled settings against the saved place. A dirty Git tree, including changes inside submodules that `submodule.<name>.ignore` or `diff.ignoreSubmodules` would hide, a missing script, stale execution settings, or an extra or duplicated executable script path inside a Rojo-owned subtree stops the operation. Studio-owned subtrees that allow unknown instances remain supported, including duplicate script paths there, such as two copies of the same free model. The input file is copied byte-for-byte, including Studio-only content outside the Rojo project. The manifest records its BLAKE3 hash, source commit, the project file's path inside the repository (with `/` separators on every OS), package manifest/lock hashes and tool version. Package files (`wally.toml`, `wally.lock`, `pesde.toml`, `pesde.lock` and `rokit.toml`) are hashed in the project's directory and every directory above it up to the repository root, keyed by their repository path such as `wally.toml` or `places/lobby/wally.toml`. If a project `$path` resolves outside the repository, for example `../shared` or a symbolic link leading out of it, the checkpoint is still created, but Rojo logs a warning naming those paths and the manifest records `sources_outside_repository: true`, because the Git commit does not pin that source. This checks source agreement; it does not prove that the game is bug-free, that ignored package installations are reproducible, or that external uploaded assets have been archived.

Each checkpoint also creates the Git ref `refs/rojo/checkpoints/<name>` pointing at its source commit. Git only keeps commits that a ref reaches, so without it, a checkpoint of a detached commit, or of a branch that is later amended, rebased or deleted, would lose its source once the reflog expires and `git gc` prunes the commit. If the ref already exists and points at a different commit, the checkpoint is refused; delete a stale ref with `git update-ref -d refs/rojo/checkpoints/<name>`. Checkpoints made before Rojo created these refs get one the next time `release prepare` or `workspace create` uses them. Deleting a ref allows Git to prune the commit again once nothing else reaches it. Git does not push these refs by default; to send them to a remote, run:

```powershell
git push origin 'refs/rojo/*:refs/rojo/*'
```

Checkpoint names are immutable. A new checkpoint, release or workspace name that differs from an existing one of the same kind only in case, such as `Base` and `base`, is rejected, because the two would share a file on case-insensitive file systems. `rojo checkpoint list` lists them, and `--verify` also re-hashes each archived place and checks that each manifest names the directory it is stored in. Unreadable manifests are skipped with a warning, and leftovers from interrupted writes (names starting with `.`) are skipped silently unless debug logging is on; `rojo checkpoint export live-v1 --output C:/PlaceBackups/recovery.rbxl` verifies the hash and copies one to a new file. Existing outputs are never overwritten. Keep the original format (`.rbxl` or `.rbxlx`, in any capitalization) when exporting. Place files up to 512 MiB are supported.

For another computer or an off-machine backup, use `rojo checkpoint pack live-v1 --output C:/PlaceBackups/live-v1`. This creates a portable directory containing the manifest and original place bytes. Copy or archive that directory using your normal backup storage. In a clone with the matching Git commit available (fetch it with `git fetch origin 'refs/rojo/*:refs/rojo/*'` if it was only reachable from a checkpoint ref), `rojo checkpoint import --input C:/PlaceBackups/live-v1` checks the hash, imports it without overwriting existing checkpoints and creates the checkpoint ref in that clone. These bundles can be large; the command does not automatically upload them to GitHub.

Bundle integrity is self-attested: the hash is stored in the bundle's own manifest, so it detects accidental corruption but not deliberate changes. Import only bundles from trusted sources. Manifests larger than 1 MiB are rejected. On import, Rojo checks that the source commit is a full commit ID for the repository's object format (40 hex digits for SHA-1, 64 for SHA-256), that it exists in your repository and that the project file exists at that commit. Bundles packed by earlier Windows builds stored the project path with `\` separators; import converts them to `/`. Absolute paths, drive letters and `..` are rejected. It does not repeat the script verification from `checkpoint create`. The imported manifest records `imported: true` and an `import_provenance` entry with the bundle's directory name (not its local location), and publication receipts for it show `checkpoint_imported`.

## Work on an update while keeping hotfixes separate

```powershell
rojo workspace create winter-update --checkpoint live-v1
rojo workspace create trading-hotfix --checkpoint live-v1
rojo workspace list
```

Both commands create a branch named `rojo/<name>` from the checkpoint's commit and a matching working place. They do not copy uncommitted changes from the primary checkout. This is why you can create a hotfix while the main directory contains unfinished update work. If the repository has submodules, they are initialized in the new worktree; a failure there is reported as a warning, and you can run `git submodule update --init --recursive` yourself. Rojo runs Git with terminal and credential-manager prompts turned off, and runs OpenSSH with `-o BatchMode=yes`, so a submodule that needs a password, passphrase or host-key confirmation fails instead of waiting for input. The option is added to `GIT_SSH_COMMAND` or `core.sshCommand` when either names `ssh`. Rojo does not change a `GIT_SSH` program or a command for another SSH client such as PuTTY's plink, so configure those not to prompt. It also ignores inherited variables such as `GIT_DIR`, `GIT_OBJECT_DIRECTORY` and `GIT_CONFIG_*` that Git sets inside hooks, and variables that change which history Git sees: `GIT_SHALLOW_FILE`, `GIT_GRAFT_FILE`, `GIT_REPLACE_REF_BASE` and `GIT_NO_REPLACE_OBJECTS`. Checkpoints, workspaces and releases need Git 2.31 or newer.

Creation refuses to reuse an existing `rojo/<name>` branch. If a step fails after Git created the worktree, Rojo removes that new worktree, its saved place and the new branch, so you can retry with the same name. If `git worktree add` itself fails, Rojo removes the branch only if Git had already created it, and the error says which happened. If cleanup fails, the error names what is left behind; remove it with `git worktree remove` and `git branch -D` before retrying. `rojo workspace list` reports worktrees whose directory was deleted as `missing` instead of failing; `git worktree prune` removes them.

Rojo has no command to delete a workspace. When you no longer need one, save anything you want to keep from its place as a checkpoint first, then from the primary checkout run `git worktree remove ../DriveForEggs.workspaces/winter-update` (Git refuses while it has uncommitted changes), delete `DriveForEggs.workspaces/.rojo/winter-update`, which holds its working place, and delete the `rojo/winter-update` branch with `git branch -D` if its commits are merged or no longer needed. Checkpoints are not affected.

From the new workspace, install packages using the native package manager and open its place:

```powershell
rojo packages doctor . --json
rojo packages install . --manager wally
rojo workspace open
rojo serve --port 34873
```

For pesde, use `--manager pesde --locked` after configuring its Rojo generator. Wally does not implement the same frozen install flag; check lockfile changes before accepting a checkpoint. Package scripts and installation commands run with your normal local permissions. Use a separate test experience for development DataStores. A local saved place may need the project's allowed place IDs adjusted for your test target; never point development at production data simply to pass a connection check.

The Studio bridge exposes session and project identity and suspends its session if the Git branch changes. Connect each Studio window to its workspace's port. The CLI targets explicit session IDs. See [the bridge guide](studio-bridge.md).

## Bring a fix into the update

Commit the hotfix's source changes. In the update workspace:

```powershell
rojo workspace apply <hotfix-commit> --code-only
```

This is an ordinary Git cherry-pick onto a clean tree. If it stops on conflicts, resolve them and run `git cherry-pick --continue`, or run `git cherry-pick --abort`. Other failures, such as a merge commit or a change that is already applied, are reported with Git's message. It does not merge the saved Studio place. If the hotfix changed content, save checkpoints of the common base, hotfix and update, export each to a different file and use [semantic merging](semantic-merge.md). Review the result in Studio, sync the combined code, save it and create a new checkpoint. The engine cannot promise byte-perfect preservation of every Roblox metadata field; original snapshots remain available.

## Prepare, publish or roll back

```powershell
rojo release prepare v1 --checkpoint live-v1 --universe-id 123 --place-id 456
rojo release list
rojo release publish v1 --confirm-target 123:456
rojo release rollback v1 --confirm-target 123:456
rojo workspace create urgent-fix --release live
rojo workspace create arena-fix --release live:123:789
```

Publishing and rollback require an Open Cloud key in `ROBLOX_API_KEY`, or another environment variable named by `--api-key-env`. A variable that is not set and one that is not valid UTF-8 are reported differently. Keys are not stored in manifests or command arguments. Both operations upload the archived bytes that were just hashed and checked, never rebuild your current working tree. `--confirm-target` must exactly match the prepared target. No server restart or player-data rollback is performed.

Rollback only republishes a release that has a successful publication receipt for the same target in this repository. Pass `--force` to publish an earlier release that was never published by Rojo, for example one deployed through Studio.

`live:UNIVERSE_ID:PLACE_ID` means the last release Rojo successfully recorded publishing to that place, not an independently verified query of Roblox. Each place's record is kept in `rojo-state/live/<universe>-<place>.json`. Plain `live` works while only one place has a recorded publication; once several do, it fails and lists the `live:` names to choose from. `rojo release list` shows every place's record under `live`. `last-publication.json` still records the most recent publication of any place; records written before per-place tracking are read from it. Publishing through Studio or another tool can make these pointers stale. Prefer a named release when uncertain. Releases for multiple places are separate operations, not an atomic experience-wide deployment.

Open Cloud does not support updating all instance types. Rojo rejects snapshots containing known restricted classes, including unions, SurfaceAppearance and wrap objects. It also rejects instances whose class is missing from Rojo's bundled reflection database, since their publishing support cannot be checked. Services directly under the place are the exception: newer Studio versions add services before Rojo knows them, so an unknown service only produces a warning in the output and receipt. Everything inside it is still checked. Use `checkpoint export`, inspect the place and publish through Studio for those cases. A deployment can also fail due to permissions, asset availability or Roblox service errors. See [Roblox's publishing limitations](https://create.roblox.com/docs/cloud/guides/usage-place-publishing).

Every attempt takes `publication.lock`, which names its attempt ID and process ID, and writes a local journal entry under `publication-attempts` before the request. Both are flushed to disk first. If the journal cannot be written, the lock is released and nothing is uploaded. When a lock is left behind, the next attempt's error names the journal entry to inspect, or says that the attempt has no entry and so uploaded nothing. Locks from earlier versions are empty; check `publication-attempts` for a `pending` or `unconfirmed` entry before removing one. Rojo sorts failed attempts by whether Roblox could have created a version:

- If the request was never sent (DNS, connection or TLS failure, invalid key format) or Roblox refused it with a 4xx status such as 401, 403 or 429, nothing was published. 408, 409 and 499 are the exception: they can arrive after Roblox acted on the upload, so they count as unconfirmed. The receipt is marked `failed` with Roblox's error message, the lock is released, and you can retry once the cause is fixed.
- After a timeout, a 5xx status, a dropped connection or a success response without a valid version number, the upload may have succeeded. Rojo keeps `publication.lock` and an `unconfirmed` receipt, blocking another attempt. Check Roblox version history, then record what it shows:

```powershell
rojo release resolve <attempt-id> --version 42
rojo release resolve <attempt-id> --not-published
```

The attempt ID is in the error and is the journal entry's file name. `--version` marks the receipt `published` and writes the place's `live` record and `last-publication.json`, unless a later publication already wrote them. `--not-published` marks it `failed`. Both record `resolved_manually_unix` and remove `publication.lock` if that attempt holds it; a lock held by another attempt, or an empty one from an earlier version, is kept. Only `pending` and `unconfirmed` attempts can be resolved, plus a `published` one with the same version whose remaining records were not written.

A `pending` attempt may still be uploading. Rojo cannot portably check whether the process ID recorded in the lock is still running, so it judges by the lock's age instead: while that attempt's lock is younger than the longest a publication can take (about 38 minutes: 30 seconds to connect, the 36-minute upload limit for a 512 MiB place below, and a minute of local work), `resolve` refuses and changes nothing. Make sure the publishing process has exited, then pass `--force` to resolve it anyway. `unconfirmed` attempts and locks that belong to another attempt are not affected.

A successful response records the returned Roblox version number. The version is printed before the journal is updated, so it is still visible if writing the receipt fails. In that case the lock stays in place, and the error names the records that still lack the version, such as the place's `live` record, and the `rojo release resolve` command that writes them and removes the lock. If Rojo cannot update the journal or remove the lock after a failed attempt, the error still starts with the publication result and then names the file to fix. When a rejected attempt's journal entry cannot be updated, it still says `pending` although nothing was published; never resolve it with `--version`. Use `--not-published` once the cause is fixed.

Connecting must succeed within 30 seconds. The whole upload may take 120 seconds plus one second per 256 KiB of place data, about 36 minutes for a 512 MiB place. `release list` reports local receipts and does not claim that existing servers run that version.

## Stage, smoke check and promote

To try a release on a test place before players get it, stage it there first and promote it once it passes:

```powershell
rojo release stage v1 --staging 900:901 --smoke smoke.luau --smoke-timeout 60
rojo release promote v1 --confirm-target 123:456
```

`release stage` publishes the release's exact archived bytes to the staging place, through the same lock, journal and checks as `release publish`. The staging place must be a different place from the release's target; giving the release's own place ID, under any universe ID, is refused before anything is uploaded, and so is a place that another release targets or that Rojo has published live. The staging place must also be in a universe that no release targets and Rojo has not published to, including the release's own. Places in one universe share its DataStores and other universe-wide data, so a smoke script run there reads and writes that game's live player data, and players can teleport into the staging place. `--allow-live-universe` accepts such a staging place anyway; use it only when the smoke script leaves shared data alone. These checks read every release manifest and `live` record, and `last-publication.json`; if one cannot be read, staging is refused and the error names the file to repair, since it could be the record of the place being staged to.

Rojo can only protect the places it knows about. Unlike `release publish`, `release stage` has no `--confirm-target`, so a mistyped `--staging` ID that no release or `live` record names publishes to whatever place the API key can write, including a live game Rojo does not track. Check the ID before running, and use an Open Cloud key that can only write to your test places where you can. Staging writes no `live` record and does not change `last-publication.json`, and its journal entry has `purpose: "staging"`. An unconfirmed staging upload keeps `publication.lock` like any other and is settled with `rojo release resolve`, which for a staging attempt writes no `live` record either. A resolved staging attempt never ran its smoke check, so stage the release again before promoting it.

With `--smoke`, Rojo then runs the script with Open Cloud Luau Execution against the version that staging just created, in a server Roblox starts for it. The key needs the `universe.place.luau-execution-session:write` scope as well as `universe-places:write`. `--smoke-timeout` is how long the script may run, from 1 to 300 seconds (60 by default); the task can wait in Roblox's queue for up to two minutes longer before Rojo gives up polling. The script passes only when it completes and its first return value is `true` or a table whose `ok` field is `true`:

```lua
local ok = workspace:FindFirstChild("SpawnLocation") ~= nil
return { ok = ok, message = if ok then "spawn found" else "no SpawnLocation" }
```

Everything else fails: a script error, running past the timeout, returning nothing or `nil`, `false`, `{ ok = false }`, a table without `ok = true`, and any other value such as a string or number. Requiring an explicit `true` means a script that forgets to report a result never counts as a pass. A `message` string in the returned table is shown when the check fails. When Open Cloud itself fails, for example the task cannot be created or is still running when polling stops, the result is `error`, which also blocks promotion. Scripts are read before anything is uploaded and may be up to 4,000,000 bytes, counted after JSON escaping, so a script with many quotes, backslashes or control characters must be smaller on disk.

Each stage writes `rojo-state/staging/<attempt>.json` with the release, place hash, staging target, staged version number, status (`passed`, `failed`, `error`, or `staged` when no smoke script was given), the smoke result with the task path, reason, return values and the last 50 log messages, `staged_unix`, and `staging_seq`. Each staging upload takes the next `staging_seq` while it holds `publication.lock`, and its journal entry records the same number. Promote decides which stage is latest by this number rather than by time, so a clock that moves backward between two stages cannot rank a later failure below an earlier pass. Numbering reads every staging record and `publication-attempts` entry, so when one cannot be read, the stage releases the lock and uploads nothing. A smoke check that does not pass is recorded and the command exits with an error; the staging place keeps the new version and the live place is not touched. `rojo release list` shows these records under `staging`.

`release promote` checks the release's latest staging record and only then performs the same live publication as `release publish`: the same `--confirm-target`, lock, journal, receipt and `live` record, with `purpose: "promote"` and the staging attempt in `promoted_from_staging`. It refuses, uploading nothing, when:

- the release has no staging record;
- the latest record is for different place bytes than the release has now;
- the latest record did not pass. Only the latest counts, so a failed stage after a passing one blocks promotion until a later stage passes;
- the release was uploaded to staging again after the stage the latest record describes, for example by a stage whose smoke check was interrupted or is still running, or whose upload was settled with `release resolve`. Only the newest upload counts. A rejected upload, or one resolved with `--not-published`, created no version and does not count;
- the latest record was made while the release had a different target;
- the latest record has no `staging_seq`, or one that differs from its journal entry's, as records from earlier development builds do. Stage again;
- the latest record is dated more than five minutes in the future, which means the clock was wrong when it was written and its age cannot be trusted. Once the clock is right, stage again;
- the latest record ran no smoke check, unless you pass `--allow-unsmoked`;
- the record is older than `--max-age-hours` (24 by default);
- a staging record cannot be read, since it could be the latest failure. Fix or remove that file;
- an entry in `publication-attempts` cannot be read, since it could be a newer staging upload. Repair it, or check Roblox version history and move it out of `publication-attempts`.

Promote runs these checks once before it takes `publication.lock` and again once it holds it, so a `release stage` that uploads in between is not missed. When the second check refuses, promote releases the lock and uploads nothing.

Staging proves that this version loaded on the test place and that the script's checks passed there. It does not prove the game works for players, and the test place's own settings, data stores and assets may differ from the live place's. Rojo state records, including staging records and journal entries, are refused when larger than 1 MiB, and the fields a smoke record copies from Roblox are shortened to keep them small.

Lifecycle commands always return structured JSON on success; `--json` is accepted explicitly. Errors return a nonzero exit status. These commands do not change your primary project unless you explicitly run `workspace apply` there.
