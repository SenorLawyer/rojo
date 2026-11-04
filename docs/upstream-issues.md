# Upstream issue inventory

196 open upstream issues on November 4, 2026, against base 84e098e. Related symptoms are not proof of an individual fix. See the [PR inventory](upstream-pull-requests.md).

| Issue | Report | Disposition |
|---|---|---|
| [1356](https://github.com/rojo-rbx/rojo/issues/1356) | Wrong type of value for property Part.Color3uint8. Expected Color3uint8, got an array of three numbers | Implemented and tested: validated Color3uint8 byte arrays. |
| [1354](https://github.com/rojo-rbx/rojo/issues/1354) | Folder-based scripts (init.client.luau) ignore $ignoreUnknownInstances and wipe out Studio-side child nodes | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1349](https://github.com/rojo-rbx/rojo/issues/1349) | Rojo Crashing | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [1348](https://github.com/rojo-rbx/rojo/issues/1348) | Crash Bug - VSCode Rojo Roblox Studio Plugin | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [1347](https://github.com/rojo-rbx/rojo/issues/1347) | After building, the terrain - exported from the Studio with "ExpandedTerrain" enabled - disappears. | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [1346](https://github.com/rojo-rbx/rojo/issues/1346) | crash: random crash in rojo happens often at random times | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [1343](https://github.com/rojo-rbx/rojo/issues/1343) | Scaling issue with Rojo but not in Studio | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [1342](https://github.com/rojo-rbx/rojo/issues/1342) | ContentId: rbxassetid://203785492 failed due to: Asset type does not match requested type  -  Studio | Already addressed by upstream PR 1351. |
| [1340](https://github.com/rojo-rbx/rojo/issues/1340) | The system cannot find the file specified. (os error 2) (roshade) | Installation, distribution or editor integration. No third-party installer change. |
| [1339](https://github.com/rojo-rbx/rojo/issues/1339) | I installed the plugin on roblox and did everything | Installation, distribution or editor integration. No third-party installer change. |
| [1338](https://github.com/rojo-rbx/rojo/issues/1338) | Crash in Rojo 7.7.1: Result::unwrap() on NotFound error in change_processor.rs line 179 | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [1336](https://github.com/rojo-rbx/rojo/issues/1336) | TweenInfo attributes (type 36) can't be decoded, so every sync reports all of the instance's attributes as removed | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [1332](https://github.com/rojo-rbx/rojo/issues/1332) | Removing a property from a file leaves its old value on the instance in Studio during live sync | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1324](https://github.com/rojo-rbx/rojo/issues/1324) | rojo just stops serving on its own | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [1323](https://github.com/rojo-rbx/rojo/issues/1323) | Rename `default.project.json` to `init.project.json`? | Proposal or workflow change. Not implemented in this fork. |
| [1321](https://github.com/rojo-rbx/rojo/issues/1321) | Crash when file not found | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [1318](https://github.com/rojo-rbx/rojo/issues/1318) | Rojo does NOT like the Robux sign | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [1317](https://github.com/rojo-rbx/rojo/issues/1317) | Expose RunContext in the sourcemap | Sourcemaps include explicit RunContext and watch changes to it. |
| [1314](https://github.com/rojo-rbx/rojo/issues/1314) | Crash while running and Claude making changes | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [1313](https://github.com/rojo-rbx/rojo/issues/1313) | VSCode plugin version | Installation, distribution or editor integration. No third-party installer change. |
| [1312](https://github.com/rojo-rbx/rojo/issues/1312) | Mass is randomly 0 on BaseParts from older rbxm files. Regression from 7.5.1 | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [1311](https://github.com/rojo-rbx/rojo/issues/1311) | In syncback, defer serialization of rbxms until end of process so they can be serialized in parallel | Proposal or workflow change. Not implemented in this fork. |
| [1309](https://github.com/rojo-rbx/rojo/issues/1309) | Rojo server stops when delete .luau files. | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [1308](https://github.com/rojo-rbx/rojo/issues/1308) | disconnect if placeid changes while rojo is still connected | Addressed by plugin session ownership, cancellation and connection error handling. |
| [1307](https://github.com/rojo-rbx/rojo/issues/1307) | JSON fails to parse if BOM is present. | Verified a BOM-prefixed project containing a commented .model.jsonc fixture. |
| [1305](https://github.com/rojo-rbx/rojo/issues/1305) | Rojo crashes on rojo serve with Result::unwrap() NotFound error (missing file in src/shared/Config) | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [1304](https://github.com/rojo-rbx/rojo/issues/1304) | Serve after Syncback errors with "Ref cannot be encoded on its own" | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1303](https://github.com/rojo-rbx/rojo/issues/1303) | Syncback while serving, after instance creation, makes duplicate  | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1302](https://github.com/rojo-rbx/rojo/issues/1302) | error | Addressed by plugin session ownership, cancellation and connection error handling. |
| [1301](https://github.com/rojo-rbx/rojo/issues/1301) | Allow configuring source of truth for sync to classes | Proposal or workflow change. Not implemented in this fork. |
| [1300](https://github.com/rojo-rbx/rojo/issues/1300) | [Bug] Rojo serve on 7.7.0 silently stops detecting newly created files (7.6.1 unaffected) | Upstream 7.7.1 includes a Windows path fix. This fork also repairs stale watch registration. |
| [1299](https://github.com/rojo-rbx/rojo/issues/1299) | [Feature Request] Add syncbackRule "ignoreClasses" | Proposal or workflow change. Not implemented in this fork. |
| [1298](https://github.com/rojo-rbx/rojo/issues/1298) | not mise compatible | Installation, distribution or editor integration. No third-party installer change. |
| [1292](https://github.com/rojo-rbx/rojo/issues/1292) | PrimaryPart not set on live sync, but correct on full rebuild | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1288](https://github.com/rojo-rbx/rojo/issues/1288) | Model and Meta JSON Files Work but Create Additional ModuleScript Instances | Open report. No verified fix for this individual reproduction. |
| [1283](https://github.com/rojo-rbx/rojo/issues/1283) | $path in a rojo tree can't refer to lua_init files (i.e init.luau) | Explicit init script paths work without adding a duplicate init child. |
| [1279](https://github.com/rojo-rbx/rojo/issues/1279) | [feature request] breakpoints & debugger sync w/ new apis | Proposal or workflow change. Not implemented in this fork. |
| [1277](https://github.com/rojo-rbx/rojo/issues/1277) | [feature request] add sourcemap-aware output sync to `rojo serve` | Proposal or workflow change. Not implemented in this fork. |
| [1275](https://github.com/rojo-rbx/rojo/issues/1275) | Harden API body handling | Interrupted API request bodies return a structured error. |
| [1273](https://github.com/rojo-rbx/rojo/issues/1273) | Made two way Source sync stable | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [1263](https://github.com/rojo-rbx/rojo/issues/1263) | Allow updating/uploading asset using Open Cloud API key | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [1259](https://github.com/rojo-rbx/rojo/issues/1259) | Syncing with team create enabled corrupts MeshParts | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [1249](https://github.com/rojo-rbx/rojo/issues/1249) | Rojo crashed! You are running Rojo 7.7.0-rc.1. | Open report. No verified fix for this individual reproduction. |
| [1246](https://github.com/rojo-rbx/rojo/issues/1246) | Syncback doesn't sync nested scripts that aren't parented to other scripts | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1245](https://github.com/rojo-rbx/rojo/issues/1245) | Syncback json field ordering is non-deterministic | Metadata properties and attributes are sorted. Other serializations not covered. |
| [1240](https://github.com/rojo-rbx/rojo/issues/1240) | Rojo 7.7.0-rc.1 displays nondescript error message when server closes while plugin is connected | Addressed by plugin session ownership, cancellation and connection error handling. |
| [1238](https://github.com/rojo-rbx/rojo/issues/1238) | "502: API Services rejected request with error. Error code: 30 Reason: The provided universe is not valid." after Rojo  Plugin sync | Open report. No verified fix for this individual reproduction. |
| [1236](https://github.com/rojo-rbx/rojo/issues/1236) | Rojo crashes when Studio connects on macOS (IsADirectory panic in change_processor.rs) | Two-way writes now resolve script directories to the init file. |
| [1235](https://github.com/rojo-rbx/rojo/issues/1235) | `*.model.jsonc` files are not recognized | Verified a BOM-prefixed project containing a commented .model.jsonc fixture. |
| [1232](https://github.com/rojo-rbx/rojo/issues/1232) | Rojo quitting every time i change something on vs code | Related to connection or initial sync. Original report not independently reproduced. |
| [1230](https://github.com/rojo-rbx/rojo/issues/1230) | Make identifying failures in patch visualizer easier | Proposal or workflow change. Not implemented in this fork. |
| [1229](https://github.com/rojo-rbx/rojo/issues/1229) | Add `--watch` flag to syncback | Proposal or workflow change. Not implemented in this fork. |
| [1225](https://github.com/rojo-rbx/rojo/issues/1225) | Rojo tries to create a file within a file on syncback. | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1221](https://github.com/rojo-rbx/rojo/issues/1221) | Rojo unable to sync mesh parts correctly | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [1219](https://github.com/rojo-rbx/rojo/issues/1219) | StyleRule crashing Rojo on sync | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [1210](https://github.com/rojo-rbx/rojo/issues/1210) | Rojo crashing while using Bat script to install packages | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [1207](https://github.com/rojo-rbx/rojo/issues/1207) | Enhance Rojo to use file ids instead of string-based paths. | Proposal or workflow change. Not implemented in this fork. |
| [1206](https://github.com/rojo-rbx/rojo/issues/1206) | Change processor tries to write to directory as if it were a file sometimes | Two-way writes now resolve script directories to the init file. |
| [1204](https://github.com/rojo-rbx/rojo/issues/1204) | Allow syncing csv files as text module scripts | Proposal or workflow change. Not implemented in this fork. |
| [1203](https://github.com/rojo-rbx/rojo/issues/1203) | Folder renames/moves do not trigger recursive path invalidation in `RojoTree` | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [1195](https://github.com/rojo-rbx/rojo/issues/1195) | Syncback crashes after initial sync with non-standard init script | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1193](https://github.com/rojo-rbx/rojo/issues/1193) | possible duplicate instance on sync after adding non-Folder object | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [1190](https://github.com/rojo-rbx/rojo/issues/1190) | Rojo doesn't keep track of files referenced by external json | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1180](https://github.com/rojo-rbx/rojo/issues/1180) | Syncback requires the children of project nodes to be unique even when not serialized | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1171](https://github.com/rojo-rbx/rojo/issues/1171) | Cancel button when connecting to server | Addressed by plugin session ownership, cancellation and connection error handling. |
| [1166](https://github.com/rojo-rbx/rojo/issues/1166) | Syncback does not support `emitLegacyScripts` | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1160](https://github.com/rojo-rbx/rojo/issues/1160) | [Feature] Add support for "extends" config | Proposal or workflow change. Not implemented in this fork. |
| [1158](https://github.com/rojo-rbx/rojo/issues/1158) | Rojo Plugin UI: Version Label overlaps with Last updated Label | Open Studio UI report; requires testing at the reported size or scroll position. |
| [1154](https://github.com/rojo-rbx/rojo/issues/1154) | Plugin attempts to delete StarterCharacterScripts and StarterPlayerScripts if StarterPlayer in the project file is a meta file with StarterCharacterScripts and StarterPlayerScripts as children | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1148](https://github.com/rojo-rbx/rojo/issues/1148) | Folder duplication | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [1141](https://github.com/rojo-rbx/rojo/issues/1141) | Move `rojo upload` over to be entirely opencloud and deprecated cookie authentication | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [1139](https://github.com/rojo-rbx/rojo/issues/1139) | Allow Numbers as Enum Values in `model.json` Format | Numeric members known to the reflected enum are accepted. |
| [1136](https://github.com/rojo-rbx/rojo/issues/1136) | Rojo crashes or freezes when syncing large projects (10,000+ instances) | Related to connection or initial sync. Original report not independently reproduced. |
| [1133](https://github.com/rojo-rbx/rojo/issues/1133) | Roblox Studio constantly crashing + Rojo error | Related to connection or initial sync. Original report not independently reproduced. |
| [1121](https://github.com/rojo-rbx/rojo/issues/1121) | Details: called `Option::unwrap()` on a `None` value | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [1115](https://github.com/rojo-rbx/rojo/issues/1115) | Make SerializationService fallback more resistant to failures | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [1105](https://github.com/rojo-rbx/rojo/issues/1105) | BUG: Details: called `Result::unwrap()`  | Open report. No verified fix for this individual reproduction. |
| [1099](https://github.com/rojo-rbx/rojo/issues/1099) | Idea: Transformers | Proposal or workflow change. Not implemented in this fork. |
| [1094](https://github.com/rojo-rbx/rojo/issues/1094) | What does this mean? It says I have to report the issue so here you go ig. | Open report. No verified fix for this individual reproduction. |
| [1090](https://github.com/rojo-rbx/rojo/issues/1090) | `rojo sourcemap` crashes on absolute paths in default.project.json | Base already uses pathdiff; the historical StripPrefixError has not been reproduced. |
| [1078](https://github.com/rojo-rbx/rojo/issues/1078) | enable init.meta.json in init.client.luau folders to support changing className while having emitLegacyScripts diabled | Proposal or workflow change. Not implemented in this fork. |
| [1075](https://github.com/rojo-rbx/rojo/issues/1075) | Support multiple users syncing at once in Team Create | Proposal or workflow change. Not implemented in this fork. |
| [1073](https://github.com/rojo-rbx/rojo/issues/1073) | Live sync not working at all | Related to connection or initial sync. Original report not independently reproduced. |
| [1071](https://github.com/rojo-rbx/rojo/issues/1071) | Regularly disconnecting | Related to connection or initial sync. Original report not independently reproduced. |
| [1070](https://github.com/rojo-rbx/rojo/issues/1070) | Crashing every few minutes | Open report. No verified fix for this individual reproduction. |
| [1065](https://github.com/rojo-rbx/rojo/issues/1065) | Scripts not syncing | Related to connection or initial sync. Original report not independently reproduced. |
| [1063](https://github.com/rojo-rbx/rojo/issues/1063) | Issues When Installing | Installation, distribution or editor integration. No third-party installer change. |
| [1061](https://github.com/rojo-rbx/rojo/issues/1061) | Studio having this strange reset when using Rojo | Open report. No verified fix for this individual reproduction. |
| [1047](https://github.com/rojo-rbx/rojo/issues/1047) | Ability for tree.$path to take an array for sourcemap generation. | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [1046](https://github.com/rojo-rbx/rojo/issues/1046) | Double clicking the Connect button in quick session can lead to a double call, and subseqeunt crash | Addressed by plugin session ownership, cancellation and connection error handling. |
| [1045](https://github.com/rojo-rbx/rojo/issues/1045) | Rojo doesnt sync | Related to connection or initial sync. Original report not independently reproduced. |
| [1038](https://github.com/rojo-rbx/rojo/issues/1038) | Rojo is crash | Open report. No verified fix for this individual reproduction. |
| [1036](https://github.com/rojo-rbx/rojo/issues/1036) | Expand `syncRules` to include property defaults | Proposal or workflow change. Not implemented in this fork. |
| [1026](https://github.com/rojo-rbx/rojo/issues/1026) | Add support for ZSTD compression to `rojo build` | Binary builds accept --compress zstd, lz4 or none. |
| [1022](https://github.com/rojo-rbx/rojo/issues/1022) | Parallel Luau support | Proposal or workflow change. Not implemented in this fork. |
| [1019](https://github.com/rojo-rbx/rojo/issues/1019) | Changes in directly defined files do not get detected (In WSL) | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [1018](https://github.com/rojo-rbx/rojo/issues/1018) | glob_ignore_rules always adds rules, even if the rules already exist, leading to an exploding rules list if the projects are too deep | Duplicate ignore rules are no longer appended. |
| [1017](https://github.com/rojo-rbx/rojo/issues/1017) | Tags not applied with .model.json or .meta.json files | Open report. No verified fix for this individual reproduction. |
| [1016](https://github.com/rojo-rbx/rojo/issues/1016) | Special file names as $path cause errors | Explicit init script paths work without adding a duplicate init child. |
| [1014](https://github.com/rojo-rbx/rojo/issues/1014) | "[Rojo-Warn] Precommit hook errored" each time rojo updates roblox studio files from filesystem change. | Open report. No verified fix for this individual reproduction. |
| [1009](https://github.com/rojo-rbx/rojo/issues/1009) | Inconsistent sync result depending on whether plugin is connected when the change happens | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [1007](https://github.com/rojo-rbx/rojo/issues/1007) | Rojo should have a command to 'patch' an existing Roblox file using a Rojo project | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [1003](https://github.com/rojo-rbx/rojo/issues/1003) | Rojo's library could use dependency trimming | Proposal or workflow change. Not implemented in this fork. |
| [1001](https://github.com/rojo-rbx/rojo/issues/1001) | Rojo's library should export more | Proposal or workflow change. Not implemented in this fork. |
| [999](https://github.com/rojo-rbx/rojo/issues/999) | `rojo sourcemap` should generate sourcemaps relative to the output instead of the cwd. | Proposal or workflow change. Not implemented in this fork. |
| [995](https://github.com/rojo-rbx/rojo/issues/995) | `rojo init --kind place` should produce a structure that mirrors common services | Proposal or workflow change. Not implemented in this fork. |
| [983](https://github.com/rojo-rbx/rojo/issues/983) | `rojo upload` does not work with plugins | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [981](https://github.com/rojo-rbx/rojo/issues/981) | Rojo deletes workspace.CurrentCamera | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [975](https://github.com/rojo-rbx/rojo/issues/975) | Alternative to servePlaceIds that uses GameId | Proposal or workflow change. Not implemented in this fork. |
| [971](https://github.com/rojo-rbx/rojo/issues/971) | Strip out Rojo Target attributes on build if the target instance is in the build | Proposal or workflow change. Not implemented in this fork. |
| [969](https://github.com/rojo-rbx/rojo/issues/969) | Add setting for ignoring empty directories | Proposal or workflow change. Not implemented in this fork. |
| [966](https://github.com/rojo-rbx/rojo/issues/966) | Rojo crashes when updating npm package | Watcher fix covers vanished paths. Individual reports may have other causes. |
| [963](https://github.com/rojo-rbx/rojo/issues/963) | init.json files should change the parent directory to be a script | Proposal or workflow change. Not implemented in this fork. |
| [962](https://github.com/rojo-rbx/rojo/issues/962) | Suggestions to improve fmt-project | Proposal or workflow change. Not implemented in this fork. |
| [938](https://github.com/rojo-rbx/rojo/issues/938) | Parent and Child Folder: Cannot add an instance from a patch that has no parent. | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [932](https://github.com/rojo-rbx/rojo/issues/932) | Emit helpful errors when file system access fails rather than just calling `unwrap`. | Open report. No verified fix for this individual reproduction. |
| [931](https://github.com/rojo-rbx/rojo/issues/931) | Upload.ashx endpoint has been disabled for places, we should raise an error when trying with `rojo upload` | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [926](https://github.com/rojo-rbx/rojo/issues/926) | Cookie authentication uploading will stop working for places | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [925](https://github.com/rojo-rbx/rojo/issues/925) | Toggling emitLegacyScripts causes ignored instances to be deleted | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [919](https://github.com/rojo-rbx/rojo/issues/919) | Option for $ignoreUnknownDescendants | Proposal or workflow change. Not implemented in this fork. |
| [907](https://github.com/rojo-rbx/rojo/issues/907) | Glob ignore paths doesn't work for projects that define content above their relative path | Open report. No verified fix for this individual reproduction. |
| [906](https://github.com/rojo-rbx/rojo/issues/906) | Naming a luau file "Init" instead of "init" causes the special case to duplicate scripts | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [895](https://github.com/rojo-rbx/rojo/issues/895) | Omit `.project.json` when searching for a project file | Proposal or workflow change. Not implemented in this fork. |
| [885](https://github.com/rojo-rbx/rojo/issues/885) | Add support for passing authorization data through the studio plugin | Proposal or workflow change. Not implemented in this fork. |
| [876](https://github.com/rojo-rbx/rojo/issues/876) | Default project does not build/sync correctly | Open report. No verified fix for this individual reproduction. |
| [874](https://github.com/rojo-rbx/rojo/issues/874) | Plugin doesn't report `Workspace.PlayerCharacterDestroyBehavior` failing to be set | Open report. No verified fix for this individual reproduction. |
| [860](https://github.com/rojo-rbx/rojo/issues/860) | Add documentation about how to set up and use Tracy to CONTRIBUTING.md | Added fork profiling instructions in FORK.md. |
| [812](https://github.com/rojo-rbx/rojo/issues/812) | Full managed rojo workflow tutorial | Proposal or workflow change. Not implemented in this fork. |
| [791](https://github.com/rojo-rbx/rojo/issues/791) | Support gradual adoption of RunContext for individual scripts | Proposal or workflow change. Not implemented in this fork. |
| [789](https://github.com/rojo-rbx/rojo/issues/789) | Adding a more human-accessible representation of a BinaryString is breaking | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [777](https://github.com/rojo-rbx/rojo/issues/777) | Rojo error when renaming script with child script | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [759](https://github.com/rojo-rbx/rojo/issues/759) | Roact error when scrolling through Patch Visualizer | Open Studio UI report; requires testing at the reported size or scroll position. |
| [756](https://github.com/rojo-rbx/rojo/issues/756) | Tooltip component errored when scrolling in the Patch Visualizer | Open Studio UI report; requires testing at the reported size or scroll position. |
| [750](https://github.com/rojo-rbx/rojo/issues/750) | Crashing on "rojo serve" | Open report. No verified fix for this individual reproduction. |
| [747](https://github.com/rojo-rbx/rojo/issues/747) | Memofs needs logging | Proposal or workflow change. Not implemented in this fork. |
| [743](https://github.com/rojo-rbx/rojo/issues/743) | Updating project json during sync sends over update.changedMetadata even when no metadata changed | Internal metadata changes no longer send a Studio property update. |
| [736](https://github.com/rojo-rbx/rojo/issues/736) | Prevent building model as a place and vice versa | Model-to-place builds rejected before opening output. Model serving remains supported. |
| [732](https://github.com/rojo-rbx/rojo/issues/732) | Upgrade `notify` as a dependency of `memofs` | Proposal or workflow change. Not implemented in this fork. |
| [716](https://github.com/rojo-rbx/rojo/issues/716) | "ignoreUnknownInstances" defaulting to false is absolutely destructive and needs to be changed to true | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [706](https://github.com/rojo-rbx/rojo/issues/706) | Support breaking changes to the project format with a `$version` field | Proposal or workflow change. Not implemented in this fork. |
| [704](https://github.com/rojo-rbx/rojo/issues/704) | Incompatibility with ObjectValue | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [676](https://github.com/rojo-rbx/rojo/issues/676) | Rojo is too hard to install, and should ship OS-specific installers | Installation, distribution or editor integration. No third-party installer change. |
| [673](https://github.com/rojo-rbx/rojo/issues/673) | Rojo doubling objects | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [660](https://github.com/rojo-rbx/rojo/issues/660) | Does not throw error for names with invalid characters | Open report. No verified fix for this individual reproduction. |
| [647](https://github.com/rojo-rbx/rojo/issues/647) | Does not sync emoji text from a .rbxmx | Open format/engine issue. Needs a round trip with the reported asset or property. |
| [644](https://github.com/rojo-rbx/rojo/issues/644) | Stack overflow when `default.project.json` refers to `.` | Circular project references now report an error before the stack overflows. |
| [640](https://github.com/rojo-rbx/rojo/issues/640) | Changing the casing of a directory breaks sync | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [638](https://github.com/rojo-rbx/rojo/issues/638) | Rojo Headless Plugin API Proposal | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [626](https://github.com/rojo-rbx/rojo/issues/626) | Let project file be provided via stdin | Proposal or workflow change. Not implemented in this fork. |
| [577](https://github.com/rojo-rbx/rojo/issues/577) | `rojo build --watch` not registering globIgnorePaths or needs debounce | Open report. No verified fix for this individual reproduction. |
| [570](https://github.com/rojo-rbx/rojo/issues/570) | Rojo create 2 scripts from 1 file | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [565](https://github.com/rojo-rbx/rojo/issues/565) | init.rbxm files | Proposal or workflow change. Not implemented in this fork. |
| [559](https://github.com/rojo-rbx/rojo/issues/559) | Rojo Hot-Reloading | Proposal or workflow change. Not implemented in this fork. |
| [536](https://github.com/rojo-rbx/rojo/issues/536) | Optional paths do not update when added during session | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [510](https://github.com/rojo-rbx/rojo/issues/510) | Empty file syncing after messing with init.lua, issue persists even after creating new files. | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [508](https://github.com/rojo-rbx/rojo/issues/508) | Split Rojo library | Proposal or workflow change. Not implemented in this fork. |
| [487](https://github.com/rojo-rbx/rojo/issues/487) | Respect Roblox's blocking of ports | Open report. No verified fix for this individual reproduction. |
| [481](https://github.com/rojo-rbx/rojo/issues/481) | Cannot add an instance from a patch that has no parent error | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [475](https://github.com/rojo-rbx/rojo/issues/475) | Creates two of everything. Two folders, two scripts etc. | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [461](https://github.com/rojo-rbx/rojo/issues/461) | Update documentation to warn against StarterPlayerScripts declaration | Proposal or workflow change. Not implemented in this fork. |
| [460](https://github.com/rojo-rbx/rojo/issues/460) | Add support for multiple $paths to be specified | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [456](https://github.com/rojo-rbx/rojo/issues/456) | Cannot add an instance from a patch that has no parent error | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [455](https://github.com/rojo-rbx/rojo/issues/455) | Cannot rename instances? | Open report. No verified fix for this individual reproduction. |
| [451](https://github.com/rojo-rbx/rojo/issues/451) | Support $path within $properties | Proposal or workflow change. Not implemented in this fork. |
| [450](https://github.com/rojo-rbx/rojo/issues/450) | Project file inheritance | Proposal or workflow change. Not implemented in this fork. |
| [445](https://github.com/rojo-rbx/rojo/issues/445) | Crash Report | Open report. No verified fix for this individual reproduction. |
| [435](https://github.com/rojo-rbx/rojo/issues/435) | Deleting an init.lua inside a folder breaks code sycning | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [395](https://github.com/rojo-rbx/rojo/issues/395) | Add project item property to disable syncing | Proposal or workflow change. Not implemented in this fork. |
| [392](https://github.com/rojo-rbx/rojo/issues/392) | Rojo 6.x "build" command fails when src contains symlinked folders | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [335](https://github.com/rojo-rbx/rojo/issues/335) | Include file paths in the /api/read endpoint | Proposal or workflow change. Not implemented in this fork. |
| [321](https://github.com/rojo-rbx/rojo/issues/321) | "Headless" studio plugin | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [315](https://github.com/rojo-rbx/rojo/issues/315) | Reconciler fails on an error of some description when a project is disconnected and reconnected or updated | Related to connection or initial sync. Original report not independently reproduced. |
| [305](https://github.com/rojo-rbx/rojo/issues/305) | 'rojo open' command | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [296](https://github.com/rojo-rbx/rojo/issues/296) | Instance tracking and partial rehydration | Proposal or workflow change. Not implemented in this fork. |
| [295](https://github.com/rojo-rbx/rojo/issues/295) | Track "source" for PatchSet | Proposal or workflow change. Not implemented in this fork. |
| [293](https://github.com/rojo-rbx/rojo/issues/293) | Write-through caching in VFS | Proposal or workflow change. Not implemented in this fork. |
| [287](https://github.com/rojo-rbx/rojo/issues/287) | rojo build should be able to update a place without nuking it | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [278](https://github.com/rojo-rbx/rojo/issues/278) | Symlinked folders drop changes on some platforms | Open: file identity or invalidation. The reported tree must converge after that sequence. |
| [276](https://github.com/rojo-rbx/rojo/issues/276) | Normalize paths to use system directory separator | Open report. No verified fix for this individual reproduction. |
| [254](https://github.com/rojo-rbx/rojo/issues/254) | Flag to make Rojo output all input paths when building | Proposal or workflow change. Not implemented in this fork. |
| [237](https://github.com/rojo-rbx/rojo/issues/237) | UISizeConstraint MaxSize can be set before MinSize | Open: needs a fixture proving ownership, defaults or references without losing Studio data. |
| [233](https://github.com/rojo-rbx/rojo/issues/233) | Stabilize Rojo API | Proposal or workflow change. Not implemented in this fork. |
| [225](https://github.com/rojo-rbx/rojo/issues/225) | Allow default filename to be supplied in the project file | Proposal or workflow change. Not implemented in this fork. |
| [208](https://github.com/rojo-rbx/rojo/issues/208) | 'rojo export' command | Proposal or workflow change. Not implemented in this fork. |
| [197](https://github.com/rojo-rbx/rojo/issues/197) | Defining the class name of a partition in init.meta.json errors | Open report. No verified fix for this individual reproduction. |
| [190](https://github.com/rojo-rbx/rojo/issues/190) | Strict model mode | Proposal or workflow change. Not implemented in this fork. |
| [172](https://github.com/rojo-rbx/rojo/issues/172) | Add support for multiple top-level instances in rbxmx and rbxm formats | Proposal or workflow change. Not implemented in this fork. |
| [171](https://github.com/rojo-rbx/rojo/issues/171) | Add support for multiple top-level instances in *.model.json format | Proposal or workflow change. Not implemented in this fork. |
| [167](https://github.com/rojo-rbx/rojo/issues/167) | Merge instances from the filesystem and project? | Proposal or workflow change. Not implemented in this fork. |
| [166](https://github.com/rojo-rbx/rojo/issues/166) | Rojo Cookbook | Proposal or workflow change. Not implemented in this fork. |
| [164](https://github.com/rojo-rbx/rojo/issues/164) | Two-Way Sync | Proposal or workflow change. Not implemented in this fork. |
| [163](https://github.com/rojo-rbx/rojo/issues/163) | Change property behavior | Proposal or workflow change. Not implemented in this fork. |
| [160](https://github.com/rojo-rbx/rojo/issues/160) | Profiles | Proposal or workflow change. Not implemented in this fork. |
| [55](https://github.com/rojo-rbx/rojo/issues/55) | Support user plugins | Related upstream proposal reviewed; see the pull request inventory for disposition. |
| [33](https://github.com/rojo-rbx/rojo/issues/33) | Partition-level ignore field | Proposal or workflow change. Not implemented in this fork. |
| [1](https://github.com/rojo-rbx/rojo/issues/1) | Rojo Roadmap Meta-Issue | Proposal or workflow change. Not implemented in this fork. |
