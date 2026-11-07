# Agent instructions

This project is built with the SenorLawyer/rojo fork of Rojo, which has
commands and file types that upstream Rojo and its website do not document,
such as `rojo test`, `rojo typecheck`, `rojo assets sync`, `*.data.csv`
modules and the Studio bridge. Before working on the project, read
ROJO_AGENT.md, or run `rojo doc --agent` for the guide that matches the
installed CLI. `rojo doc --agent --list` lists the detailed guides.

When both Roblox's Studio MCP server and `rojo mcp` are connected, use
Roblox's for generic Studio work and Open Cloud calls (`execute_luau`,
`search_game_tree`, `inspect_instance`, `insert_asset`, `open_cloud_invoke`)
and `rojo` for work tied to the files: `studio_verify`, backups, locks,
`run_tests`, checkpoints, and the `rojo assets sync`, `rojo test --cloud` and
`rojo release` commands. `rojo doc --agent mcp` explains the split.
