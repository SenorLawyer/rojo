use clap::Parser;

/// Open documentation, or print the bundled agent workflow with --agent.
#[derive(Debug, Parser)]
pub struct DocCommand {
    /// Print offline instructions for agents, Studio controls, and version switching.
    #[clap(long)]
    pub agent: bool,
}

pub const AGENT_GUIDE: &str = include_str!("../../docs/agent-workflow.md");

impl DocCommand {
    pub fn run(self) -> anyhow::Result<()> {
        if self.agent {
            println!("{AGENT_GUIDE}");
            return Ok(());
        }
        opener::open("https://github.com/SenorLawyer/rojo/tree/master/docs")?;
        Ok(())
    }
}
