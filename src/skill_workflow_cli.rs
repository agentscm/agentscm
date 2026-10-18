//! Trusted-local skill workflow commands. The global data directory is operator-selected.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use agentscm::execution::Cancellation;
#[cfg(unix)]
use agentscm::skill_workflow::runtime::CommandExecutor;
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
        /// Trusted absolute path of the Docker CLI executable.
        #[arg(long, requires = "docker_socket")]
        docker: Option<PathBuf>,
        /// Existing local Unix socket of the trusted Docker daemon.
        #[arg(long, requires = "docker")]
        docker_socket: Option<PathBuf>,
    },
    /// Inspect a retained run without advancing it.
    Status { run_id: String },
    /// Continue a retained run after revalidating its source, skill and policy bindings.
    Resume {
        run_id: String,
        #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u32).range(1..=32))]
        max_steps: u32,
        /// Trusted absolute path of the Docker CLI executable.
        #[arg(long, requires = "docker_socket")]
        docker: Option<PathBuf>,
        /// Existing local Unix socket of the trusted Docker daemon.
        #[arg(long, requires = "docker")]
        docker_socket: Option<PathBuf>,
    },
    /// Cancel unfinished work while preserving completed observations.
    Cancel {
        run_id: String,
        /// Trusted absolute path of the Docker CLI executable; required to reconcile a container.
        #[arg(long, requires = "docker_socket")]
        docker: Option<PathBuf>,
        /// Existing local Unix socket of the trusted Docker daemon.
        #[arg(long, requires = "docker")]
        docker_socket: Option<PathBuf>,
    },
    /// Recover one interrupted command step after reconciling its container.
    Recover {
        run_id: String,
        step_id: String,
        /// Trusted absolute path of the Docker CLI executable.
        #[arg(long)]
        docker: PathBuf,
        /// Existing local Unix socket of the trusted Docker daemon.
        #[arg(long)]
        docker_socket: PathBuf,
    },
}

pub fn run(data_dir: &Path, command: SkillWorkflowCommand) -> Result<()> {
    match command {
        SkillWorkflowCommand::Plan { input } => {
            let definition = read_definition(&input)?;
            let plan = skill_workflow::compile(data_dir, &definition)?;
            crate::print_json(&serde_json::to_value(plan)?);
            Ok(())
        }
        SkillWorkflowCommand::Run {
            run_id,
            input,
            docker,
            docker_socket,
        } => {
            let definition = read_definition(&input)?;
            let plan = skill_workflow::compile(data_dir, &definition)?;
            skill_workflow::start(data_dir, &run_id, &plan)?;
            print_execution(&advance(data_dir, &run_id, 32, docker, docker_socket)?)
        }
        SkillWorkflowCommand::Status { run_id } => {
            crate::print_json(&serde_json::to_value(skill_workflow::status(
                data_dir, &run_id,
            )?)?);
            Ok(())
        }
        SkillWorkflowCommand::Resume {
            run_id,
            max_steps,
            docker,
            docker_socket,
        } => print_execution(&advance(
            data_dir,
            &run_id,
            max_steps as usize,
            docker,
            docker_socket,
        )?),
        SkillWorkflowCommand::Cancel {
            run_id,
            docker,
            docker_socket,
        } => {
            crate::print_json(&serde_json::to_value(cancel(
                data_dir,
                &run_id,
                docker,
                docker_socket,
            )?)?);
            Ok(())
        }
        SkillWorkflowCommand::Recover {
            run_id,
            step_id,
            docker,
            docker_socket,
        } => print_execution(&recover(
            data_dir,
            &run_id,
            &step_id,
            docker,
            docker_socket,
        )?),
    }
}

fn advance(
    data_dir: &Path,
    run_id: &str,
    max_steps: usize,
    docker: Option<PathBuf>,
    docker_socket: Option<PathBuf>,
) -> Result<RunView> {
    #[cfg(unix)]
    if let (Some(docker), Some(docker_socket)) = (docker.as_ref(), docker_socket.as_ref()) {
        return with_executor(data_dir, docker, docker_socket, |executor| {
            skill_workflow::runtime::advance_with_executor(data_dir, run_id, max_steps, executor)
        });
    }
    reject_unavailable_backend(&docker, &docker_socket)?;
    skill_workflow::advance(data_dir, run_id, max_steps)
}

fn cancel(
    data_dir: &Path,
    run_id: &str,
    docker: Option<PathBuf>,
    docker_socket: Option<PathBuf>,
) -> Result<RunView> {
    #[cfg(unix)]
    if let (Some(docker), Some(docker_socket)) = (docker.as_ref(), docker_socket.as_ref()) {
        return with_executor(data_dir, docker, docker_socket, |executor| {
            skill_workflow::runtime::cancel_with_executor(data_dir, run_id, executor)
        });
    }
    reject_unavailable_backend(&docker, &docker_socket)?;
    skill_workflow::cancel(data_dir, run_id)
}

#[cfg(unix)]
fn recover(
    data_dir: &Path,
    run_id: &str,
    step_id: &str,
    docker: PathBuf,
    docker_socket: PathBuf,
) -> Result<RunView> {
    with_executor(data_dir, &docker, &docker_socket, |executor| {
        skill_workflow::runtime::recover_command(data_dir, run_id, step_id, executor)
    })
}

#[cfg(not(unix))]
fn recover(
    _data_dir: &Path,
    _run_id: &str,
    _step_id: &str,
    _docker: PathBuf,
    _docker_socket: PathBuf,
) -> Result<RunView> {
    Err(Error::Invalid(
        "skill workflow command execution is unavailable on this platform".into(),
    ))
}

#[cfg(unix)]
fn with_executor<T>(
    data_dir: &Path,
    docker: &Path,
    docker_socket: &Path,
    operation: impl FnOnce(&CommandExecutor<'_>) -> Result<T>,
) -> Result<T> {
    let work_dir = crate::private_execution_dir(data_dir)?;
    let Some((_config, backend)) = crate::optional_backend(
        Some(docker.to_owned()),
        Some(docker_socket.to_owned()),
        &work_dir,
    )?
    else {
        return Err(Error::Invalid(
            "--docker and --docker-socket must be provided together".into(),
        ));
    };
    let cancellation = Cancellation::default();
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let signals = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
            cancellation.cancel();
        }
    });
    let executor = CommandExecutor {
        backend: &backend,
        work_dir: &work_dir,
        cancellation: &cancellation,
    };
    let result = tokio::task::block_in_place(|| operation(&executor));
    signals.abort();
    result
}

fn reject_unavailable_backend(
    docker: &Option<PathBuf>,
    docker_socket: &Option<PathBuf>,
) -> Result<()> {
    if docker.is_some() != docker_socket.is_some() {
        return Err(Error::Invalid(
            "--docker and --docker-socket must be provided together".into(),
        ));
    }
    #[cfg(not(unix))]
    if docker.is_some() {
        return Err(Error::Invalid(
            "skill workflow command execution is unavailable on this platform".into(),
        ));
    }
    Ok(())
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
