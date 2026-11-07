# Agent tools over MCP

`rojo mcp` serves a fixed set of inspection, model-export and native simulation tools through standard input and output. It reuses the same native Rojo CLI commands as a terminal workflow. There is no arbitrary command, package installation or place publication tool, and no tool accepts Luau source. `studio_test` and `run_tests` do run the place's existing and synced scripts, so anything those scripts can do happens during a test.

```text
rojo mcp --project C:/Projects/MyGame --address http://127.0.0.1:34872 --token-file %LOCALAPPDATA%/Rojo/studio-token
```

Keep the token in a per-user directory such as `%LOCALAPPDATA%\Rojo` on Windows or `~/.config/rojo` elsewhere. Hosts that do not expand environment variables need the full path.

Start `rojo serve` separately with the same Studio token file and connect the matching plugin. The token contents are read by the native Studio CLI, not sent as MCP arguments or returned in tool results. The address must be a loopback address accepted by the Studio CLI. Workspace, checkpoint and package inspection can also be used without a token file.

A host launches the binary as a subprocess. For a host using a `mcpServers` configuration object, the command entry looks like this:

```json
{
  "mcpServers": {
    "rojo": {
      "command": "C:/Tools/rojo.exe",
      "args": [
        "mcp",
        "--project", "C:/Projects/MyGame",
        "--address", "http://127.0.0.1:34872",
        "--token-file", "C:/Users/<you>/AppData/Local/Rojo/studio-token"
      ]
    }
  }
}
```

Use the host's own configuration format when it differs. Rojo writes only JSON-RPC messages to stdout; ordinary diagnostics use stderr. Messages are UTF-8 JSON objects separated by newlines, following the MCP [stdio transport](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports).

## Tools

| Tool | Native CLI command | Arguments |
| --- | --- | --- |
| `studio_sessions` | `studio sessions --json` | None |
| `studio_status` | `studio status --json` | `session` |
| `studio_test` | `studio test --json` | `session`, optional `seconds` |
| `run_tests` | `test --json` | optional `session`, `roots`, `filter`, `framework`, `module`, `timeout` |
| `studio_tree` | `studio tree --json` | `session`, optional `instance`, `depth`, `limit` |
| `studio_properties` | `studio properties --json` | `session`, `instance`, `properties` |
| `studio_logs` | `studio logs --json` | `session`, optional `limit`, `after` |
| `studio_export` | `studio export --json` | `session`, `instance`, `output` |
| `studio_verify` | `studio verify --json` | `session`, optional `fix`, `includeHeld` |
| `studio_backup` | `studio backup create --json` | `session`, `instance`, optional `name`, `note` |
| `studio_backup_list` | `studio backup list --json` | None |
| `studio_lock_acquire` | `studio lock acquire --json` | `lock`, `owner`, optional `exclusive`, `limit`, `ttl`, `wait` (at most 60), `note` |
| `studio_lock_release` | `studio lock release --json` | `lock`, `owner` |
| `studio_lock_status` | `studio lock status --json` | optional `lock` |
| `checkpoint_list` | `checkpoint list --json` | None |
| `workspace_list` | `workspace list --json` | None |
| `packages_doctor` | `packages doctor --json` | None |

`studio_tree` returns the bridge's ID for each instance, and `studio_properties` reads an instance by that ID and returns the IDs of Instance-valued properties. `studio_backup` and `studio_export` accept such an ID as well as a `game/...` path, and need it when two siblings share a name, since such a path is ambiguous. The instance IDs Roblox's `inspect_instance` returns are engine IDs this bridge does not resolve.

List Studio sessions first and choose the exact UUID for every targeted operation. The adapter never chooses the first available session; `run_tests` without a `session` lets `rojo test` use the only connected session and fails when there are several. It forwards the configured address and token file to each Studio command. Package, workspace and checkpoint tools use the project fixed at server startup; a tool call cannot override it.

`studio_export` creates a native `.rbxm` file at a relative path inside that project. Its parent directory must already exist. Absolute paths, parent traversal and escaping through a parent symlink are rejected. Paths inside a directory mapped by the project's `$path` entries are also rejected, since `rojo serve` would sync the exported model back into Studio. The native export command refuses to overwrite an existing file. This tool exports a model; it does not save or publish the place. Imports remain explicit CLI operations outside this MCP catalog.

`studio_verify` compares the session's scripts with their files, as described in [Verify Studio against the files](sync-health.md#verify-studio-against-the-files). A place that does not match is a successful call with `inSync: false`; `fix` changes scripts in Studio, so the tool is not read-only. `studio_lock_acquire` answers a busy lock with `granted: false` and the current holders rather than a tool error. Lock tools need no Studio connection. See [Studio locks](studio-locks.md).

`studio_logs` returns the newest entries as a list. To tail the log, pass `after: 0` on the first call and the result's `nextAfter` on each later call. Each result then holds only the entries captured since the previous call, oldest first, with `more: true` when more are waiting than `limit` allowed and a `dropped` count for entries lost from the plugin's 200-entry buffer in between. See [Following logs](studio-bridge.md#following-logs) for every field. The MCP tool reads once per call; it does not run `--follow`.

`studio_test` runs a native simulation for `1..5` seconds, then returns to Edit mode. It executes the existing and synced scripts of the explicitly selected trusted place in game context. With Studio API access enabled, those scripts can write DataStores or make HTTP requests. Its annotations set `readOnlyHint: false`, `destructiveHint: true` and `openWorldHint: true` for that reason. It does not accept new script source.

`run_tests` runs the place's `*.spec` modules in Run mode, as described in [Running specs](testing.md), and returns the report as JSON: `status` (`passed`, `failed`, `error` or `timedOut`), counts, and each failure's test path and messages. A run whose tests fail is a successful tool call with `isError: false` and `status: "failed"`; `isError: true` means the run could not be requested at all, such as when no session is connected. It carries the same annotations as `studio_test`, since it executes the same scripts. Only a call with an explicit `session` can be cancelled at the bridge. Roots and the module are `game/...` paths without empty names (anything else is rejected as invalid arguments before Studio is asked), the filter is at most 256 bytes of UTF-8 and the timeout is `1..300` seconds. A report too large for the 256 KiB output limit is cut to fit rather than failing the call: `rojo test` keeps failures and load errors in order while they fit, shortens the messages of the first one that does not, and counts the rest in `omittedFailures`, `omittedLoadErrors` and `truncatedMessages`. The `failed` count still covers every failure.

Unknown arguments and incorrect types are rejected before starting a child command. Tree depth is limited to `0..6`, tree limits to `1..500`, log limits to `1..200` (the most entries the plugin returns), log `after` values to `0..2^53-1`, and property reads to `1..64` ordinary property names. All commands use native argument arrays without a shell. Tool descriptions include JSON Schemas with additional properties disabled.

## Which MCP server to use

Roblox's [Studio MCP server](https://create.roblox.com/docs/studio/mcp-open-cloud) can run next to this one. It reads and edits the place, runs Luau, play tests, inserts and uploads assets, and calls Open Cloud through `open_cloud_invoke` with the scopes the creator has granted for the game. This server is for work that depends on the Rojo project on disk. The initialization `instructions` and the tool descriptions say the same, so an agent connected to both picks the right one.

| `rojo mcp` tool | Closest Roblox Studio MCP tool | Decision |
| --- | --- | --- |
| `studio_verify` | None | Keep: compares Studio with the files and fixes it from them |
| `studio_sessions`, `studio_status` | `list_roblox_studios`, `get_studio_state` | Keep: Rojo sync health and the session UUIDs every `rojo` tool takes |
| `studio_backup`, `studio_backup_list` | None | Keep: backups in the repository, restored with `rojo studio backup restore` |
| `studio_lock_acquire`, `studio_lock_release`, `studio_lock_status` | None | Keep: shared by every worktree; Roblox's tools do not check them |
| `run_tests` | None | Keep: TestEZ or Jest-Lua report |
| `studio_export` | None | Keep: writes `.rbxm` into the project, outside the synced folders |
| `checkpoint_list`, `workspace_list`, `packages_doctor` | None | Keep: project files and Git |
| `studio_test` | `start_stop_play` | Keep, pointing at Roblox's: a bounded run that always returns to Edit; use `start_stop_play` for a play test that keeps running |
| `studio_logs` | `get_console_output` | Keep, pointing at Roblox's: follows the log across calls with `after` |
| `studio_tree` | `search_game_tree` | Keep, pointing at Roblox's: gives the IDs `studio_backup` and `studio_export` accept; browse and search with Roblox's, which filters by class and name and reads Client and Server during a play test |
| `studio_properties` | `inspect_instance` | Keep, pointing at Roblox's: reads by exact ID and returns IDs for Instance-valued properties, to pick one of several same-named siblings; browse properties and attributes with Roblox's |

Every tool is listed. The descriptions of the overlapping tools name Roblox's counterpart and say when to use each.

Open Cloud work is split the same way. `open_cloud_invoke` covers developer products, game passes, game and place settings, analytics, server restarts and one-off uploads. Data stores and memory stores are not Open Cloud calls in Roblox's server: scripts reach them through `execute_luau`, with the matching Save & load data permission and the game's Studio access to API services. Uploads that project code refers to go through `rojo assets sync`, which records them in `assets.lock.json` and the generated module; cloud test runs use `rojo test --cloud`. Places are published from archived checkpoints with `rojo release`: `rojo release stage` publishes to a separate test place, `rojo release promote` then publishes the same bytes live after a recent passing stage, and `rojo release publish` publishes live directly. These remain CLI commands; this server does not call Roblox's server or Open Cloud.

Edits made with Roblox's tools to scripts Rojo syncs are overwritten from the files on the next sync, and `studio_verify` reports them until then. Change the file instead.

## Protocol and limits

The adapter supports `initialize`, `notifications/initialized`, `ping`, `tools/list` and `tools/call`. Initialization negotiates `2025-06-18` or `2024-11-05`. An unrecognized version receives the supported `2025-06-18` version so the client can accept it or disconnect, as described in the MCP [lifecycle specification](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle). Send the initialized notification before calling tools.

Results include text content and `isError`. Studio tool results add a second text item noting that instance names, properties, logs and errors come from Studio and are untrusted data, not instructions. Under `2025-06-18`, native JSON objects also appear as `structuredContent`; older clients receive the same JSON in text content. Invalid JSON, invalid arguments and unknown methods produce JSON-RPC errors. A failure reported by an allowed native command produces a tool result with `isError: true`, following the MCP [tool result format](https://modelcontextprotocol.io/specification/2025-06-18/server/tools).

Request lines are limited to 64 KiB, and blank lines are ignored. Each child output stream is limited to 256 KiB; oversized output returns a tool error and the next request can still run. `run_tests` asks `rojo test` for a report within that limit, so it is not affected. Serialized responses are capped at 2 MiB. Child commands have a 70-second deadline, longer than the 65 seconds `rojo studio` waits for the bridge, which in turn outlasts the server's own bound on a command (60 seconds, including waiting for Git and checking an import). `run_tests` scales the same chain with its timeout: the server allows the timeout plus 75 seconds, `rojo test` waits 5 seconds more and the adapter another 5, so a 300-second run has a 385-second deadline. Up to eight tool calls run concurrently on worker threads, so `ping` and other requests are answered while a Studio command is pending. `notifications/cancelled` stops the matching call's child command, and no response is sent for it. When a Studio command is cancelled or fails for any reason, including a timeout or a lost connection, the adapter also asks the bridge to cancel its request ID, so Studio does not run it after the caller has given up. This holds even when the command is still waiting for Git or its import check: the bridge remembers the cancelled ID for 60 seconds and refuses to queue it. Cancelling a command that already finished does not change its result, but the next command with its request ID within 60 seconds is answered with HTTP 410, since the bridge cannot tell it from a command whose request has not been read yet. This adapter does not advertise subscriptions, resources, prompts or a network MCP transport. Close stdin to stop it; calls already running finish first.

`cargo test --locked --test mcp` exercises an actual stdio process, both negotiated versions, catalog restrictions, a native package-doctor call, malformed/oversized messages, output limits, recovery on the next request, `ping` during a pending Studio call, and cancellation reaching the bridge.
