# Verification

Tested on Windows on November 4, 2026. The fork starts at upstream commit 84e098e. These results describe disposable fixtures, not a guarantee for every project in the issue tracker.

## CLI and server

At commit d295daa, `cargo test --workspace --locked` on Windows passed 322 tests: 36 memofs, 185 library and 101 end-to-end tests. Two tests are ignored. Regression coverage includes missing event paths, delayed mapped-directory recreation, explicit init-file paths, interrupted request bodies, historical subscription IDs, metadata changes, binary compression round trips and embedded plugin replacement.

The live watcher test compares every name and Source across 322 scripts after ten real Git branch checkouts, twelve complete package-tree replacements, a deletion/recreation with a 500 ms gap, two-way init-source writing and a server restart. The optimized build passed every comparison. Tests cover both rapid replacement and recreation after the deletion has settled. Timing depends on filesystem event batching and is not a production-project latency guarantee. Filesystem errors while a tree is temporarily absent are logged and recovered; no panic occurred.

To repeat on Windows, install Python's msgpack package and run:
```powershell
py scripts/watcher-smoke.py target/debug/rojo.exe
```
The script writes its fixture, server logs and JSON report under a temporary directory. It leaves them for inspection.

Standalone checks also covered all three init templates with lz4/zstd/uncompressed binary output, a BOM project referencing a commented BOM model, byte-array colors, explicit Client run context, cycle rejection, and byte-identical metadata from three fresh syncback processes.

## Studio

At commit d295daa, the full plugin/package suite passed 131 tests, with no failures and one skipped test, in an isolated Studio test place on Windows.

The actual App and local server were exercised together:
- After a request to the host had completed in the Studio session, a stalled handshake failed with a retry message after 15.07 seconds. Until a request to the host completes in a session, a stalled handshake waits up to two more minutes, even when access was allowed in an earlier session, because the plugin cannot tell it apart from an unanswered permission prompt.
- Cancel released the tracked request immediately.
- A duplicate Connect attempt kept the existing session.
- Killing the server cleared the session and displayed a readable error.
- Reconnecting after restart took 116 ms in the fixture and hydrated changed Source.
- A real place-ID change disconnected the session.
- With the GitHub updater deliberately stalled, the native toolbar was created in 11.9 ms and the full App mounted in 14.6 ms.

The header reads cached update information during render, and background checks update that cache, so a stalled update request cannot hold up mounting.

## Remaining scope

Regression tests cover coalesced directory events, a race that only showed up in optimized stress runs. Windows CI runs the release-profile suite and Git/package stress script; other platform failures cannot cancel that job.

The [issue inventory](upstream-issues.md) includes all 196 open upstream issues, and the [PR review](upstream-pull-requests.md) includes all 27 open PRs with authors and reviewed heads. Deferred entries are not solved. Cloud upload migration, multiple paths, child patterns, custom metadata names and a third-party plugin API require more design and testing.

This work does not claim live macOS or Linux Studio verification, a fix for every Roblox serialization issue, or production proof on large games. The test place was isolated from production games.
