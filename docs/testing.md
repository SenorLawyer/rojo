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

By default the specs are searched for in ReplicatedFirst, ReplicatedStorage, ServerScriptService, ServerStorage, StarterGui, StarterPack and StarterPlayer. Specs inside `_Index` (Wally) and `.pesde` folders belong to dependencies and are skipped, as is anything inside the TestEZ module. Roblox's default `PlayerModule` under StarterPlayerScripts is skipped too: Roblox inserts it at run time and its internal specs do not load outside Roblox's own tests. It is recognized by name, so a project that ships its own fork of `PlayerModule` there has that fork's specs skipped as well; pass a `--root` that points at the `PlayerModule` or inside it to run them.

| Option | Meaning |
| --- | --- |
| `--root <path>` | Search only beneath this instance, such as `game/ReplicatedStorage/Shared`. Repeat for several roots, up to 32. Paths start with `game`, have no empty names (so no trailing or doubled `/`), are at most 2048 bytes and use the bridge's syntax: `/` between names and `\` before a `/` or `\` inside a name. |
| `--filter <pattern>` | Run only tests whose full name (the describe phrases and the test's own phrase, joined by spaces) matches this Lua pattern, at most 256 bytes. TestEZ marks the rest as skipped. |
| `--framework auto\|testez\|jest` | Which framework to run. `auto`, the default, uses TestEZ when the place has it, otherwise Jest. |
| `--module <path>` | The framework's ModuleScript, as a `game/...` path like `--root`. When omitted, the shallowest ModuleScript named `TestEZ` (or `Jest`) in ReplicatedStorage, ServerStorage or ServerScriptService is used. |
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
| 2 | `rojo test` could not reach Studio or was used incorrectly: the bridge is not running or refused the token, no session or several sessions are connected without `--session`, or an option is invalid, such as a root that does not start with `game/` or has an empty name like `game/`, more than 32 roots or a filter longer than 256 bytes. A root that does not exist is only found in Studio, so it exits with 1. |

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
| `truncatedMessages` | Messages shortened or left out. Each message is cut to 4 KiB, a failure keeps 8 messages, and a report carries at most 2 MiB of text: messages, test paths and module paths together. A failure or load error whose path no longer fits is left out and counted above. |
| `error` | Why the run could not finish, for `error` and `timedOut`. |
| `durationSeconds` | Time spent running the specs inside Studio. |
| `framework`, `sessionId`, `requestId` | What ran, and where. |

## How it runs

The bridge's `spec` action runs the specs in Studio's Run mode, the same way the bridge's `test` action does: the plugin inserts a temporary Script into ServerScriptService and calls `StudioTestService:ExecuteRunModeAsync`. The script's source is fixed by the plugin. It copies the plugin's `SpecRunner` module beside it, reads the options from `GetTestArgs()`, runs the specs and returns the report through `EndTest`. Only options, such as paths and a filter, are sent from the command line; no Luau source is. The runner is removed again when Run mode ends, and its insertion and removal share one undo step.

The plugin checks the options, the roots and the framework in Edit mode first, so a wrong path or a missing TestEZ fails without starting Run mode. TestEZ itself requires every spec before running any, and one that fails to load stops the run, so the runner requires each spec separately and gives the rest to TestEZ's planner and runner.

While the specs run, Studio cannot answer other bridge commands. They wait until Run mode ends and may time out. A second run requested meanwhile waits too; if less than its `--timeout` plus 10 seconds of its deadline is left once Studio gets to it, it fails without entering Run mode and asks you to run the specs again. A spec that yields for longer than `--timeout` ends the run with `timedOut`. A spec that never yields cannot be interrupted, but Roblox stops it once it exhausts the script execution timeout. Cancelling a run that Studio has started discards its result but does not stop Run mode.

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

Hosted runners without Studio can use [`rojo test --cloud`](#cloud) instead.

## Cloud

`rojo test --cloud` runs the specs without Studio, through Open Cloud's [Luau Execution](https://create.roblox.com/docs/cloud/reference/features/luau-execution) API. It suits hosted CI runners.

```text
rojo test --cloud --universe-id <universe> --place-id <place> [PROJECT]
```

It does three things:

1. Builds the project (by default the one in the current directory) into a place in memory. The project's root must be a `DataModel`.
2. Saves the place as a new **Saved** version of `--place-id`. It is never published: players keep joining the current published version. Rojo prints the version number it saved. Studio, however, opens a place's latest saved version, so whoever opens the place in Studio next gets the test build, and publishing from Studio then would publish it. This is another reason to use a place set aside for testing.
3. Runs the same spec runner as Studio mode as a Luau Execution task against exactly that version, and prints the report and exits as Studio mode does.

Use a place set aside for testing, since every run adds a version to its history. Both `--universe-id` and `--place-id` are required; Rojo never picks a place on its own.

| Option | Meaning |
| --- | --- |
| `--universe-id <id>`, `--place-id <id>` | The experience and the place to save to and run in. Required. |
| `PROJECT` | The place project to build. Defaults to the current directory. |
| `--api-key-env <var>` | The environment variable holding the Open Cloud API key. Defaults to `ROBLOX_API_KEY`. The key is never printed. |
| `--no-upload` | Save nothing and run against the version Open Cloud picks when none is named, which Roblox documents as the place's current published version. This tests what was uploaded before, not your local files. Rojo prints the version the task ran on. |
| `--root`, `--filter`, `--framework`, `--module`, `--timeout`, `--json` | As in Studio mode. |

The API key needs the `universe-places:write` scope (not needed with `--no-upload`) and the `universe.place.luau-execution-session:read` and `:write` scopes, for the experience you test in.

Progress (the build, the saved version, the Luau task's path) goes to stderr, so `--json` output on stdout stays one object. In human mode, messages the task logged as errors are printed before the report. With `--json`, the object also has `universeId`, `placeId`, `placeVersion` (the version the task ran on), `uploaded`, `taskPath`, `logs` (every `{ "message", "kind" }` the task logged, where `kind` is `OUTPUT`, `INFO`, `WARNING` or `ERROR`) and `logsTruncated`.

The exit codes are those of Studio mode. A task that fails (for example a script error), is cancelled, or runs out of time is reported with status `error` or `timedOut` and exits with 1. A missing ID or API key, a refused save or a request that Open Cloud refuses exits with 2.

### How a cloud run differs from Studio

Luau Execution starts a server, loads the place and runs the script there. According to Roblox's documentation:

- The place's own Server and Local scripts do not run automatically. Specs only see what they set up themselves (ModuleScripts can still be required).
- Physics simulation does not run.
- Changes to the DataModel stay inside the task and are not saved.
- Engine APIs that reach the experience's cloud data, such as DataStores, are available and Roblox advises caution with them: specs should not write to real data.
- A task runs for at most five minutes, and a place allows at most ten unfinished tasks. Task creation is limited to five per minute per API key owner. Roblox keeps up to 450 KB of logs per task.

No player joins the task's server, so specs that need `Players`, characters or client scripts do not behave as they do in Studio's Run mode. Use Studio mode for those.

The task's timeout is `--timeout` plus 30 seconds, at most five minutes, so the runner's own timeout reports a slow spec first. With `--timeout` above 270, the task's limit can end the run first, which is reported as `timedOut` without a list of failures.

While the place is open in Team Create, Roblox refuses to save new versions (HTTP 409). Close it in Studio first, or use `--no-upload`.

### GitHub Actions

Store the API key as a repository secret, here `ROBLOX_API_KEY`, and the IDs as repository variables:

```yaml
name: Tests
on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    # Secrets are not given to workflows run from forks.
    if: github.event_name == 'push' || github.event.pull_request.head.repo.full_name == github.repository
    # One run at a time keeps within Luau Execution's limits.
    concurrency: roblox-cloud-tests
    steps:
      - uses: actions/checkout@v4
      - uses: CompeyDev/setup-rokit@v0.1.2
      - name: Run specs
        env:
          ROBLOX_API_KEY: ${{ secrets.ROBLOX_API_KEY }}
        run: >
          rojo test --cloud
          --universe-id ${{ vars.TEST_UNIVERSE_ID }}
          --place-id ${{ vars.TEST_PLACE_ID }}
          --json > test-report.json
      - if: always()
        uses: actions/upload-artifact@v4
        with:
          name: test-report
          path: test-report.json
```

This assumes Rojo is installed with Rokit from your `rokit.toml`. Install your packages (for example with `wally install`) before running the specs if the project maps them in.

## For agents

`rojo mcp` offers the same run as the `run_tests` tool. See [MCP](mcp.md#tools).
