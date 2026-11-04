# Semantic Roblox file merges

`rojo merge` adapts [rbx-merge](https://github.com/kennethloeffler/rbx-merge) for `.rbxm`, `.rbxl`, `.rbxmx`, and `.rbxlx` files. It merges instance trees and typed properties. Filesystem scripts already tracked as text continue to use Git normally.

## Install the engine

```powershell
rojo merge install-engine
```

This explicit command requires Cargo and Git. It builds upstream revision `d6375bde1bdc1f08437324ebfbd6bfcc655e90d0`, with its locked dependencies, into Rojo's private user cache. Rojo records the revision, version, and executable digest and checks the receipt before using that installation. The receipt lives in the same writable installation folder as the executable, so its BLAKE3 digest only detects accidental corruption or a partial install; it does not protect against someone who can write to that folder. Git merge drivers run the engine directly and skip the receipt check. No merge engine is downloaded during ordinary Rojo commands.

The default Windows root is `%LOCALAPPDATA%/Rojo/rbx-merge/<revision>`. Other platforms use `$XDG_CACHE_HOME/Rojo/rbx-merge/<revision>`, or `$HOME/.cache/Rojo/rbx-merge/<revision>`.

For a custom installation root, use `rojo merge install-engine --root <directory>`, then select its `bin/rbx-merge` executable with `--engine`. An explicitly selected executable, or the `ROJO_RBX_MERGE` environment variable, is a trusted override that must report version `0.2.0`; it does not receive revision or receipt verification.

## Inspect and merge files

```powershell
rojo merge textconv "asset.rbxm"
rojo merge diff "before.rbxm" "after.rbxm"
rojo merge run --base "base.rbxm" --ours "ours.rbxm" --theirs "theirs.rbxm" --out "merged.rbxm"
```

`diff` prints both semantic representations with file headers. It does not produce a line-by-line patch. `--all-properties` includes class defaults and volatile UniqueIds that the default text view suppresses.

Use matching input and output formats. Extensions are matched in any capitalization, such as `.RBXL`. When Git supplies temporary paths without extensions, pass `--path asset.rbxm` as the original format hint. `--out` may be the same file as `--ours`.

Rojo writes merged output to a temporary file beside the destination, validates its Roblox encoding, checks that the destination did not change during the merge, and then publishes the result atomically. On Unix, the result keeps the replaced file's permissions, and a new file gets mode 0644. The engine runs in Rojo's console, so Ctrl+C stops both. A temporary file left by a process that was killed is named `.rojo-merge-*.tmp` and can be deleted. Conflicts, invalid engine output, or engine failures preserve the destination. Symbolic-link destinations are rejected. Existing conflict reports and stash directories are preserved; choose new artifact paths for another attempt.

## Resolve a conflict explicitly

```powershell
rojo merge run --base base.rbxmx --ours ours.rbxmx --theirs theirs.rbxmx --out merged.rbxmx --stash-dir "conflict-state"
# Edit conflict-state/conflicts.txt, choosing ours, theirs, or base per block.
rojo merge resolve --stash-dir "conflict-state" --out merged.rbxmx
```

Alternatively, save a report with `--conflicts-out conflicts.txt`, edit its choices, and repeat the original command with `--resolutions conflicts.txt`. `--take ours|theirs|base` explicitly chooses a side for every remaining conflict. It resolves conflicts inside the semantic merge; it does not copy that whole input file. For `RefTarget` conflicts, any resolution drops the dangling reference. For `UniqueIdCollision`, any resolution drops the duplicate ID. Review those changes in the output.

## Activate Git for a clone

```powershell
rojo merge install --write-gitattributes
rojo merge doctor
```

Installation uses upstream's local Git configuration and `.git/info/attributes` override. Git resolves shared paths itself, so linked worktrees use the clone's common configuration. Repeat installation per clone. Rojo exposes no global installation option.

`--write-gitattributes` also adds a managed block marking Roblox files `binary`. Commit that file so collaborators who have not installed the driver get conflicts instead of unsafe XML line merges. Installation does not commit anything. Existing surrounding attribute rules are preserved.

After a conflicting Git merge, the driver saves inputs and a report under `.rbxmerge/<asset-path>/`. Edit its `conflicts.txt`, run `rojo merge resolve --stash-dir <directory> --out <asset-path>`, inspect the result, then stage it and continue the Git operation. `rojo merge uninstall` removes local activation while retaining committed binary defaults. Git invokes the upstream engine directly; the adapter's extra output validation and concurrent-destination checks apply to explicit `rojo merge run` and `resolve` commands.

## Scope and preservation limits

The engine combines independent property edits, attributes, additions, deletions, moves, sibling order, and internal references. Conflicting edits to the same `Source` property require a choice; the engine does not merge Luau source lines.

Stable UniqueIds provide the strongest identity. Instances without usable IDs rely on structure, names, similarity, and sometimes sibling position. Review positional or ambiguous identity diagnostics, particularly after Studio regenerates IDs or reorders duplicate siblings.

This is semantic preservation, not a byte-for-byte file copier. WeakDom does not retain every Roblox file-level metadata field. Unknown XML properties are read and written; binary properties survive when the selected codecs can represent them. The pinned engine has its own older reflection database. Keep the original inputs and inspect diagnostics and merged output before publishing a place.

The backend is [MIT licensed](https://github.com/kennethloeffler/rbx-merge/blob/d6375bde1bdc1f08437324ebfbd6bfcc655e90d0/rbx_merge/LICENSE); the separate CLI is [Apache-2.0 licensed](https://github.com/kennethloeffler/rbx-merge/blob/d6375bde1bdc1f08437324ebfbd6bfcc655e90d0/rbx_merge_cli/LICENSE). Rojo does not bundle either crate or binary. Keeping the engine separate avoids adding its six pinned Git DOM dependencies and separate CLI dependency graph to Rojo.

## Verification

At the pinned revision, the upstream Windows suite passed 64 backend tests, including property-based invariants, 10 setup tests, and 13 CLI integration tests. Rojo's opt-in integration suite exercises the real executable, all four formats, preservation of references and script contents, property/move/delete/reference conflicts, edited-report resolution, linked worktrees, and quoted Git paths:

```powershell
$env:ROJO_TEST_RBX_MERGE = "C:/path/to/pinned/rbx-merge.exe"
cargo test --locked --test merge_backend -- --ignored
```
