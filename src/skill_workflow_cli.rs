//! Trusted-local skill workflow commands. The global data directory is operator-selected.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use agentscm::skill_workflow::{self, RunState, RunView, WorkflowDefinition};
use agentscm::{Error, Result};
use clap::Subcommand;

const MAX_DEFINITION_BYTES: u64 = 256 * 1024;

#[derive(Debug, Subcommand)]
pub enum SkillWorkflowCommand {
    /// Validate and pin a workflow against an existing native source revision.
    Plan {
        /// JSON definition, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// Start or inspect the same exact run, then advance up to 32 steps.
    Run {
        run_id: String,
        /// JSON definition, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// Inspect a retained run without advancing it.
    Status { run_id: String },
    /// Continue a retained run after revalidating its source, skill and policy bindings.
    Resume {
        run_id: String,
        #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u32).range(1..=32))]
        max_steps: u32,
    },
    /// Cancel unfinished work while preserving completed observations.
    Cancel { run_id: String },
}

pub fn run(data_dir: &Path, command: SkillWorkflowCommand) -> Result<()> {
    match command {
        SkillWorkflowCommand::Plan { input } => {
            let definition = read_definition(&input)?;
            let plan = skill_workflow::compile(data_dir, &definition)?;
            crate::print_json(&serde_json::to_value(plan)?);
            Ok(())
        }
        SkillWorkflowCommand::Run { run_id, input } => {
            let definition = read_definition(&input)?;
            let plan = skill_workflow::compile(data_dir, &definition)?;
            skill_workflow::start(data_dir, &run_id, &plan)?;
            print_execution(&skill_workflow::advance(data_dir, &run_id, 32)?)
        }
        SkillWorkflowCommand::Status { run_id } => {
            crate::print_json(&serde_json::to_value(skill_workflow::status(
                data_dir, &run_id,
            )?)?);
            Ok(())
        }
        SkillWorkflowCommand::Resume { run_id, max_steps } => print_execution(
            &skill_workflow::advance(data_dir, &run_id, max_steps as usize)?,
        ),
        SkillWorkflowCommand::Cancel { run_id } => {
            crate::print_json(&serde_json::to_value(skill_workflow::cancel(
                data_dir, &run_id,
            )?)?);
            Ok(())
        }
    }
}

fn print_execution(view: &RunView) -> Result<()> {
    crate::print_json(&serde_json::to_value(view)?);
    if matches!(
        view.state,
        RunState::Failed | RunState::Blocked | RunState::Cancelled
    ) {
        return Err(Error::Conflict(format!(
            "skill workflow {} did not complete successfully ({:?})",
            view.run_id, view.state
        )));
    }
    Ok(())
}

fn read_definition(path: &Path) -> Result<WorkflowDefinition> {
    let mut reader: Box<dyn Read> = if path == Path::new("-") {
        Box::new(io::stdin())
    } else {
        Box::new(File::open(path)?)
    };
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(MAX_DEFINITION_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DEFINITION_BYTES {
        return Err(Error::Limit("workflow definition exceeds 256 KiB".into()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
