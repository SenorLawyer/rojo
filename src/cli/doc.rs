use std::{fs, io, path::PathBuf};

use anyhow::{bail, Context};
use clap::Parser;

/// Open documentation, or print the bundled agent guides with --agent.
#[derive(Debug, Parser)]
pub struct DocCommand {
    /// Print offline instructions for agents. Name a topic to print its guide,
    /// for example `rojo doc --agent testing`.
    #[clap(long)]
    pub agent: bool,

    /// With --agent, the topic to print. Run `rojo doc --agent --list` to see them.
    #[clap(requires = "agent")]
    pub topic: Option<String>,

    /// With --agent, list the bundled topics.
    #[clap(long, requires = "agent", conflicts_with_all = &["topic", "write"])]
    pub list: bool,

    /// With --agent, write ROJO_AGENT.md into this project directory, replacing
    /// an older copy, and create AGENTS.md pointing to it if there is none.
    #[clap(long, value_name = "DIR", requires = "agent", conflicts_with = "topic")]
    pub write: Option<PathBuf>,
}

pub const AGENT_GUIDE: &str = include_str!("../../docs/agent-workflow.md");

/// Written to a project's AGENTS.md when it has none. Agents read AGENTS.md on
/// their own, but not ROJO_AGENT.md, so this is what makes them find the guide.
pub const AGENTS_POINTER: &str = include_str!("../../assets/agents-pointer.md");

/// Guides bundled into the binary, so agents get the docs for this exact
/// version without network access or the upstream Rojo website, which does not
/// describe this fork's commands.
pub const AGENT_TOPICS: &[(&str, &str, &str)] = &[
    (
        "testing",
        "rojo test: TestEZ/Jest specs in Studio or Open Cloud",
        include_str!("../../docs/testing.md"),
    ),
    (
        "typecheck",
        "rojo typecheck: luau-lsp analysis as a gate",
        include_str!("../../docs/typecheck.md"),
    ),
    (
        "assets",
        "rojo assets: upload images, audio and models, id module, remap after a creator move",
        include_str!("../../docs/assets.md"),
    ),
    (
        "data-modules",
        "*.data.csv files as typed ModuleScripts",
        include_str!("../../docs/data-modules.md"),
    ),
    (
        "content-properties",
        "legacy and Content asset property names",
        include_str!("../../docs/content-properties.md"),
    ),
    (
        "studio-bridge",
        "rojo studio: sessions, tree, properties, logs --follow, import/export",
        include_str!("../../docs/studio-bridge.md"),
    ),
    (
        "studio-locks",
        "rojo studio lock: advisory leases for agents sharing Studio windows",
        include_str!("../../docs/studio-locks.md"),
    ),
    (
        "mcp",
        "rojo mcp: the Studio bridge as MCP tools",
        include_str!("../../docs/mcp.md"),
    ),
    (
        "releases",
        "checkpoints, workspaces, release prepare/stage/promote/publish/rollback",
        include_str!("../../docs/workspaces.md"),
    ),
    (
        "packages",
        "rojo packages doctor/install for Wally and pesde",
        include_str!("../../docs/packages.md"),
    ),
    (
        "merge",
        "rojo merge: semantic merges of .rbxl/.rbxm files",
        include_str!("../../docs/semantic-merge.md"),
    ),
];

impl DocCommand {
    pub fn run(self) -> anyhow::Result<()> {
        if !self.agent {
            opener::open("https://github.com/SenorLawyer/rojo/tree/master/docs")?;
            return Ok(());
        }

        if self.list {
            for (name, summary, _) in AGENT_TOPICS {
                println!("{name:<20} {summary}");
            }
            return Ok(());
        }

        if let Some(dir) = self.write {
            return write_agent_files(&dir);
        }

        match self.topic {
            None => println!("{AGENT_GUIDE}"),
            Some(topic) => match AGENT_TOPICS.iter().find(|(name, ..)| *name == topic) {
                Some((_, _, contents)) => println!("{contents}"),
                None => {
                    let names: Vec<_> = AGENT_TOPICS.iter().map(|(name, ..)| *name).collect();
                    bail!("Unknown topic '{topic}'. Topics: {}", names.join(", "));
                }
            },
        }
        Ok(())
    }
}

fn write_agent_files(dir: &std::path::Path) -> anyhow::Result<()> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    // ROJO_AGENT.md is generated from this binary, so it is replaced: an
    // upgraded CLI should not leave agents reading an older version's guide.
    let guide = dir.join("ROJO_AGENT.md");
    fs::write(&guide, AGENT_GUIDE)
        .with_context(|| format!("Could not write {}", guide.display()))?;
    println!("Wrote {}", guide.display());

    // AGENTS.md belongs to the project, so it is only created, never changed.
    let agents = dir.join("AGENTS.md");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&agents)
    {
        Ok(mut file) => {
            io::Write::write_all(&mut file, AGENTS_POINTER.as_bytes())
                .with_context(|| format!("Could not write {}", agents.display()))?;
            println!("Wrote {}", agents.display());
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let existing = fs::read_to_string(&agents).unwrap_or_default();
            if !existing.contains("ROJO_AGENT.md") {
                println!(
                    "{} already exists and was left alone. Add this to it so agents find the guide:\n\n{}",
                    agents.display(),
                    AGENTS_POINTER
                );
            }
        }
        Err(err) => {
            return Err(err).with_context(|| format!("Could not write {}", agents.display()))
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn topic_names_are_unique_and_guides_are_not_empty() {
        let mut names: Vec<_> = AGENT_TOPICS.iter().map(|(name, ..)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), AGENT_TOPICS.len());
        for (name, _, contents) in AGENT_TOPICS {
            assert!(contents.starts_with("# "), "{name} has no title");
        }
    }

    #[test]
    fn the_guide_lists_every_topic() {
        for (name, ..) in AGENT_TOPICS {
            assert!(
                AGENT_GUIDE.contains(&format!("`rojo doc --agent {name}`")),
                "the agent guide does not mention the {name} topic"
            );
        }
    }

    #[test]
    fn write_replaces_the_guide_and_keeps_an_existing_agents_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ROJO_AGENT.md"), "old guide").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "project rules").unwrap();
        write_agent_files(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("ROJO_AGENT.md")).unwrap(),
            AGENT_GUIDE
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
            "project rules"
        );
    }

    #[test]
    fn write_creates_an_agents_file_that_points_to_the_guide() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_files(dir.path()).unwrap();
        let agents = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(agents.contains("ROJO_AGENT.md"));
    }
}
