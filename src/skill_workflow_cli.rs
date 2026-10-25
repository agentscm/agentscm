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

/// Keep the existing Unix signal-to-cancellation bridge alive for a complete
/// productive CLI operation, including an idle watch interval.
#[cfg(unix)]
pub(crate) struct ProductiveSignals {
    pub cancellation: Cancellation,
    stopped: tokio::sync::watch::Receiver<bool>,
    task: tokio::task::JoinHandle<()>,
}
#[cfg(unix)]
impl ProductiveSignals {
    pub fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let cancellation = Cancellation::default();
        let token = cancellation.clone();
        let (sender, stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            tokio::select! {_=interrupt.recv()=>{},_=terminate.recv()=>{}}
            token.cancel();
            let _ = sender.send(true);
        });
        Ok(Self {
            cancellation,
            stopped,
            task,
        })
    }
    pub async fn stopped(&mut self) {
        if !*self.stopped.borrow() {
            let _ = self.stopped.changed().await;
        }
    }
}
#[cfg(unix)]
impl Drop for ProductiveSignals {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug, Subcommand)]
pub enum SkillWorkflowCommand {
    /// Serve local workflow tools to any stdio MCP client using fixed operator settings.
    #[cfg(unix)]
    Mcp(crate::skill_workflow_mcp::McpArgs),
    /// Inspect or configure a reusable template from exact native source.
    Library {
        #[command(subcommand)]
        command: LibraryCommand,
    },
    /// Export retained bindings and actionable continuation information.
    Handoff { run_id: String },
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
    /// Opt-in Claude Code Stop hook for agent-owned tasks. Read-only: asks the task facade whether
    /// the exact Change subject is qualified and never runs, uploads, finalizes or checkpoints.
    #[cfg(unix)]
    TaskStop(crate::agent_task_stop::TaskStopArgs),
    /// Opt-in Codex Stop hook for agent-owned tasks. Read-only like `task-stop`.
    #[cfg(unix)]
    TaskCodexStop(crate::agent_task_stop::TaskStopArgs),
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
        /// Explicit trusted-local productive action profile (not candidate source).
        #[arg(long)]
        action_profile: Option<PathBuf>,
    },
    /// Read a validated named productive artifact as JSON text.
    Artifact {
        run_id: String,
        step_id: String,
        name: String,
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
        /// Explicit trusted-local productive action profile (not candidate source).
        #[arg(long)]
        action_profile: Option<PathBuf>,
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
    /// Explicitly fence an interrupted productive action without replaying it.
    RecoverAction { run_id: String, step_id: String },
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

#[derive(Debug, Subcommand)]
pub enum LibraryCommand {
    /// Inspect one pinned template without changing its Change or executing steps.
    Inspect {
        change: String,
        #[arg(long)]
        library: String,
    },
    /// Install this template as the target's first immutable requirement.
    #[cfg(unix)]
    Configure {
        change: String,
        #[arg(long)]
        library: String,
    },
}

pub(crate) fn telemetry_action(command: &SkillWorkflowCommand) -> bool {
    match command {
        SkillWorkflowCommand::Artifact { .. }
        | SkillWorkflowCommand::Status { .. }
        | SkillWorkflowCommand::Handoff { .. }
        | SkillWorkflowCommand::Library {
            command: LibraryCommand::Inspect { .. },
        } => false,
        #[cfg(unix)]
        SkillWorkflowCommand::Mcp(_) => false,
        #[cfg(unix)]
        SkillWorkflowCommand::ClaudeStop(_)
        | SkillWorkflowCommand::CodexStop(_)
        | SkillWorkflowCommand::RemoteStop(_)
        | SkillWorkflowCommand::RemoteCodexStop(_)
        | SkillWorkflowCommand::TaskStop(_)
        | SkillWorkflowCommand::TaskCodexStop(_) => false,
        _ => true,
    }
}

pub async fn run(data_dir: &Path, command: SkillWorkflowCommand) -> Result<()> {
    match command {
        #[cfg(unix)]
        SkillWorkflowCommand::Mcp(args) => crate::skill_workflow_mcp::run(data_dir, args),
        SkillWorkflowCommand::Library { command } => run_library(data_dir, command),
        SkillWorkflowCommand::Handoff { run_id } => handoff(data_dir, &run_id),
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
        SkillWorkflowCommand::TaskStop(args) => {
            crate::agent_task_stop::task_stop(data_dir, args, StopAgent::Claude).await
        }
        #[cfg(unix)]
        SkillWorkflowCommand::TaskCodexStop(args) => {
            crate::agent_task_stop::task_stop(data_dir, args, StopAgent::Codex).await
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
            action_profile,
        } => {
            let definition = read_definition(&input)?;
            let plan = skill_workflow::compile(data_dir, &definition)?;
            skill_workflow::start(data_dir, &run_id, &plan)?;
            print_execution(&advance(
                data_dir,
                &run_id,
                32,
                docker,
                docker_socket,
                action_profile,
            )?)
        }
        SkillWorkflowCommand::Artifact {
            run_id,
            step_id,
            name,
        } => {
            let bytes = skill_workflow::runtime::artifact(data_dir, &run_id, &step_id, &name)?;
            crate::print_json(
                &serde_json::json!({"run_id":run_id,"step_id":step_id,"name":name,"content":String::from_utf8(bytes).map_err(|_| Error::Invalid("artifact is not UTF-8".into()))?}),
            );
            Ok(())
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
            action_profile,
        } => print_execution(&advance(
            data_dir,
            &run_id,
            max_steps as usize,
            docker,
            docker_socket,
            action_profile,
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
        SkillWorkflowCommand::RecoverAction { run_id, step_id } => print_execution(
            &skill_workflow::runtime::recover_action(data_dir, &run_id, &step_id)?,
        ),
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

fn current_context(data_dir: &Path, change: &str) -> Result<agentscm::context::ReviewContext> {
    let authority = agentscm::authority::Authority::open_read_only(data_dir)?;
    let id = agentscm::context::ChangeId {
        repository: authority.repository_id()?,
        id: change.into(),
    };
    let state = authority.change(&id)?;
    authority.review_context(&state.context_digest)
}

fn run_library(data_dir: &Path, command: LibraryCommand) -> Result<()> {
    crate::print_json(&library_value(data_dir, command)?);
    Ok(())
}

pub(crate) fn library_value(data_dir: &Path, command: LibraryCommand) -> Result<serde_json::Value> {
    let (change, path) = match &command {
        LibraryCommand::Inspect { change, library } => (change, library),
        #[cfg(unix)]
        LibraryCommand::Configure { change, library } => (change, library),
    };
    let context = current_context(data_dir, change)?;
    let snapshot = skill_workflow::library::load(data_dir, &context.revision, path)?;
    let definition = snapshot.instantiate(&context)?;
    skill_workflow::compiler::validate_sources(data_dir, &definition)?;
    match command {
        LibraryCommand::Inspect { .. } => Ok(serde_json::json!({
            "reference": snapshot.reference(), "template": snapshot.template(),
            "definition": definition, "admission": "not_assessed",
        })),
        #[cfg(unix)]
        LibraryCommand::Configure { .. } => {
            let mut value = configure_definition(data_dir, definition, &context)?;
            value["reference"] = serde_json::to_value(snapshot.reference())?;
            Ok(value)
        }
    }
}

fn handoff(data_dir: &Path, run_id: &str) -> Result<()> {
    crate::print_json(&handoff_value(data_dir, run_id)?);
    Ok(())
}

pub(crate) fn handoff_value(data_dir: &Path, run_id: &str) -> Result<serde_json::Value> {
    let run = skill_workflow::status(data_dir, run_id)?;
    let definition = skill_workflow::runtime::retained_definition(data_dir, run_id)?;
    let mut command_context_bindings = Vec::new();
    for step in &definition.steps {
        if let skill_workflow::StepAction::Command {
            change,
            change_version,
            context_digest,
            ..
        } = &step.action
        {
            let binding = serde_json::json!({
                "change": change,
                "change_version": change_version,
                "context_digest": context_digest,
            });
            if !command_context_bindings.contains(&binding) {
                command_context_bindings.push(binding);
            }
        }
    }
    let subject_scope = if command_context_bindings.is_empty() {
        "source_revision_only"
    } else {
        "source_revision_and_command_contexts"
    };
    let has_claude = definition.steps.iter().any(|step| {
        matches!(
            step.action,
            skill_workflow::StepAction::ClaudeCodeReview { .. }
        )
    });
    let next_action = match run.state {
        RunState::Pending | RunState::Running if has_claude => {
            "Inspect running attempts and recover interrupted command custody first. For an installed target requirement, continue with skill-workflow check for the current Change and its trusted Docker/Claude backends; direct resume does not execute Claude review steps."
        }
        RunState::Pending | RunState::Running => {
            "Inspect running attempts before resuming. Resume this exact run only while any recorded Change contexts remain current, using its required trusted backends; recover interrupted command attempts before continuing."
        }
        RunState::Succeeded | RunState::CompletedWithOmissions => {
            "Inspect retained results and omissions. For an installed target requirement, run skill-workflow check for the current Change to revalidate applicability and record its receipt before separate approval."
        }
        RunState::Failed | RunState::Blocked => {
            "Inspect failed or blocked steps and omissions. Fix source or missing capabilities, checkpoint fresh bytes, and check the current Change again; this terminal run cannot resume."
        }
        RunState::Cancelled => {
            "Cancellation is terminal. Reconcile retained command custody if needed and explicitly start a new run for the current subject."
        }
    };
    Ok(serde_json::json!({
        "run": run, "definition": definition, "next_action": next_action,
        "subject_scope": subject_scope,
        "command_context_bindings": command_context_bindings,
        "admission": "not_assessed",
    }))
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
    use agentscm::skill_workflow::{SkillReference, StepAction, StepDefinition, WorkflowSource};
    let context = current_context(data_dir, change)?;
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
            repository: context.change.repository.clone(),
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
                    change: context.change.clone(),
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
    crate::print_json(&configure_definition(data_dir, definition, &context)?);
    Ok(())
}

#[cfg(unix)]
fn configure_definition(
    data_dir: &Path,
    mut definition: WorkflowDefinition,
    expected_context: &agentscm::context::ReviewContext,
) -> Result<serde_json::Value> {
    use agentscm::authority::{Authority, workflows::WorkflowRequirement};
    use agentscm::context::{InputIdentity, RequiredEvidence};
    use agentscm::execution::{COMMAND_CAPABILITY, COMMAND_EVIDENCE};
    use agentscm::skill_workflow::StepAction;

    WorkflowRequirement::validate_admission_actions(&definition)?;
    let mut authority = Authority::open(data_dir)?;
    let state = authority.change(&expected_context.change)?;
    if state.context_digest != expected_context.digest()?
        || state.version != expected_context.change_version
    {
        return Err(Error::Conflict(
            "Change context changed before workflow configuration".into(),
        ));
    }
    let mut context = expected_context.clone();
    if authority.workflow_requirement(&context.target)?.is_some() {
        return Err(Error::Conflict("target already has an immutable workflow requirement; check the installed workflow instead".into()));
    }
    let target = authority.target(&context.target)?;
    let base_matches = match (&context.base, &target) {
        (None, None) => true,
        (Some(base), Some(target)) => {
            base.revision == target.revision && base.target_version == target.version
        }
        _ => false,
    };
    if !base_matches {
        return Err(Error::Conflict(format!(
            "Change base no longer matches its target; inspect with agentscm show {} {} and publish an updated context with agentscm context {} FILE before configuring a workflow",
            context.change.repository, context.change.id, state.version,
        )));
    }
    // Validate the complete graph/package before publishing any authority changes.
    skill_workflow::compiler::validate_sources(data_dir, &definition)?;
    let root = definition
        .skill
        .entrypoint
        .rsplit_once('/')
        .map(|(root, _)| format!("{root}/"))
        .unwrap_or_default();
    let mut primary = None;
    for step in &definition.steps {
        if let StepAction::Command { runner, .. } = &step.action {
            let recipe = skill_workflow::compiler::read_command_recipe(
                data_dir,
                &context.revision,
                &format!("{root}{runner}"),
            )?;
            let digest = recipe.digest()?;
            if let Some((previous, _)) = &primary
                && previous != &digest
            {
                return Err(Error::Invalid(
                    "one ReviewContext cannot bind multiple command recipes".into(),
                ));
            }
            primary = Some((digest, recipe));
        }
    }
    if primary.is_none()
        && (context
            .required_capabilities
            .iter()
            .any(|cap| cap == COMMAND_CAPABILITY)
            || context
                .required_evidence
                .iter()
                .any(|evidence| evidence.identity == COMMAND_EVIDENCE))
    {
        return Err(Error::Invalid(
            "a workflow without command steps cannot configure a context requiring command evidence"
                .into(),
        ));
    }
    if let Some((recipe_digest, recipe)) = primary {
        if context
            .required_capabilities
            .iter()
            .any(|cap| cap == agentscm::checks::SOURCE_PATHS_CAPABILITY)
        {
            return Err(Error::Invalid(
                "configure requires a context without a separate source-path recipe".into(),
            ));
        }
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
    }
    // Installation needs two context generations and one policy advance. Reject
    // exhaustion before the first of the existing separate authority transactions.
    if context.change_version > i64::MAX as u64 - 2 || authority.policy_epoch()? >= i64::MAX as u64
    {
        return Err(Error::Limit(
            "workflow configuration generation exhausted".into(),
        ));
    }
    context.change_version += 1;
    // Configuration is an explicit operator rebind to the current global epoch;
    // an unrelated target's installation may have advanced it since this Change.
    context.policy_epoch = authority.policy_epoch()?;
    context.validate()?;
    if let agentscm::context::ContextReadiness::Incomplete { reasons } =
        context.readiness(agentscm::context::SUPPORTED_CAPABILITIES)
    {
        return Err(Error::Invalid(format!(
            "workflow context is incomplete: {}",
            reasons.join("; ")
        )));
    }
    definition.source.policy_epoch = context.policy_epoch;
    for step in &mut definition.steps {
        if let StepAction::Command {
            change,
            change_version,
            context_digest,
            ..
        } = &mut step.action
        {
            *change = context.change.clone();
            *change_version = context.change_version;
            *context_digest = context.digest()?;
        }
    }
    // Leave room for the requirement wrapper and normalized subject bindings.
    if serde_json::to_vec(&definition)?.len() > 255 * 1024 {
        return Err(Error::Limit(
            "configured workflow definition exceeds 255 KiB".into(),
        ));
    }
    authority.update_change_context(state.version, &context)?;
    let published_version = context.change_version;
    // These established authority operations are separate transactions. A crash,
    // I/O failure or concurrent administrator can interrupt installation; never
    // claim that a partially completed configuration was atomic or rolled back.
    let digest = (|| -> Result<String> {
        let plan = skill_workflow::compile(data_dir, &definition)?;
        let requirement = WorkflowRequirement::from_plan(&context.target, &plan)?;
        let digest = authority.install_workflow_requirement(&requirement)?;
        context.change_version += 1;
        context.policy_epoch = authority.policy_epoch()?;
        authority.update_change_context(context.change_version - 1, &context)?;
        Ok(digest)
    })().map_err(|error| Error::Conflict(format!(
        "workflow configuration interrupted after publishing Change version {published_version}: {error}. Inspect the current Change and target requirement; if installed, run skill-workflow check to refresh its context; otherwise retry configuration after resolving the error"
    )))?;
    Ok(serde_json::json!({
        "requirement_digest": digest, "context": context,
        "next_action": "Run skill-workflow check for the native Change with the trusted backends required by its selected steps.",
    }))
}

fn advance(
    data_dir: &Path,
    run_id: &str,
    max_steps: usize,
    docker: Option<PathBuf>,
    docker_socket: Option<PathBuf>,
    action_profile: Option<PathBuf>,
) -> Result<RunView> {
    #[cfg(unix)]
    if let Some(path) = action_profile {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        if docker.is_some() || docker_socket.is_some() {
            return Err(Error::Invalid(
                "select productive profile separately from native check backends".into(),
            ));
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !path.is_absolute() || !metadata.is_file() || metadata.len() > 64 * 1024 {
            return Err(Error::Invalid(
                "action profile must be a bounded regular operator file at an absolute path".into(),
            ));
        }
        let canonical = std::fs::canonicalize(&path)?;
        // geteuid reads process metadata; no pointer or memory ownership crosses FFI.
        let owner = unsafe { libc::geteuid() };
        if metadata.uid() != owner
            || metadata.permissions().mode() & 0o022 != 0
            || canonical
                .ancestors()
                .any(|parent| std::fs::symlink_metadata(parent.join(".git")).is_ok())
        {
            return Err(Error::Invalid("action profile must be operator-owned, not writable by others, and outside Git checkouts".into()));
        }
        let profile: skill_workflow::action::ActionProfile =
            serde_json::from_slice(&std::fs::read(canonical)?)?;
        profile.validate()?;
        let mut random = [0; 16];
        getrandom::fill(&mut random)
            .map_err(|_| Error::Invalid("cannot create action parent".into()))?;
        let parent = std::env::temp_dir().join(format!(
            "agentscm-actions-{}",
            agentscm::store::digest(&random)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&parent)?;
        let signals = ProductiveSignals::new()?;
        let result = tokio::task::block_in_place(|| {
            skill_workflow::runtime::advance_with_actions(
                data_dir,
                run_id,
                max_steps,
                &skill_workflow::action::ActionExecutor {
                    profile: &profile,
                    work_dir: &parent,
                    cancellation: &signals.cancellation,
                },
            )
        });
        let _ = std::fs::remove_dir_all(parent);
        return result;
    }
    #[cfg(not(unix))]
    if action_profile.is_some() {
        return Err(Error::Invalid(
            "productive action execution requires Unix".into(),
        ));
    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrap {
        #[command(subcommand)]
        command: SkillWorkflowCommand,
    }

    #[test]
    fn stop_hooks_are_not_telemetry_actions() {
        let local = [
            "C",
            "--workspace",
            "/w",
            "--docker",
            "/d",
            "--docker-socket",
            "/s",
            "--claude",
            "/c",
        ];
        let remote = [
            "--endpoint",
            "https://h.example",
            "--repository",
            "o/r",
            "--profile",
            "p",
            "--token-file",
            "/t",
            "--workspace",
            "/w",
            "--change",
            "c",
        ];
        let task = [
            "C",
            "--endpoint",
            "https://h.example",
            "--repository",
            "o/r",
            "--profile",
            "p",
            "--token-file",
            "/t",
            "--workspace",
            "/w",
        ];
        for (name, args) in [
            ("claude-stop", &local[..]),
            ("codex-stop", &local[..]),
            ("remote-stop", &remote[..]),
            ("remote-codex-stop", &remote[..]),
            ("task-stop", &task[..]),
            ("task-codex-stop", &task[..]),
        ] {
            let wrap = Wrap::try_parse_from(
                std::iter::once("x")
                    .chain([name])
                    .chain(args.iter().copied()),
            )
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(!telemetry_action(&wrap.command), "{name}");
        }
    }
}
