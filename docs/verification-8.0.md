# 8.0 development verification

This change is stacked on the reliability work in PR #1. Verification was performed on November 4, 2026, on Windows x64. It is a development build, not a production deployment. Each section names the PR #2 branch commit it was measured at. These SHAs are historical references: PR #2 is merged by squashing, so they will not be on `master`.

Commits after branch commit `039f6cf` were not re-measured here. For them, GitHub CI runs only these checks:

- `cargo test --workspace --locked` on Windows x64 (release profile), Ubuntu 22.04 x64 and arm64, and macOS.
- Exporting the embedded Studio plugin with `rojo plugin install --plugins-dir` on each of those platforms. This builds the plugin model but does not load or run it.
- On Windows only: the release watcher stress script (`scripts/watcher-smoke.py`) and the opt-in semantic merge tests against the pinned rbx-merge engine, including Git drivers in linked worktrees. Linux and macOS do not run the real-engine merge tests.
- A Rust 1.88 (MSRV) build, rustfmt, all-target Clippy with `-D warnings`, StyLua and Selene.

CI does not run the plugin `.spec.lua` suites, which need Studio, `scripts/package-smoke.py`, which needs network access and native Wally, pesde and Lune binaries, or any live Studio check: bridge and MCP operations, reconnect timings, package installs while connected, or the settings UI. Plugin specs and live Studio checks are run manually.

Plugin specs re-run in Studio for 8.0.0-dev.5, after the child-process import check and bridge retry changes: 255 passed, 0 failed, 1 skipped. A live 8.0.0-dev.5 `rojo serve` with the Studio bridge on then served a scratch place's handshake, initial read and sync socket, and delivered two edits made before and after a `git commit`. `rojo studio import` of a 13-byte attribute blob declaring 2^32 keypoints reported "Import model could not be decoded" after the check process failed to allocate 51 GB, and the server kept answering.

## 8.0.0-dev.6 (PR #4)

Measured on November 4, 2026, on Windows x64 with Studio, against private test experiences set aside for this; no game that players use was touched:

- **Bridge stall.** With the installed 8.0.0-dev.5 plugin connected to a scratch place, `rojo studio status` every 5 seconds returned HTTP 504 within the first minute, and the session then expired. A diagnostic build logged each `RequestAsync`: only one bridge request was ever in flight, yet each took longer than the last (about 2, 4, 7, 9 and 13 seconds) while the server answered the same requests in 2 seconds (polls) and 1 millisecond (results) when sent from outside Studio. A fresh plugin sent 25 quick requests in 0.6 seconds; once it alternated 2-second held requests with quick ones, every later request was delayed further. The same pattern over a `RawStream` client stayed at 2.02 and 0.02 seconds for all 12 rounds. With the dev.6 plugin, which polls over its own stream, the same 5-minute soak passed 57 of 57 commands (median 0.21 seconds, slowest 1.1 seconds). Instance names and log lines with non-ASCII text round-tripped intact over the stream.
- **`rojo test`.** In Studio, a fixture with two passing tests, one failing test and one spec that fails to load reported exactly that and exited 1; `--root nope` exited 2. A spec that waits 45 seconds passed through the bridge, longer than the 30-second session expiry, because the plugin keeps the session alive during Run mode. Cancelling that spec 12 seconds into its run returned 410 to `rojo test`; 90 seconds later the same session was still registered and ran the next test. `--cloud` saved the built place as a new unpublished version of the test place, ran the specs there with Luau Execution and gave the same result.
- **Staged releases.** `release stage` published a checkpointed release to the staging test place and its smoke script passed against that version; `release promote` then published it to a second test place. A stage whose smoke script returned `{ ok = false }` was recorded as failed and promote refused.
- **Assets.** `rojo assets sync` uploaded two 8x8 test images (both approved), a rerun uploaded nothing, and a rename kept the existing asset id.
- **Data modules and logs.** A `.data.csv` file arrived in Studio as a typed ModuleScript and a later edit synced live. `rojo studio logs --follow` printed new Studio output, including warnings, as it appeared.
- **Specs.** The plugin specs passed in Studio after the review fixes: 304 passed, 0 failed, 1 skipped.

## Observed results

These results were measured at branch commit `ca36245` (8.0.0-dev.1). After branch commit `3ffbfb4` only the focused macOS path regression described below was re-run.

- The optimized Rust workspace suite passed, apart from existing skipped tests. The opt-in external-engine tests passed separately against the pinned rbx-merge executable.
- The native Studio plugin suite passed, apart from an existing skipped test.
- Rust formatting, strict all-target Clippy, StyLua and Selene passed.
- Windows release watcher stress: 322 scripts survived ten Git checkouts, twelve package replacements, delayed mapped-directory recreation, two-way init writes, server restart and subsequent writes. Every stage converged without a server crash.
- Wally and pesde fixtures installed with native managers, built through Rojo, and executed successfully in an isolated Studio fixture. See [package evidence](packages.md).
- Native Studio bridge operations, cancellation recovery and project-identity guards were exercised in an isolated Edit fixture. See [bridge evidence](studio-bridge.md).
- Checkpoint regressions cover exact archived bytes, dirty/stale source rejection, unexpected owned scripts, execution settings, portable import, immutable exports and independent hotfix worktrees.
- Publication tests used a loopback HTTP server and injected upload responses. They verified exact bytes, target checks, successful receipts and ambiguous-failure locks. No live Roblox publication or rollback was attempted.
- The pinned merge engine's upstream suite passed. Adapter integration covered all four Roblox formats, conflicts, reference preservation and actual Git drivers in linked worktrees. See [merge evidence](semantic-merge.md).

## Review findings

Automated review covered the working-tree diff against `73f9e45449dbd0f6f7adc577fc6e250675e04aba` and the subsequent corrections. This is not an approval of the pull request.

The first macOS CI run found a test path comparison between `/var` and its canonical `/private/var` alias. The test now canonicalizes its created worktree before checking containment; production behavior is unchanged. The focused Windows regression passed after this correction.

| Finding | Outcome |
| --- | --- |
| S1: late cancelled bridge results disconnected normal sync | Fixed; regression tests and a real cancelled Run simulation retained the same connected session. |
| S2: checkpoint verification missed extra scripts in owned subtrees | Fixed; owned extras are rejected, permitted Studio-owned content remains supported, and execution settings are compared. |
| S3: nested server package mappings could evade exposure checks | Fixed; ancestor and descendant mappings are checked for both managers. |
| T1: publishing restrictions missed subclasses | Fixed; preflight follows reflection inheritance and rejects unknown classes. |
| S4: duplicate Studio-owned script paths are rejected | Deferred nit; this conservative checkpoint limitation is documented. |

## Boundaries

Whole-place checkpoints require a native Studio save in Edit mode. Semantic merging cannot preserve every file-level metadata field. Open Cloud publishing is restricted to supported content and does not restart servers or restore player data. The local `live` pointer records only publications made through this tool. A local passing suite does not replace GitHub's platform checks or gameplay testing of a real release.

## Installed dev.4 follow-up

The following checks were measured at branch commit `039f6cf` (8.0.0-dev.4) on Windows x64 on November 4, 2026. They cover the connection, UI, agent-control and package-install fixes made after the results above.

- The Rust workspace suite passed, apart from existing ignored tests. The opt-in merge-engine integration tests passed separately.
- The native Studio suite passed, apart from an existing skipped test. Rust formatting, strict release all-target Clippy, StyLua and Selene passed.
- The installed dev.4 CLI and plugin connected in the user's designated small Modeling Test place through Studio MCP. Five reconnects took 0.26–0.56 seconds, including reconciliation and socket opening. Initial HTTP requests took about 0.26 seconds in total. These are small-place measurements, not promises for large games.
- A forced test-server restart reported a disconnected/error state. Reconnecting to the replacement server completed in under half a second in the observed check.
- Disk source edits, a new ModuleScript, its rename and removal, a Part position change, and Unicode TextLabel changes arrived in Studio. The native UI exported through the CLI bridge as a 3,978-byte model. No whole-place save or publication was performed.
- The user confirmed the settings layout and separate version/sync-time rows visually. No desktop control was used for this follow-up.
- Real Wally and pesde installs ran while Studio was connected. Promise and `osyrisrblx/t` entry points, plus a local pesde dependency, executed successfully in Studio. Repeated Wally installs and a locked pesde reinstall retained working dependencies and delivered a subsequent source edit. Fresh cloned module trees were required to avoid Roblox's cached `require` results masking a stale package.
- Native package smoke fixtures also passed installation, repeat installation, lockfile checks, diagnostics, generated project configuration and XML builds for both managers.
- The final Windows watcher run converged across 322 scripts, ten Git checkouts, twelve package replacements, mapped-directory recreation, two-way init writes, restart and subsequent writes.
- Agent control rejected wrong place/version targets, concurrent preview ownership, and invalid settings. A disconnected setting change was read back and restored. Bridge retry preserved normal sync.
- The original CLI shim and managed plugin hashes were unchanged. Versioned preview artifacts were installed alongside them; PATH was not changed.

### Failures reproduced and corrected

1. Studio's RawStream delivery truncated binary MessagePack at a NUL byte. Opt-in base64 transfer now preserves finite HTTP bodies, while old clients retain raw responses. The installed plugin successfully read the complete package-filled project through this path.
2. Wally reinstall deleted instances referenced by queued subscription history. Publication now freezes each patch's instance contents under the tree lock, while retaining the original internal patch for sourcemap consumers.
3. A package extraction could finish after an intermediate snapshot failed, leaving a partial tree until another edit. Queued events are reconciled by affected DOM subtree and receive one trailing rescan after filesystem activity settles. Independent mappings of the same file remain distinct. Real reinstall tests were repeated after both fixes.

### Unclean-data coverage

Malformed/empty/null/truncated MessagePack and invalid base64 request bodies returned HTTP 400, left source unchanged and did not prevent a subsequent valid write. A write held behind a Git index lock for more than five seconds reached disk after unlocking.

Studio tests cover empty, Unicode, embedded-NUL and 64-KiB strings through MessagePack/base64, non-finite MessagePack numbers, truncated decode recovery, and rejection of NaN/infinity/fractional/out-of-range control inputs. Preserving a numeric payload is different from accepting it as a port or threshold.

Roblox itself truncates a `StringValue.Value` at an embedded NUL, including a direct assignment without Rojo. The transport test preserves those bytes, but cannot make the engine store a property value it rejects or normalizes. This is not blanket validation of every Roblox property or every malformed place file.

### Follow-up review findings

| Finding | Outcome |
| --- | --- |
| S1/T1: version ownership guarded only the agent entry point | Fixed in the shared session-start path; both controller orders tested. |
| S2/T2: a five-second Git-lock deadline could reject a valid pending write | Fixed by waiting for unlock and rechecking identity; real HTTP write regression passed. |
| S3/T3: shared HTTP queue let bridge polling or update checks delay normal sync | Separate bridge/sync queues at the time; external update requests use their own path. Later PR #2 commits replaced this: there is one HTTP stream queue, and the bridge used RequestAsync until #4 gave it its own stream slot. |
| S4/T4: filesystem-parent batching could miss independent project mappings | Coalescing now follows DOM ancestry; overlapping-mapping regression passed. |
| Final extraction recovery delta | Settled paths are rescanned once; repeated live package installs passed. |

Update notifications target `SenorLawyer/rojo`, including the tested prerelease-cache refresh behavior. The repository had no published release during these checks. There is no `rojo update` self-installer; installation from a published release has therefore not been tested end to end. The offline agent guide explains manual versioned installation and preserves earlier versions.
