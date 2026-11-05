# Working on this fork

This is [SenorLawyer/rojo](https://github.com/SenorLawyer/rojo), a public fork of [rojo-rbx/rojo](https://github.com/rojo-rbx/rojo). The CLI and Studio plugin are built together. The original MPL-2.0 [license](LICENSE.txt), copyright notices and Git history are retained. Vendored packages keep their own licenses.

File fork bugs and proposals in [this issue tracker](https://github.com/SenorLawyer/rojo/issues). Send pull requests to this repository's `master` branch, or to `release/7.7` for fixes to the 7.7 line, whose releases are tagged on that branch. Link overlapping upstream work and credit its authors. See the [build and contribution guide](CONTRIBUTING.md), the [issue inventory](docs/upstream-issues.md) and the [PR inventory](docs/upstream-pull-requests.md).

For a local build, clone with `--recurse-submodules`, then run `rokit install` and `cargo build --locked`. Run `cargo test --workspace --locked` and `bash scripts/unit-test-plugin.sh` (requires Studio). Use the built CLI at `target/debug/rojo` or `target/debug/rojo.exe`. To try the matching Studio plugin, close Studio, run that binary with `plugin install`, disable any duplicate Rojo plugin and reopen Studio. If Studio discovery fails or you want to inspect the model before installing, use `plugin install --plugins-dir <folder>`. The CLI prints the file it wrote. Use `serve` from the same binary. A published upstream release or the Rojo pinned in rokit.toml does not include this fork's changes.

The default branch requires a pull request, passing CI and resolved review threads. Squash merge is enabled. External contributions do not need a second maintainer account to open a PR. Report security issues [privately](.github/SECURITY.md).

To profile initial sync or file churn, build with `cargo build --release --locked --features profile-with-tracy`. Run the resulting binary with `serve` and attach the [Tracy UI](https://github.com/wolfpld/tracy) before reproducing. Match the UI version to the locked tracy-client-sys dependency using the [client version table](https://docs.rs/tracy-client/latest/tracy_client/#version-support-table). Based on [upstream PR 1282](https://github.com/rojo-rbx/rojo/pull/1282).

For generated projects, use `rojo build -o place.rbxl` for a place or `rojo build -o model.rbxm` for a model. Binary output supports `--compress lz4`, `--compress zstd` and `--compress none`. XML output rejects compression flags.

Windows x64 is the primary supported target. Windows ARM is excluded from CI and release artifacts. Linux and macOS retain their existing CI coverage.

## Compatibility

Use this fork's Studio plugin (7.7.2-fork.2 or later) with this fork's server at the same version. Encoded HTTP transfers, which keep binary responses intact in Studio, need this fork's server at 7.7.2-fork.2 or later. The plugin detects servers that don't encode responses and uses plain HTTP requests for them, so it still connects to upstream and older servers, with the issues listed below.

Mixing the upstream plugin with this fork's server has these known issues:

- When the target of a `Rojo_Target_*` attribute is removed, the server clears the referencing property by sending a null reference. The upstream plugin treats it as a failed change. With sync fallback enabled, it replaces the referencing instance. Otherwise it logs the change as unapplied. Before this fork, the upstream plugin kept the stale reference without reporting it.
- When a plugin falls further behind than the server's change history, the server closes the socket with code 1012 and the reason "Subscription history is stale; reconnect to resync". The upstream plugin reports this as "WebSocket connection closed unexpectedly". Reconnect to resync.
- The upstream plugin does not send a session ID when it subscribes. After the server restarts, a subscription from an older session is only rejected when its message cursor is beyond the new server's queue. The server then closes the socket with code 1012, which the upstream plugin reports as "WebSocket connection closed unexpectedly" instead of waiting without syncing. Otherwise the server accepts it, and the upstream plugin then ignores the packets because they come from a different session. This also happens with upstream Rojo. Reconnect after restarting the server.

Mixing this fork's plugin with an upstream server has these known issues:

- The upstream server does not clear properties that point at a removed `Rojo_Target_*` target, so Studio keeps the stale reference.
- When any requested instance was removed in the meantime, the upstream server rejects the whole `/api/serialize` request with a 400 instead of listing the missing IDs. The sync fallback then replaces none of the instances in that request.
- The upstream server ignores the `sessionId` query parameter on `/api/socket` and does not check the message cursor, so it accepts a socket opened against a restarted server. The plugin only notices the restart when the first message from the new server arrives, and then ends the session. Reconnect after restarting the server.
