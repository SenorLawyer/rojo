# Studio locks

When several agents share a few open Roblox Studio windows, they have to take turns. Edit work can happen side by side, but a play test replaces the Studio's Edit datamodel for everyone using that window, and a big job can make a Studio crash when several run at once. `rojo studio lock` gives agents named, expiring leases to coordinate with.

Locks are advisory. Nothing in Studio checks them: Studio MCP tools such as `execute_luau` or `start_stop_play`, the Rojo bridge and Studio itself work whether or not a lock is held. Every agent must acquire the right lock before it uses a Studio, and release it afterwards.

Locks are plain files, so they work across processes and for Studios that are not connected to `rojo serve`. `rojo studio lock` needs no `--token-file`.

## The model

A lock has a name, such as `main` or `build`: 1-64 letters, digits, `_`, `.` or `-`. Names are case-insensitive and stored in lowercase. Use one lock per Studio window or per kind of turn.

Each hold has an owner (any name the agent uses consistently, such as its task name), a mode and an expiry:

* **Shared** (the default) is granted when no one holds the lock exclusively and no exclusive waiter came first. With `--limit N`, at most N shared holders are allowed at once. Pass the same limit everywhere for one lock: each request is checked against its own `--limit`.
* **Exclusive** (`--exclusive`) is granted when no one else holds the lock in any mode and no other exclusive waiter came first.

A hold expires after `--ttl` seconds (default 600, at most 3600) unless renewed. Acquiring again with the same owner and mode renews it from now; renewing always succeeds, even while the lock is paused or others are waiting. Acquiring with the same owner in the other mode switches modes if that mode can be granted. Expired holds are ignored and removed on the next operation, so a crashed agent blocks others for at most its TTL.

### Waiting and fairness

`--wait 0` (the default) tries once. With `--wait SECS`, the command keeps trying about once a second until the lock is granted or the time runs out, and holds a place in line meanwhile. Waiters are served oldest first:

* An exclusive waiter blocks shared requests that arrive after it started waiting, so a stream of shared holders cannot starve it. Current shared holders keep their holds, and can renew them, until they release or expire.
* With `--limit`, shared waiters that came first get free slots before newcomers.

A waiter refreshes its place every poll. A place not refreshed for 30 seconds, for example because the waiting process was killed, is dropped.

### Pausing

`rojo studio lock pause <name> --reason "Rojo down"` stops new grants on that lock until `rojo studio lock resume <name>`. Current holders keep their holds and can renew them. Waiters stay in line.

## Commands

```text
rojo studio lock acquire <name> --owner <who> [--exclusive] [--limit N] [--ttl SECS] [--wait SECS] [--note TEXT]
rojo studio lock release <name> --owner <who>
rojo studio lock status [<name>]
rojo studio lock pause <name> [--reason TEXT]
rojo studio lock resume <name>
rojo studio lock log <name> [-n N]
rojo studio lock reset <name>
```

`acquire` exits with 0 when the lock is granted or renewed and with 3 when it is not, printing why (for example `held by build-agent (exclusive, expires in 212s)`) and who holds and waits for it. Invalid names, owners, TTLs or limits exit with 2, and other failures, such as an unreadable state file, with 1.

`release` removes the owner's hold and place in line. Releasing a lock the owner does not hold succeeds and says so, so cleanup can always run.

`status` lists each lock's holders (owner, mode, when acquired, seconds until expiry, note), waiters in the order they are served, and whether the lock is paused. Without a name it shows every lock in the directory.

`log` prints the last N events (default 20): acquire, renew, release, expire, timeout (a wait that ran out), pause, resume and reset.

Pass `--json` before the subcommand (`rojo studio --json lock status`) for one compact JSON object per command.

## Storage

Inside a Git repository, locks live in `<git common dir>/rojo-state/studio-locks/`, the directory `git rev-parse --git-common-dir` names, so every worktree of a repository shares them. Outside Git they live in `.rojo/studio-locks/` in the current directory. `--dir <path>` uses another directory; give every agent the same one.

Each lock has three files:

* `<name>.json` is its state. It is written to a temporary file and renamed into place, so a killed process never leaves half a file.
* `<name>.json.lock` is locked by the operating system while one command updates the state, and commands wait up to 5 seconds for it. The system releases the lock when that process exits, even if it is killed, so a leftover file never blocks anyone and nothing ever removes another process's lock.
* `<name>.log.jsonl` is the event log, one JSON object per line. Past 256 KiB it is renamed to `<name>.log.jsonl.1`, replacing the older one, so the log stays bounded.

A state file that cannot be read is an error for every command except `reset`, rather than being treated as an empty lock that could be handed to a second holder. Look at it, then run `rojo studio lock reset <name>` to delete it. Reset drops every hold and place in line on that lock, and keeps the old contents in the event log.

## Example: three Studios

A game with one main Studio for shared Edit work, a second Studio for play tests and a third for UI could use four locks:

| Lock | Studio | Used for | Acquire with |
| --- | --- | --- | --- |
| `main` | MAIN | Edit-mode builds, previews, screenshots, everyone at once | `--owner <you>` |
| `heavy` | MAIN | Sim batches, whole-level rebuilds, big serializations, two at a time | `--owner <you> --limit 2 --ttl 240` |
| `build` | BUILD | Play tests, one at a time | `--owner <you> --exclusive --ttl 360` |
| `ui` | UI | UI previews shared; a UI play test exclusive | `--owner <you>`, or `--exclusive --ttl 360` for a play test |

An agent about to play-test waits up to ten minutes for its turn, runs the test and releases:

```text
rojo studio lock acquire build --owner level-3 --exclusive --ttl 360 --wait 600 --note "slingshot play test"
# ... start the play test in the BUILD Studio, read the output, stop it ...
rojo studio lock release build --owner level-3
```

Edit work in MAIN takes the shared lock, which is granted at once unless MAIN is paused:

```text
rojo studio lock acquire main --owner level-3
# ... edit in MAIN ...
rojo studio lock release main --owner level-3
```

A heavy job takes one of two slots and gives it back promptly:

```text
rojo studio lock acquire heavy --owner balance --limit 2 --ttl 240 --wait 300
rojo studio lock release heavy --owner balance
```

If Rojo stops syncing MAIN, an orchestrator pauses it until it is fixed:

```text
rojo studio lock pause main --reason "Rojo down"
rojo studio lock resume main
```

`main` and `heavy` are separate locks, so a heavy job does not stop shared Edit work. To keep Edit work out of MAIN during a heavy job instead, take `main` exclusively for it.

Agents should never hold one lock while waiting for another (for example, holding `heavy` while waiting for `build`), since two agents doing that in opposite order wait for each other until one TTL runs out.
