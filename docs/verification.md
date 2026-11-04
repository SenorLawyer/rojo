# Verification

Tested on Windows on November 4, 2026. The fork starts at upstream commit 84e098e. These results describe disposable fixtures, not a guarantee for every project in the issue tracker.

## CLI and server

The workspace suite passed 264 tests: 14 memofs, 154 library and 96 end-to-end tests. Two pre-existing tests are ignored. Regression coverage includes missing event paths, delayed mapped-directory recreation, explicit init-file paths, interrupted request bodies, historical subscription IDs, metadata changes, binary compression round trips and embedded plugin replacement.

The live watcher test compares every name and Source across 322 scripts after ten real Git branch checkouts, twelve complete package-tree replacements, a deletion/recreation with a 500 ms gap, two-way init-source writing and a server restart. The final run passed every comparison. Checkout convergence after Git returned ranged from about 3 to 289 ms. The twelve replacements settled in 2.29 seconds after mutations stopped. Initial read after restarted server readiness took 4.32 ms; server startup itself took 504 ms. Filesystem errors while a tree is temporarily absent are logged and recovered; no panic occurred.

To repeat on Windows, install Python's msgpack package and run:
```powershell
py scripts/watcher-smoke.py target/debug/rojo.exe
```
The script writes its fixture, server logs and JSON report under a temporary directory. It leaves them for inspection.

Standalone checks also covered all three init templates with lz4/zstd/uncompressed binary output, a BOM project referencing a commented BOM model, byte-array colors, explicit Client run context, cycle rejection, and byte-identical metadata from three fresh syncback processes.

## Studio

The full plugin/package suite passed 98 tests, with no failures and one pre-existing skipped test in an isolated Studio test place.

The actual App and local server were exercised together:
- A stalled handshake failed with a retry message after 15.07 seconds.
- Cancel released the tracked request immediately.
- A duplicate Connect attempt kept the existing session.
- Killing the server cleared the session and displayed a readable error.
- Reconnecting after restart took 116 ms in the fixture and hydrated changed Source.
- A real place-ID change disconnected the session.
- With the GitHub updater deliberately stalled, the native toolbar was created in 11.9 ms and the full App mounted in 14.6 ms.

A cold-font measurement was initially suspected. The suspended coroutine showed the actual remaining mount blocker was the header making a synchronous update request during render. The header now reads cached update information; background checks update that cache.

## Review and remaining scope

GPT-6.1 Sol High reviewed the combined diff independently through sol-loop. Round one found no blocker or should-level defect and one artifact-location nit, which was fixed. The final review and GitHub CI verdict are recorded in the PR.

The [issue inventory](upstream-issues.md) includes all 196 open upstream issues, and the [PR review](upstream-pull-requests.md) includes all 27 open PRs with authors and reviewed heads. Deferred entries are not solved. Cloud upload migration, multiple paths, child patterns, custom metadata names and a third-party plugin API require more design and testing.

This work does not claim live macOS or Linux Studio verification, a fix for every Roblox serialization issue, or production proof on the user's large games. The test place was isolated from those games.
