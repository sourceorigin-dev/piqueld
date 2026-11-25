//! `just standard`: the rules of the shared engineering standard, which the
//! `sourceorigin-standard` crate's checks enforce. Its README lists them, and
//! `standard.toml` and `budget.toml` hold piqueld's settings, deviations and
//! exception budgets.

use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use sourceorigin_standard::rules;

use crate::Workspace;

/// `cargo xtask standard [--fix | report | rules | skills]`.
#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct Standard {
    /// First make the fixes that need no judgement, and print each.
    #[arg(long)]
    fix: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

/// What `cargo xtask standard` does instead of checking the rules.
#[derive(Clone, Copy, Subcommand)]
enum Command {
    /// Print each rule's violations and deviation, with the `#[expect]`s,
    /// `// INVARIANT:` lines and allowed lints, as JSON.
    Report,
    /// List the rules.
    Rules,
    /// Write the standard's shared skills into `.agents/skills`.
    Skills,
}

impl Standard {
    /// Checks the rules, printing each violation, or runs the command.
    pub(crate) fn run(self, workspace: &Workspace) -> Result<ExitCode> {
        let root = workspace.root();
        match self.command {
            Some(Command::Report) => {
                let report = rules::report(root).context("report on the standard's rules")?;
                println!("{}", report.to_json().context("serialize the report")?);
            }
            Some(Command::Rules) => print!("{}", rules::describe()),
            Some(Command::Skills) => {
                for fix in rules::sync_skills(root).context("sync the standard's skills")? {
                    println!("synced {fix}");
                }
            }
            None => {
                if self.fix {
                    for fix in rules::fix(root).context("fix the standard's rules")? {
                        println!("fixed {fix}");
                    }
                }
                let violations = rules::check(root).context("check the standard's rules")?;
                for violation in &violations {
                    eprintln!("{violation}");
                }
                if !violations.is_empty() {
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        Ok(ExitCode::SUCCESS)
    }
}
