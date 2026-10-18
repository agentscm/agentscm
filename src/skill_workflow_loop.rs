//! One bounded native checkpoint -> workflow -> feedback loop for Claude Code.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use agentscm::authority::Authority;
use agentscm::authority::workflows::WorkflowRequirement;
use agentscm::context::{BaseState, ChangeId, ReviewContext};
use agentscm::execution::Cancellation;
use agentscm::model::{LandingPreparation, PrepareLandingRequest, TargetState};
use agentscm::reviewer::ReviewCancellation;
use agentscm::skill_workflow::runtime::{ClaudeExecutor, CommandExecutor};
use agentscm::skill_workflow::{self, RunState, RunView, StepState};
use agentscm::store::Engine;
use agentscm::workspace::{OperationKind, OperationOutcome, Workspace};
use agentscm::{Error, Result};
use clap::Args;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_WORKSPACE_CHANGES: usize = 4_096;
const MAX_HOOK_INPUT_BYTES: u64 = 64 * 1024;
const MAX_HOOK_REASON_BYTES: usize = 16 * 1024;
const MAX_FAILURE_DETAIL_BYTES: usize = 4 * 1024;
const MAX_COMMAND_OUTPUT_BYTES: usize = 8 * 1024;

#[derive(Debug, Args)]
pub struct LoopArgs {
    pub change: String,
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    #[arg(long)]
    pub docker: PathBuf,
    #[arg(long)]
    pub docker_socket: PathBuf,
    #[arg(long)]
    pub claude: PathBuf,
}

pub fn check(data_dir: &Path, args: LoopArgs) -> Result<Value> {
    let engine = Engine::open(data_dir)?;
    let mut authority = Authority::open(data_dir)?;
    let change_id = ChangeId {
        repository: authority.repository_id()?,
        id: args.change.clone(),
    };
    let state = authority.change(&change_id)?;
    let mut context = authority.review_context(&state.context_digest)?;
    let current_policy = authority.policy_epoch()?;
    let target = authority.target(&context.target)?;

    let mut workspace = args.workspace.as_ref().map(Workspace::open).transpose()?;
    let mut workspace_binding = None;
    let source_tip = if let Some(workspace) = workspace.as_mut() {
        let before = workspace.state()?;
        if before.change != change_id {
            return Err(Error::Conflict(
                "native workspace belongs to a different Change".into(),
            ));
        }
        if !before.ready {
            return Err(Error::Conflict(
                "native workspace is not ready for a workflow checkpoint".into(),
            ));
        }
        let checkpoint_operation = stable_id(
            "loop-checkpoint-",
            &[
                args.change.as_bytes(),
                &before.generation.to_be_bytes(),
                state.context_digest.as_bytes(),
            ],
        );
        if let Some(pending) = &before.pending {
            if pending.kind != OperationKind::Checkpoint
                || pending.operation_id != checkpoint_operation
            {
                return Err(Error::Conflict(
                    "native workspace has a different operation pending recovery".into(),
                ));
            }
            let outcome =
                workspace.checkpoint(&engine, &checkpoint_operation, before.generation)?;
            let OperationOutcome::Checkpointed {
                revision,
                generation,
            } = outcome
            else {
                return Err(Error::Conflict(
                    "native workspace checkpoint recovery did not complete".into(),
                ));
            };
            workspace_binding = Some((generation, revision.clone()));
            revision
        } else {
            let status = workspace.status(&engine, MAX_WORKSPACE_CHANGES)?;
            if !status.complete {
                return Err(Error::Limit(
                    "native workspace scan exceeds the supported complete change set".into(),
                ));
            }
            if status.changes.is_empty() {
                workspace_binding = Some((before.generation, before.base_revision.clone()));
                before.base_revision
            } else {
                let outcome =
                    workspace.checkpoint(&engine, &checkpoint_operation, before.generation)?;
                let OperationOutcome::Checkpointed {
                    revision,
                    generation,
                } = outcome
                else {
                    return Err(Error::Conflict(
                        "native workspace checkpoint did not complete".into(),
                    ));
                };
                workspace_binding = Some((generation, revision.clone()));
                revision
            }
        }
    } else {
        context.revision.clone()
    };

    let prepared = if let Some(target) = target.as_ref() {
        context.policy_epoch == current_policy
            && context.base.as_ref() == Some(&base_state(target))
            && prepared_for_source(&engine, &context, &source_tip, target, workspace.is_some())?
    } else {
        false
    };
    if !prepared {
        context = refresh_context(
            &mut authority,
            context,
            &source_tip,
            target.as_ref(),
            current_policy,
        )?;
        if let Some(target) = target.as_ref() {
            context = prepare_landing(&mut authority, &engine, context, &source_tip, target)?;
        }
    }

    let requirement = authority
        .workflow_requirement(&context.target)?
        .ok_or_else(|| {
            Error::Conflict(format!(
                "target {} has no installed workflow requirement",
                context.target
            ))
        })?;
    let definition = requirement.instantiate(&context)?;
    let plan = skill_workflow::compile(data_dir, &definition)?;
    if WorkflowRequirement::from_plan(&context.target, &plan)? != requirement {
        return Err(Error::Conflict(
            "current source does not match the installed workflow skill and recipes".into(),
        ));
    }
    let context_digest = context.digest()?;
    let run_id = stable_run_id(&requirement.digest()?, &context_digest);
    skill_workflow::start(data_dir, &run_id, &plan)?;

    let work_dir = crate::private_execution_dir(data_dir)?;
    let Some((_docker_config, backend)) = crate::optional_backend(
        Some(args.docker.clone()),
        Some(args.docker_socket.clone()),
        &work_dir,
    )?
    else {
        return Err(Error::Invalid(
            "--docker and --docker-socket are required".into(),
        ));
    };
    let claude_dir = crate::PrivateExecutionDirectory::create(&work_dir)?;
    let command_cancellation = Cancellation::default();
    let review_cancellation = ReviewCancellation::default();
    let signals = signal_bridge(command_cancellation.clone(), review_cancellation.clone())?;
    let command = CommandExecutor {
        backend: &backend,
        work_dir: &work_dir,
        cancellation: &command_cancellation,
    };
    let claude = ClaudeExecutor {
        executable: &args.claude,
        work_dir: &claude_dir.0,
        cancellation: &review_cancellation,
    };
    let advanced = tokio::task::block_in_place(|| {
        skill_workflow::runtime::advance_with_backends(
            data_dir,
            &run_id,
            32,
            Some(&command),
            Some(&claude),
        )
    });
    signals.abort();
    let run = match advanced {
        Ok(run) => run,
        Err(error) => match skill_workflow::status(data_dir, &run_id) {
            Ok(run)
                if matches!(
                    run.state,
                    RunState::Succeeded
                        | RunState::CompletedWithOmissions
                        | RunState::Failed
                        | RunState::Blocked
                        | RunState::Cancelled
                ) =>
            {
                run
            }
            _ => return Err(error),
        },
    };

    let workspace_changed = if let (Some(workspace), Some((generation, revision))) =
        (workspace.as_ref(), workspace_binding.as_ref())
    {
        let after = workspace.state()?;
        let status = workspace.status(&engine, MAX_WORKSPACE_CHANGES)?;
        after.change != change_id
            || !after.ready
            || after.pending.is_some()
            || after.generation != *generation
            || after.base_revision != *revision
            || !status.complete
            || !status.changes.is_empty()
    } else {
        false
    };

    let failures = failure_feedback(&authority, &run)?;
    let receipt = if !workspace_changed
        && matches!(
            run.state,
            RunState::Succeeded | RunState::CompletedWithOmissions
        )
        && run
            .steps
            .iter()
            .all(|step| !step.required || step.state == StepState::Succeeded)
    {
        Some(skill_workflow::runtime::finalize(
            data_dir, &run_id, &context,
        )?)
    } else {
        None
    };
    let next_action = if workspace_changed {
        "Workspace bytes changed during verification. Checkpoint the new bytes and run the workflow again."
    } else if receipt.is_some() {
        "The exact current revision passed its required workflow. It may now be reviewed and approved for native landing."
    } else {
        "Fix the reported failures, checkpoint the changed bytes, and run the workflow again for the fresh revision."
    };
    Ok(json!({
        "context": context,
        "run": run,
        "receipt": receipt,
        "workspace_changed_during_run": workspace_changed,
        "failures": failures,
        "next_action": next_action,
    }))
}

pub fn claude_stop(data_dir: &Path, args: LoopArgs) -> Result<()> {
    let payload = read_hook_payload()?;
    if payload.get("hook_event_name").and_then(Value::as_str) != Some("Stop") {
        return Err(Error::Invalid(
            "Claude workflow hook expects hook_event_name Stop".into(),
        ));
    }
    if args.workspace.is_none() {
        return Err(Error::Invalid(
            "Claude Stop workflow hook requires --workspace".into(),
        ));
    }
    let result = match check(data_dir, args) {
        Ok(result) => result,
        Err(error) => json!({
            "failures": [{"detail": bounded_text(&error.to_string(), MAX_FAILURE_DETAIL_BYTES)}],
            "next_action": "Resolve the workflow setup or execution error, then try to finish again.",
        }),
    };
    if result
        .get("receipt")
        .is_some_and(|receipt| !receipt.is_null())
        && result
            .get("workspace_changed_during_run")
            .and_then(Value::as_bool)
            != Some(true)
    {
        println!("{{}}");
        return Ok(());
    }
    let feedback = json!({
        "failures": result.get("failures").cloned().unwrap_or_else(|| json!([])),
        "next_action": result.get("next_action").cloned().unwrap_or_else(||
            json!("Fix the workflow failure and run it again for the current bytes.")),
    });
    let reason = bounded_json(&feedback, MAX_HOOK_REASON_BYTES)?;
    println!("{}", json!({"decision":"block","reason":reason}));
    Ok(())
}

fn refresh_context(
    authority: &mut Authority,
    mut context: ReviewContext,
    revision: &str,
    target: Option<&TargetState>,
    policy_epoch: u64,
) -> Result<ReviewContext> {
    let desired_base = target.map(base_state);
    if context.revision == revision
        && context.base == desired_base
        && context.policy_epoch == policy_epoch
    {
        return Ok(context);
    }
    let expected = context.change_version;
    context.change_version = expected
        .checked_add(1)
        .ok_or_else(|| Error::Limit("change version exhausted".into()))?;
    context.revision = revision.into();
    context.base = desired_base;
    context.policy_epoch = policy_epoch;
    let state = authority.update_change_context(expected, &context)?;
    authority.review_context(&state.context_digest)
}

fn prepare_landing(
    authority: &mut Authority,
    engine: &Engine,
    context: ReviewContext,
    source_tip: &str,
    target: &TargetState,
) -> Result<ReviewContext> {
    if source_tip == target.revision.as_str() {
        return Ok(context);
    }
    let mut landing_context = context.clone();
    landing_context.change_version = context
        .change_version
        .checked_add(1)
        .ok_or_else(|| Error::Limit("change version exhausted".into()))?;
    landing_context.revision = source_tip.into();
    landing_context.base = Some(base_state(target));
    let operation = stable_id(
        "loop-landing-",
        &[
            context.digest()?.as_bytes(),
            source_tip.as_bytes(),
            target.revision.as_bytes(),
            &target.version.to_be_bytes(),
        ],
    );
    match authority.prepare_landing(
        engine,
        &PrepareLandingRequest {
            operation_id: operation,
            target: context.target.clone(),
            expected_target: target.clone(),
            source_tip: source_tip.into(),
            change_version: context.change_version,
            context: landing_context,
        },
    )? {
        LandingPreparation::Prepared { change, .. } => {
            authority.review_context(&change.context_digest)
        }
        LandingPreparation::AlreadyCurrent { .. } => Ok(context),
    }
}

fn prepared_for_source(
    engine: &Engine,
    context: &ReviewContext,
    source_tip: &str,
    target: &TargetState,
    has_workspace: bool,
) -> Result<bool> {
    if source_tip == target.revision.as_str() && context.revision.as_str() == source_tip {
        return Ok(true);
    }
    let revision = engine.load_revision(&context.revision)?;
    if revision.parents.first() != Some(&target.revision) {
        return Ok(false);
    }
    if !has_workspace {
        return Ok(true);
    }
    let source = engine.load_revision(source_tip)?;
    Ok(revision.tree == source.tree
        && revision.parents.get(1).map(String::as_str) == Some(source_tip))
}

fn base_state(target: &TargetState) -> BaseState {
    BaseState {
        revision: target.revision.clone(),
        target_version: target.version,
    }
}

fn failure_feedback(authority: &Authority, run: &RunView) -> Result<Vec<Value>> {
    let mut failures = Vec::new();
    for step in &run.steps {
        if step.state == StepState::Succeeded {
            continue;
        }
        let mut failure = json!({
            "step_id": step.id,
            "required": step.required,
            "state": step.state,
            "detail": step.detail.as_deref().map(|detail|
                bounded_text(detail, MAX_FAILURE_DETAIL_BYTES)),
        });
        if let Some(review) = &step.review {
            failure["review"] = serde_json::to_value(review)?;
        }
        if let Some(record) = step
            .execution
            .as_ref()
            .and_then(|execution| execution.record_digest.as_deref())
            .map(|digest| authority.command_record(digest))
            .transpose()?
        {
            let stdout_budget = MAX_COMMAND_OUTPUT_BYTES / 2;
            failure["command"] = json!({
                "outcome": record.outcome,
                "exit_code": record.exit_code,
                "stdout": bounded_bytes(&record.stdout, stdout_budget),
                "stderr": bounded_bytes(&record.stderr, MAX_COMMAND_OUTPUT_BYTES - stdout_budget),
                "output_truncated": record.output_truncated
                    || record.stdout.len() > stdout_budget
                    || record.stderr.len() > MAX_COMMAND_OUTPUT_BYTES - stdout_budget,
            });
        }
        failures.push(failure);
    }
    Ok(failures)
}

fn signal_bridge(
    command: Cancellation,
    review: ReviewCancellation,
) -> Result<tokio::task::JoinHandle<()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(tokio::spawn(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        command.cancel();
        review.cancel();
    }))
}

fn read_hook_payload() -> Result<Value> {
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_HOOK_INPUT_BYTES {
        return Err(Error::Limit("Claude Stop hook input exceeds 64 KiB".into()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn bounded_json(value: &Value, maximum: usize) -> Result<String> {
    let encoded = serde_json::to_string(value)?;
    Ok(bounded_text(&encoded, maximum))
}

fn bounded_text(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.into();
    }
    let mut end = maximum.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &value[..end])
}

fn bounded_bytes(value: &[u8], maximum: usize) -> String {
    bounded_text(&String::from_utf8_lossy(value), maximum)
}

fn stable_run_id(requirement: &str, context: &str) -> String {
    let digest = stable_hash(&[requirement.as_bytes(), context.as_bytes()]);
    format!("loop-{}", &digest[..59])
}

fn stable_id(prefix: &str, fields: &[&[u8]]) -> String {
    format!("{prefix}{}", stable_hash(fields))
}

fn stable_hash(fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.workflow-loop.v1\0");
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}
