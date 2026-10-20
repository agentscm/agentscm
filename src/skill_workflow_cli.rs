//! Trusted-local skill workflow commands. The global data directory is operator-selected.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use crate::agent_stop::StopAgent;
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
    /// Install an immutable workflow requirement for a native target (operator action).
    Require { target: String, input: PathBuf },
    /// Configure the first required test command and Claude review for an existing Change.
    #[cfg(unix)]
    Configure {
        change: String,
        /// Native path of the pinned review SKILL.md.
        #[arg(long)]
        skill: String,
        /// Native path of a CommandRecipe JSON within the skill directory.
        #[arg(long)]
        runner: String,
        /// Explicit native source path disclosed to Claude (repeatable).
        #[arg(long, required = true)]
        review_path: Vec<String>,
        #[arg(long, default_value = "sonnet")]
        model: String,
    },
    /// Checkpoint optional workspace edits, run the installed test/review graph, and qualify landing.
    #[cfg(unix)]
    Check(crate::skill_workflow_loop::LoopArgs),
    /// Opt-in Claude Code Stop hook. Native landing is independently enforced by the authority.
    #[cfg(unix)]
    ClaudeStop(crate::skill_workflow_loop::LoopArgs),
    /// Opt-in Codex Stop hook. Native landing is independently enforced by the authority.
    #[cfg(unix)]
    CodexStop(crate::skill_workflow_loop::LoopArgs),
    /// Preview (default) or apply a Stop hook entry in an operator-local Claude Code or Codex hook
    /// file. Writes only that file, never installs from candidate source. Give the Stop
    /// subcommand and its arguments after `--`; `--remove` takes none.
    #[cfg(unix)]
    InstallStop {
        /// Hook file to edit; it must be outside the workspace and not a symbolic link.
        #[arg(long)]
        settings: PathBuf,
        /// Write the change; without it only the result is printed.
        #[arg(long)]
        apply: bool,
        /// Remove the installed Stop hooks instead of adding one.
        #[arg(long)]
        remove: bool,
        /// With --remove, remove only the hooks for this workspace.
        #[arg(long, requires = "remove")]
        workspace: Option<PathBuf>,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Create an empty laptop workspace from an operator-enabled hosted source profile.
    #[cfg(unix)]
    RemoteInit(crate::hosted_workflow_client::HostedWorkflowArgs),
    /// Synchronize local bytes and run the exact hosted workflow without approving or landing.
    #[cfg(unix)]
    RemoteCheck(crate::hosted_workflow_client::HostedWorkflowArgs),
    /// Opt-in Claude Code Stop hook using the hosted source and workflow queue.
    #[cfg(unix)]
    RemoteStop(crate::hosted_workflow_client::HostedWorkflowArgs),
    /// Opt-in Codex Stop hook using the hosted source and workflow queue.
    #[cfg(unix)]
    RemoteCodexStop(crate::hosted_workflow_client::HostedWorkflowArgs),
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

pub async fn run(data_dir: &Path, command: SkillWorkflowCommand) -> Result<()> {
    match command {
        #[cfg(unix)]
        SkillWorkflowCommand::Configure {
            change,
            skill,
            runner,
            review_path,
            model,
        } => configure(data_dir, &change, &skill, &runner, review_path, model),
        SkillWorkflowCommand::Require { target, input } => {
            let definition = read_definition(&input)?;
            let plan = skill_workflow::compile(data_dir, &definition)?;
            let requirement =
                agentscm::authority::workflows::WorkflowRequirement::from_plan(&target, &plan)?;
            let mut authority = agentscm::authority::Authority::open(data_dir)?;
            let digest = authority.install_workflow_requirement(&requirement)?;
            crate::print_json(
                &serde_json::json!({"requirement_digest":digest,"policy_epoch":authority.policy_epoch()?,"target":target}),
            );
            Ok(())
        }
        #[cfg(unix)]
        SkillWorkflowCommand::Check(args) => {
            let value = crate::skill_workflow_loop::check(data_dir, args)?;
            let passed = value
                .get("receipt")
                .is_some_and(|receipt| receipt.is_string());
            crate::print_json(&value);
            if passed {
                Ok(())
            } else {
                Err(Error::Invalid("required workflow did not qualify the current revision; fix the reported failures and check again".into()))
            }
        }
        #[cfg(unix)]
        SkillWorkflowCommand::ClaudeStop(args) => {
            crate::skill_workflow_loop::claude_stop(data_dir, args, StopAgent::Claude)
        }
        #[cfg(unix)]
        SkillWorkflowCommand::CodexStop(args) => {
            crate::skill_workflow_loop::claude_stop(data_dir, args, StopAgent::Codex)
        }
        #[cfg(unix)]
        SkillWorkflowCommand::InstallStop {
            settings,
            apply,
            remove,
            workspace,
            command,
        } => {
            let value = crate::agent_stop_install::run(
                data_dir,
                &settings,
                apply,
                remove,
                workspace.as_deref(),
                &command,
            )?;
            crate::print_json(&value);
            Ok(())
        }
        #[cfg(unix)]
        SkillWorkflowCommand::RemoteInit(args) => {
            let value = crate::hosted_workflow_client::initialize(data_dir, &args).await?;
            crate::print_json(&value);
            Ok(())
        }
        #[cfg(unix)]
        SkillWorkflowCommand::RemoteCheck(args) => {
            let value = crate::hosted_workflow_client::check(data_dir, &args).await?;
            let passed = value
                .get("receipt")
                .is_some_and(serde_json::Value::is_string)
                && value
                    .get("workspace_changed_during_run")
                    .and_then(serde_json::Value::as_bool)
                    == Some(false);
            crate::print_json(&value);
            if passed {
                Ok(())
            } else {
                Err(Error::Invalid(
                    "the hosted workflow did not qualify the current laptop bytes".into(),
                ))
            }
        }
        #[cfg(unix)]
        SkillWorkflowCommand::RemoteStop(args) => {
            crate::hosted_workflow_client::claude_stop(data_dir, args, StopAgent::Claude).await
        }
        #[cfg(unix)]
        SkillWorkflowCommand::RemoteCodexStop(args) => {
            crate::hosted_workflow_client::claude_stop(data_dir, args, StopAgent::Codex).await
        }
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

#[cfg(unix)]
fn configure(
    data_dir: &Path,
    change: &str,
    skill: &str,
    runner: &str,
    paths: Vec<String>,
    model: String,
) -> Result<()> {
    use agentscm::authority::{Authority, workflows::WorkflowRequirement};
    use agentscm::context::{ChangeId, InputIdentity, RequiredEvidence};
    use agentscm::execution::{COMMAND_CAPABILITY, COMMAND_EVIDENCE};
    use agentscm::skill_workflow::{SkillReference, StepAction, StepDefinition, WorkflowSource};
    let mut authority = Authority::open(data_dir)?;
    let id = ChangeId {
        repository: authority.repository_id()?,
        id: change.into(),
    };
    let state = authority.change(&id)?;
    let mut context = authority.review_context(&state.context_digest)?;
    if authority.workflow_requirement(&context.target)?.is_some() {
        return Err(Error::Conflict("target already has an immutable workflow requirement; check the installed workflow instead".into()));
    }
    let recipe =
        skill_workflow::compiler::read_command_recipe(data_dir, &context.revision, runner)?;
    // The existing context supports one primary command recipe. Do not silently
    // replace a separately requested source check with a command check.
    if context
        .required_capabilities
        .iter()
        .any(|cap| cap == agentscm::checks::SOURCE_PATHS_CAPABILITY)
    {
        return Err(Error::Invalid(
            "configure requires a context without a separate source-path recipe".into(),
        ));
    }
    let recipe_digest = recipe.digest()?;
    context.recipe = InputIdentity::Digest {
        sha256: recipe_digest.clone(),
    };
    context.toolchain = InputIdentity::Digest {
        sha256: recipe.toolchain_sha256,
    };
    context.environment = InputIdentity::Digest {
        sha256: recipe.environment_sha256,
    };
    context
        .required_evidence
        .retain(|item| item.identity != COMMAND_EVIDENCE);
    context.required_evidence.push(RequiredEvidence {
        identity: COMMAND_EVIDENCE.into(),
        digest: InputIdentity::Digest {
            sha256: recipe_digest,
        },
    });
    if !context
        .required_capabilities
        .iter()
        .any(|cap| cap == COMMAND_CAPABILITY)
    {
        context
            .required_capabilities
            .push(COMMAND_CAPABILITY.into());
    }
    context.change_version = context
        .change_version
        .checked_add(1)
        .ok_or_else(|| Error::Limit("change generation exhausted".into()))?;
    context.policy_epoch = authority.policy_epoch()?;
    let root = skill
        .rsplit_once('/')
        .map(|(root, _)| format!("{root}/"))
        .unwrap_or_default();
    let relative = runner
        .strip_prefix(&root)
        .ok_or_else(|| Error::Invalid("runner must be in the skill directory".into()))?;
    let definition = WorkflowDefinition {
        version: 1,
        name: "claude-code-checks".into(),
        source: WorkflowSource {
            repository: id.repository.clone(),
            revision: context.revision.clone(),
            policy_epoch: context.policy_epoch,
        },
        skill: SkillReference {
            entrypoint: skill.into(),
            resources: vec![runner.into()],
        },
        steps: vec![
            StepDefinition {
                id: "tests".into(),
                needs: vec![],
                required: true,
                action: StepAction::Command {
                    runner: relative.into(),
                    change: id,
                    change_version: context.change_version,
                    context_digest: context.digest()?,
                },
            },
            StepDefinition {
                id: "review".into(),
                needs: vec!["tests".into()],
                required: true,
                action: StepAction::ClaudeCodeReview { paths, model },
            },
        ],
    };
    // Validate structural and source inputs before publishing a new context.
    // The context-dependent compile follows its compare-and-swap update.
    skill_workflow::compiler::validate_sources(data_dir, &definition)?;
    authority.update_change_context(state.version, &context)?;
    let plan = skill_workflow::compile(data_dir, &definition)?;
    let requirement = WorkflowRequirement::from_plan(&context.target, &plan)?;
    let digest = authority.install_workflow_requirement(&requirement)?;
    context.change_version += 1;
    context.policy_epoch = authority.policy_epoch()?;
    authority.update_change_context(context.change_version - 1, &context)?;
    crate::print_json(
        &serde_json::json!({"requirement_digest":digest,"context":context,
        "next_action":"Run skill-workflow check with the native workspace and operator-selected Docker/Claude executables."}),
    );
    Ok(())
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
