# Running specs with `rojo test`

`rojo test` runs a place's [TestEZ](https://github.com/Roblox/testez) specs in Roblox Studio and prints which tests passed and failed, so you do not have to press Play and read the output window. It works through the [local Studio bridge](studio-bridge.md): Studio must be open with the place connected to `rojo serve`.

Jest-Lua is also supported, with the caveats under [Jest-Lua](#jest-lua).

## Set up the place

1. Add TestEZ as a dependency. With Wally, add it to `[dev-dependencies]` (or `[dependencies]`):

   ```toml
   [dev-dependencies]
   TestEZ = "roblox/testez@0.4.1"
   ```

   With pesde, add the equivalent package to `dev_dependencies`. Map the installed folder (`DevPackages`, `Packages` or `roblox_packages`) into ReplicatedStorage, ServerStorage or ServerScriptService in your project file, so that a ModuleScript named `TestEZ` ends up in the place.

2. Name each spec module `<Name>.spec.lua` (or `.luau`). Rojo syncs it as a ModuleScript named `<Name>.spec`. A spec returns a function that calls `describe` and `it`:

   ```lua
   return function()
   	local Math = require(script.Parent.Math)

   	describe("add", function()
   		it("adds two numbers", function()
   			expect(Math.add(1, 2)).to.equal(3)
   		end)
   	end)
   end
   ```

   An `init.spec.lua` inside a folder is reported under the folder's name, as TestEZ does.

3. Create a Studio token and serve the project with it, then connect the Rojo plugin in Studio:

   ```text
   rojo studio token --output <private-token-file>
   rojo serve --studio-token-file <private-token-file>
   ```

   See [the bridge's documentation](studio-bridge.md#connect) for where to keep the token.

## Run the specs

```text
rojo test --token-file <private-token-file>
```

With one Studio session connected, `rojo test` uses it. With several, it lists them and asks for `--session <id>`.

By default the specs are searched for in ReplicatedFirst, ReplicatedStorage, ServerScriptService, ServerStorage, StarterGui, StarterPack and StarterPlayer. Specs inside `_Index` (Wally) and `.pesde` folders belong to dependencies and are skipped, as is anything inside the TestEZ module.

| Option | Meaning |
| --- | --- |
| `--root <path>` | Search only beneath this instance, such as `game/ReplicatedStorage/Shared`. Repeat for several roots, up to 32. Paths use the bridge's syntax: `/` between names and `\` before a `/` or `\` inside a name. |
| `--filter <pattern>` | Run only tests whose full name (the describe phrases and the test's own phrase, joined by spaces) matches this Lua pattern. TestEZ marks the rest as skipped. |
| `--framework auto\|testez\|jest` | Which framework to run. `auto`, the default, uses TestEZ when the place has it, otherwise Jest. |
| `--module <path>` | The framework's ModuleScript. When omitted, the shallowest ModuleScript named `TestEZ` (or `Jest`) in ReplicatedStorage, ServerStorage or ServerScriptService is used. |
| `--timeout <seconds>` | How long the specs may run, 1 to 300 seconds. The default is 60. |
| `--session <id>` | The Studio session, from `rojo studio sessions`. |
| `--json` | Print the report as JSON. |
| `--request-id <uuid>` | Name the request so `rojo studio cancel` can cancel it before Studio starts it. |
| `--address`, `--token-file` | As for `rojo studio`. |

Output lists every failure with its path and messages, then a summary:

```text
FAIL ReplicatedStorage > Shared > Math > add > adds two numbers
    ReplicatedStorage.Shared.Math.spec:6: Expected value "4" (number), got "3" (number) instead

FAILED: 11 passed, 1 failed, 0 skipped in 0.42s (testez, 3 spec modules)
```

A spec module that fails to load (a syntax error, or an error while it is required) is reported as `LOAD ERROR <path>` and fails the run, while the other modules still run. A `describe` block that throws while TestEZ plans it counts as a failure as well, which TestEZ's own summary leaves out.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Every test passed. |
| 1 | A test failed, a module failed to load, or the run could not finish: no TestEZ was found, no specs were found, the specs did not finish within `--timeout`, Studio was not in Edit mode, or Studio did not answer in time. |
| 2 | `rojo test` could not reach Studio or was used incorrectly: the bridge is not running or refused the token, no session or several sessions are connected without `--session`, or an option is invalid. |

### JSON report

`--json` prints one object:

| Field | Meaning |
| --- | --- |
| `status` | `passed`, `failed`, `error` (the run could not finish) or `timedOut`. |
| `passed`, `failed`, `skipped` | Test counts. `failed` includes failures that are not listed. |
| `modules` | Spec modules that loaded (TestEZ) or test files (Jest). |
| `failures` | Up to 500 entries of `{ "path": [...phrases], "messages": [...] }`. |
| `loadErrors` | Up to 100 entries of `{ "module", "message" }`. |
| `omittedFailures`, `omittedLoadErrors` | Entries left out past those limits. |
| `truncatedMessages` | Messages shortened or left out. Each message is cut to 4 KiB, a failure keeps 8 messages, and a report carries at most 2 MiB of message text. |
| `error` | Why the run could not finish, for `error` and `timedOut`. |
| `durationSeconds` | Time spent running the specs inside Studio. |
| `framework`, `sessionId`, `requestId` | What ran, and where. |

## How it runs

The bridge's `spec` action runs the specs in Studio's Run mode, the same way the bridge's `test` action does: the plugin inserts a temporary Script into ServerScriptService and calls `StudioTestService:ExecuteRunModeAsync`. The script's source is fixed by the plugin. It copies the plugin's `SpecRunner` module beside it, reads the options from `GetTestArgs()`, runs the specs and returns the report through `EndTest`. Only options, such as paths and a filter, are sent from the command line; no Luau source is. The runner is removed again when Run mode ends, and its insertion and removal share one undo step.

The plugin checks the options, the roots and the framework in Edit mode first, so a wrong path or a missing TestEZ fails without starting Run mode. TestEZ itself requires every spec before running any, and one that fails to load stops the run, so the runner requires each spec separately and gives the rest to TestEZ's planner and runner.

While the specs run, Studio cannot answer other bridge commands. They wait until Run mode ends and may time out. A spec that yields for longer than `--timeout` ends the run with `timedOut`. A spec that never yields cannot be interrupted, but Roblox stops it once it exhausts the script execution timeout. Cancelling a run that Studio has started discards its result but does not stop Run mode.

The specs run with the place's other scripts, in a server context. Anything those scripts do during a normal Run session, such as DataStore or HTTP requests when Studio API access is enabled, happens during a test run too.

## Jest-Lua

With `--framework jest`, or with `auto` when the place has no TestEZ, the runner calls `runCLI` from the ModuleScript named `Jest`, with every folder beneath the roots that contains a `jest.config` ModuleScript as a project. `--filter` becomes Jest's `testNamePattern`, a regular expression. Jest-Lua needs Studio's `FFlagEnableLoadModule` flag turned on, as described in its documentation. Jest support has only been checked against a sample of Jest's result format, not yet in Studio.

## Continuous integration

`rojo test` needs a running Roblox Studio with the place open and connected, so it suits a developer machine or a self-hosted runner where Studio is logged in. A typical script:

```text
rojo serve --studio-token-file <token> &
# Open the place in Studio and connect the Rojo plugin, then:
rojo test --token-file <token> --json > test-report.json
```

Hosted runners without Studio need a different runner, such as Open Cloud's Luau Execution. The runner module and the report format do not depend on Studio, so they can be shared with one.

## For agents

`rojo mcp` offers the same run as the `run_tests` tool. See [MCP](mcp.md#tools).
