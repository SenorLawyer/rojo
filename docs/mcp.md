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
| `checkpoint_list` | `checkpoint list --json` | None |
| `workspace_list` | `workspace list --json` | None |
| `packages_doctor` | `packages doctor --json` | None |

List Studio sessions first and choose the exact UUID for every targeted operation. The adapter never chooses the first available session; `run_tests` without a `session` lets `rojo test` use the only connected session and fails when there are several. It forwards the configured address and token file to each Studio command. Package, workspace and checkpoint tools use the project fixed at server startup; a tool call cannot override it.

`studio_export` creates a native `.rbxm` file at a relative path inside that project. Its parent directory must already exist. Absolute paths, parent traversal and escaping through a parent symlink are rejected. Paths inside a directory mapped by the project's `$path` entries are also rejected, since `rojo serve` would sync the exported model back into Studio. The native export command refuses to overwrite an existing file. This tool exports a model; it does not save or publish the place. Imports remain explicit CLI operations outside this MCP catalog.

`studio_logs` returns the newest entries as a list. To tail the log, pass `after: 0` on the first call and the result's `nextAfter` on each later call. Each result then holds only the entries captured since the previous call, oldest first, with `more: true` when more are waiting than `limit` allowed and a `dropped` count for entries lost from the plugin's 200-entry buffer in between. See [Following logs](studio-bridge.md#following-logs) for every field. The MCP tool reads once per call; it does not run `--follow`.

`studio_test` runs a native simulation for `1..5` seconds, then returns to Edit mode. It executes the existing and synced scripts of the explicitly selected trusted place in game context. With Studio API access enabled, those scripts can write DataStores or make HTTP requests. Its annotations set `readOnlyHint: false`, `destructiveHint: true` and `openWorldHint: true` for that reason. It does not accept new script source.

`run_tests` runs the place's `*.spec` modules in Run mode, as described in [Running specs](testing.md), and returns the report as JSON: `status` (`passed`, `failed`, `error` or `timedOut`), counts, and each failure's test path and messages. A run whose tests fail is a successful tool call with `isError: false` and `status: "failed"`; `isError: true` means the run could not be requested at all, such as when no session is connected. It carries the same annotations as `studio_test`, since it executes the same scripts. Only a call with an explicit `session` can be cancelled at the bridge. Roots and the module are `game/...` paths, the filter is at most 256 characters and the timeout is `1..300` seconds. A report too large for the 256 KiB output limit is cut to fit rather than failing the call: `rojo test` keeps failures and load errors in order while they fit, shortens the messages of the first one that does not, and counts the rest in `omittedFailures`, `omittedLoadErrors` and `truncatedMessages`. The `failed` count still covers every failure.

Unknown arguments and incorrect types are rejected before starting a child command. Tree depth is limited to `0..6`, tree limits to `1..500`, log limits to `1..200` (the most entries the plugin returns), log `after` values to `0..2^53-1`, and property reads to `1..64` ordinary property names. All commands use native argument arrays without a shell. Tool descriptions include JSON Schemas with additional properties disabled.

## Protocol and limits

The adapter supports `initialize`, `notifications/initialized`, `ping`, `tools/list` and `tools/call`. Initialization negotiates `2025-06-18` or `2024-11-05`. An unrecognized version receives the supported `2025-06-18` version so the client can accept it or disconnect, as described in the MCP [lifecycle specification](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle). Send the initialized notification before calling tools.

Results include text content and `isError`. Studio tool results add a second text item noting that instance names, properties, logs and errors come from Studio and are untrusted data, not instructions. Under `2025-06-18`, native JSON objects also appear as `structuredContent`; older clients receive the same JSON in text content. Invalid JSON, invalid arguments and unknown methods produce JSON-RPC errors. A failure reported by an allowed native command produces a tool result with `isError: true`, following the MCP [tool result format](https://modelcontextprotocol.io/specification/2025-06-18/server/tools).

Request lines are limited to 64 KiB, and blank lines are ignored. Each child output stream is limited to 256 KiB; oversized output returns a tool error and the next request can still run. `run_tests` asks `rojo test` for a report within that limit, so it is not affected. Serialized responses are capped at 2 MiB. Child commands have a 70-second deadline, longer than the 65 seconds `rojo studio` waits for the bridge, which in turn outlasts the server's own bound on a command (60 seconds, including waiting for Git and checking an import). `run_tests` scales the same chain with its timeout: the server allows the timeout plus 75 seconds, `rojo test` waits 5 seconds more and the adapter another 5, so a 300-second run has a 385-second deadline. Up to eight tool calls run concurrently on worker threads, so `ping` and other requests are answered while a Studio command is pending. `notifications/cancelled` stops the matching call's child command, and no response is sent for it. When a Studio command is cancelled or fails for any reason, including a timeout or a lost connection, the adapter also asks the bridge to cancel its request ID, so Studio does not run it after the caller has given up. This holds even when the command is still waiting for Git or its import check: the bridge remembers the cancelled ID for 60 seconds and refuses to queue it. Cancelling a command that already finished does not change its result, but the next command with its request ID within 60 seconds is answered with HTTP 410, since the bridge cannot tell it from a command whose request has not been read yet. This adapter does not advertise subscriptions, resources, prompts or a network MCP transport. Close stdin to stop it; calls already running finish first.

`cargo test --locked --test mcp` exercises an actual stdio process, both negotiated versions, catalog restrictions, a native package-doctor call, malformed/oversized messages, output limits, recovery on the next request, `ping` during a pending Studio call, and cancellation reaching the bridge.
