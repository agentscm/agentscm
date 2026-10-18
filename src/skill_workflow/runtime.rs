//! Durable trusted-local execution for compiled skill workflows.
//!
//! The journal records intent before each step and its terminal observation in
//! a later transaction. Explicit command actions reuse the authority's durable
//! command attempts; skill prose cannot authorize execution or admission.

use std::collections::BTreeMap;
use std::fs::{OpenOptions, symlink_metadata};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::compiler::{CompiledPlan, compile, validate_retained_definition, verify_current};
use super::contracts::{StepAction, StepDefinition, WorkflowDefinition, WorkflowSource};
#[cfg(unix)]
use crate::authority::Authority;
use crate::checks::PathObservation;
#[cfg(unix)]
use crate::execution::{
    Cancellation, CommandAttempt, CommandCheckRequest, CommandState, ExecutionOutcome, OciBackend,
};
use crate::lease::Lease;
use crate::store::{Engine, create_directory_durable};
use crate::{Error, Result};

#[path = "review.rs"]
mod review;
pub use review::{ClaudeReviewFinding, ClaudeReviewView};

const VIEW_VERSION: u32 = 1;
const APPLICATION_ID: i64 = 0x4153_5746;
const SCHEMA_VERSION: i64 = 2;
const MAX_RUN_ID_BYTES: usize = 64;
const MAX_DEFINITION_BYTES: usize = 256 * 1024;
const MAX_DETAIL_BYTES: usize = 4 * 1024;
const MAX_OBSERVATIONS_BYTES: usize = 1024 * 1024;
const MAX_ADVANCE_STEPS: usize = 32;
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ENCODED_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const OPERATION_DOMAIN: &[u8] = b"agentscm.skill-workflow.step-operation.v1\0";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Pending,
    Running,
    Succeeded,
    CompletedWithOmissions,
    Failed,
    Blocked,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StepView {
    pub id: String,
    #[serde(default)]
    pub needs: Vec<String>,
    pub required: bool,
    pub state: StepState,
    pub observations: Vec<PathObservation>,
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<CommandStepView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ClaudeReviewView>,
}

/// A projection of the retained authority attempt, never worker-supplied evidence.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandStepView {
    pub attempt_id: String,
    pub generation: u64,
    pub state: String,
    pub outcome: Option<String>,
    pub record_digest: Option<String>,
}

#[cfg(unix)]
pub struct CommandExecutor<'a> {
    pub backend: &'a OciBackend,
    pub work_dir: &'a Path,
    pub cancellation: &'a Cancellation,
}

#[cfg(unix)]
pub struct ClaudeExecutor<'a> {
    pub executable: &'a Path,
    pub work_dir: &'a Path,
    pub cancellation: &'a crate::reviewer::ReviewCancellation,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunView {
    pub version: u32,
    pub run_id: String,
    pub plan_digest: String,
    pub skill_digest: String,
    pub source: WorkflowSource,
    pub state: RunState,
    pub steps: Vec<StepView>,
    pub admission: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunSummary {
    pub run_id: String,
    pub name: String,
    pub sequence: String,
    pub revision: String,
    pub state: RunState,
}

struct Journal {
    data_dir: PathBuf,
    connection: Connection,
    _access: crate::local_recovery::AccessGuard,
    _lease: Lease,
}

struct StoredRun {
    definition: WorkflowDefinition,
    ordered_steps: Vec<StepDefinition>,
    plan_digest: String,
    skill_digest: String,
    state: RunState,
}

struct StepResult {
    state: StepState,
    observations: Vec<PathObservation>,
    detail: String,
}

impl StepResult {
    fn encoded_budget_failure() -> Self {
        Self {
            state: StepState::Blocked,
            observations: Vec::new(),
            detail: "source-path step exceeded its 16 MiB encoded-object read limit".into(),
        }
    }
}

#[derive(Clone, Copy)]
enum OpenMode {
    Create,
    Write,
    Read,
}

pub fn start(data_dir: &Path, run_id: &str, plan: &CompiledPlan) -> Result<RunView> {
    validate_run_id(run_id)?;
    let definition_json = serde_json::to_vec(plan.definition())?;
    if definition_json.len() > MAX_DEFINITION_BYTES {
        return Err(Error::Limit(format!(
            "workflow definition exceeds {MAX_DEFINITION_BYTES} bytes"
        )));
    }
    let existing_database = data_dir.join("skill-workflows/state.sqlite");
    match symlink_metadata(&existing_database) {
        Ok(_) => {
            let journal = Journal::open(data_dir, OpenMode::Read)?;
            if let Some(existing) = load_stored_run(&journal.connection, run_id)? {
                ensure_same_plan(run_id, &existing, plan)?;
                return journal.view(run_id);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    verify_current(data_dir, plan)?;
    let mut journal = Journal::open(data_dir, OpenMode::Create)?;
    let tx = journal
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(existing) = load_stored_run(&tx, run_id)? {
        ensure_same_plan(run_id, &existing, plan)?;
        tx.commit()?;
        return journal.view(run_id);
    }
    tx.execute(
        "INSERT INTO workflow_runs(run_id,plan_digest,skill_digest,definition_json,state) \
         VALUES(?1,?2,?3,?4,'pending')",
        params![run_id, plan.digest(), plan.skill_digest(), definition_json],
    )?;
    for (position, step) in plan.ordered_steps().iter().enumerate() {
        tx.execute(
            "INSERT INTO workflow_steps(\
                run_id,position,step_id,required,state,operation_id,generation,observations_json,detail\
             ) VALUES(?1,?2,?3,?4,'pending',?5,0,?6,NULL)",
            params![
                run_id,
                sql_i64("step position", position)?,
                step.id,
                step.required,
                operation_id(run_id, &step.id),
                b"[]".as_slice(),
            ],
        )?;
    }
    tx.commit()?;
    journal.view(run_id)
}

fn ensure_same_plan(run_id: &str, stored: &StoredRun, plan: &CompiledPlan) -> Result<()> {
    if stored.plan_digest != plan.digest()
        || stored.skill_digest != plan.skill_digest()
        || stored.definition != *plan.definition()
    {
        return Err(Error::Conflict(format!(
            "workflow run ID {run_id} is already bound to a different plan"
        )));
    }
    Ok(())
}

/// Domain-separated identity prevents another workflow or plan sharing an attempt.
pub fn command_attempt_id(run_id: &str, plan_digest: &str, step_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.skill-workflow.command.v1\0");
    for field in [run_id, plan_digest, step_id] {
        hash_field(&mut hasher, field.as_bytes());
    }
    format!("workflow-{:x}", hasher.finalize())
}

#[cfg(unix)]
fn command_request(
    run_id: &str,
    plan: &CompiledPlan,
    step: &StepDefinition,
) -> Result<CommandCheckRequest> {
    let StepAction::Command {
        change,
        change_version,
        context_digest,
        ..
    } = &step.action
    else {
        return Err(Error::Invalid("step is not a command".into()));
    };
    Ok(CommandCheckRequest {
        attempt_id: command_attempt_id(run_id, plan.digest(), &step.id),
        change: change.clone(),
        change_version: *change_version,
        context_digest: context_digest.clone(),
        revision: plan.definition().source.revision.clone(),
        recipe: plan
            .command_recipe(&step.id)
            .ok_or_else(|| Error::Invalid("compiled command recipe missing".into()))?
            .clone(),
    })
}

#[cfg(unix)]
fn command_result(attempt: &CommandAttempt) -> Result<StepResult> {
    if attempt.state != CommandState::Finished || attempt.record_digest.is_none() {
        return Err(Error::Conflict(
            "command has no terminal execution record; recovery required".into(),
        ));
    }
    let outcome = attempt
        .outcome
        .ok_or_else(|| Error::Invalid("finished command is missing outcome".into()))?;
    let state = match outcome {
        ExecutionOutcome::Success => StepState::Succeeded,
        ExecutionOutcome::Failed | ExecutionOutcome::TimedOut | ExecutionOutcome::OutputLimit => {
            StepState::Failed
        }
        ExecutionOutcome::Cancelled => StepState::Cancelled,
        ExecutionOutcome::Unavailable
        | ExecutionOutcome::Incomplete
        | ExecutionOutcome::Indeterminate => StepState::Blocked,
    };
    Ok(StepResult {
        state,
        observations: Vec::new(),
        detail: format!(
            "command {}: {}; record {}",
            attempt.attempt_id,
            serde_json::to_value(outcome)?.as_str().unwrap_or("unknown"),
            attempt.record_digest.as_deref().expect("checked record")
        ),
    })
}

fn command_projection(
    data_dir: &Path,
    run_id: &str,
    run: &StoredRun,
    step: &StepDefinition,
    state: StepState,
) -> Result<Option<CommandStepView>> {
    let StepAction::Command {
        change,
        change_version,
        context_digest,
        ..
    } = &step.action
    else {
        return Ok(None);
    };
    #[cfg(not(unix))]
    {
        let _ = (
            data_dir,
            run_id,
            run,
            state,
            change,
            change_version,
            context_digest,
        );
        Err(Error::Invalid(
            "command workflow history requires Unix authority support".into(),
        ))
    }
    #[cfg(unix)]
    {
        let authority = Authority::open_read_only(data_dir)?;
        let id = command_attempt_id(run_id, &run.plan_digest, &step.id);
        let attempt = match authority.command_attempt(&id) {
            Ok(attempt) => attempt,
            Err(Error::NotFound(_))
                if !matches!(state, StepState::Succeeded | StepState::Failed) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if attempt.subject.change != *change
            || attempt.subject.change_version != *change_version
            || attempt.subject.repository != run.definition.source.repository
            || attempt.subject.context_digest != *context_digest
            || attempt.subject.revision != run.definition.source.revision
        {
            return Err(Error::Invalid(
                "workflow command attempt does not match its retained definition".into(),
            ));
        }
        if let Some(digest) = &attempt.record_digest {
            let record = authority.command_record(digest)?;
            // Recovery records retain the execution generation whose custody
            // was reconciled. The terminal attempt has a later fencing generation.
            // Observed command results must instead match the generation exactly.
            let fenced_recovery = !record.isolation_observed
                && matches!(
                    record.outcome,
                    ExecutionOutcome::Cancelled
                        | ExecutionOutcome::Incomplete
                        | ExecutionOutcome::Indeterminate
                )
                && record.request.run_generation < attempt.run_generation;
            if record.request.attempt_id != id
                || record.request.subject != attempt.subject
                || Some(record.outcome) != attempt.outcome
                || (record.request.run_generation != attempt.run_generation && !fenced_recovery)
            {
                return Err(Error::Invalid(
                    "workflow command record does not match its attempt".into(),
                ));
            }
        }
        if !matches!(state, StepState::Pending | StepState::Running)
            && command_result(&attempt)?.state != state
        {
            return Err(Error::Invalid(
                "workflow command state does not match observed execution".into(),
            ));
        }
        Ok(Some(CommandStepView {
            attempt_id: id,
            generation: attempt.run_generation,
            state: serde_json::to_value(attempt.state)?
                .as_str()
                .unwrap_or("unknown")
                .into(),
            outcome: attempt
                .outcome
                .map(|outcome| {
                    serde_json::to_value(outcome)
                        .map(|value| value.as_str().unwrap_or("unknown").to_owned())
                })
                .transpose()?,
            record_digest: attempt.record_digest,
        }))
    }
}

/// Fence and reconcile a running command. Recovery never retries its effect.
#[cfg(unix)]
pub fn recover_command(
    data_dir: &Path,
    run_id: &str,
    step_id: &str,
    executor: &CommandExecutor<'_>,
) -> Result<RunView> {
    validate_run_id(run_id)?;
    let mut journal = Journal::open(data_dir, OpenMode::Write)?;
    let current = journal.view(run_id)?;
    let stored = journal.stored_run(run_id)?;
    let step = stored
        .ordered_steps
        .iter()
        .find(|step| step.id == step_id && matches!(step.action, StepAction::Command { .. }))
        .ok_or_else(|| Error::Invalid("recovery requires a declared command step".into()))?;
    if !current
        .steps
        .iter()
        .any(|view| view.id == step_id && view.state == StepState::Running)
    {
        return Err(Error::Conflict(
            "only a running workflow command needs recovery".into(),
        ));
    }
    let mut authority = Authority::open(data_dir)?;
    let id = command_attempt_id(run_id, &stored.plan_digest, step_id);
    let attempt = authority.command_attempt(&id)?;
    let attempt = match attempt.state {
        CommandState::Finished => attempt,
        CommandState::Pending => authority.cancel_command_attempt(
            Some(executor.backend),
            executor.work_dir,
            &id,
            attempt.run_generation,
            "workflow recovered before execution",
        )?,
        CommandState::Running => authority.recover_command_attempt(
            Some(executor.backend),
            executor.work_dir,
            &id,
            attempt.run_generation,
            false,
            "workflow command recovered by operator",
        )?,
    };
    journal.finish_step(run_id, step, command_result(&attempt)?)?;
    journal.refresh_run_state(run_id)?;
    journal.view(run_id)
}

pub fn advance(data_dir: &Path, run_id: &str, max_steps: usize) -> Result<RunView> {
    advance_inner(
        data_dir,
        run_id,
        max_steps,
        #[cfg(unix)]
        None,
        #[cfg(unix)]
        None,
        None,
    )
}

#[cfg(unix)]
pub fn advance_with_executor(
    data_dir: &Path,
    run_id: &str,
    max_steps: usize,
    executor: &CommandExecutor<'_>,
) -> Result<RunView> {
    advance_inner(data_dir, run_id, max_steps, Some(executor), None, None)
}

#[cfg(unix)]
pub fn advance_with_backends(
    data_dir: &Path,
    run_id: &str,
    max_steps: usize,
    command: Option<&CommandExecutor<'_>>,
    claude: Option<&ClaudeExecutor<'_>>,
) -> Result<RunView> {
    advance_inner(data_dir, run_id, max_steps, command, claude, None)
}

/// A server's current authorization check. Called before each external step and
/// each model call reservation; it cannot select or modify the compiled plan.
pub type DispatchGuard<'a> = dyn Fn() -> Result<()> + Send + Sync + 'a;

#[cfg(unix)]
pub fn advance_guarded(
    data_dir: &Path,
    run_id: &str,
    max_steps: usize,
    command: Option<&CommandExecutor<'_>>,
    claude: Option<&ClaudeExecutor<'_>>,
    guard: &DispatchGuard<'_>,
) -> Result<RunView> {
    advance_inner(data_dir, run_id, max_steps, command, claude, Some(guard))
}

fn advance_inner(
    data_dir: &Path,
    run_id: &str,
    max_steps: usize,
    #[cfg(unix)] executor: Option<&CommandExecutor<'_>>,
    #[cfg(unix)] claude: Option<&ClaudeExecutor<'_>>,
    guard: Option<&DispatchGuard<'_>>,
) -> Result<RunView> {
    validate_run_id(run_id)?;
    if max_steps > MAX_ADVANCE_STEPS {
        return Err(Error::Limit(format!(
            "workflow advance supports at most {MAX_ADVANCE_STEPS} steps"
        )));
    }
    let mut journal = Journal::open(data_dir, OpenMode::Write)?;
    let stored = journal.stored_run(run_id)?;
    if is_terminal_run(stored.state) {
        return journal.view(run_id);
    }
    let retained = journal.view(run_id)?;
    if retained
        .steps
        .iter()
        .all(|step| !matches!(step.state, StepState::Pending | StepState::Running))
    {
        journal.refresh_run_state(run_id)?;
        return journal.view(run_id);
    }

    // A persisted definition is untrusted historical input. Recompile it from
    // the current native source and authority rather than deserializing a plan.
    let plan = compile(data_dir, &stored.definition)?;
    if plan.digest() != stored.plan_digest || plan.skill_digest() != stored.skill_digest {
        return Err(Error::Conflict(
            "persisted workflow plan no longer recompiles to its recorded identity".into(),
        ));
    }
    if max_steps == 0 {
        return journal.view(run_id);
    }

    journal.set_run_state(run_id, RunState::Running)?;
    let engine = Engine::open(data_dir)?;
    for _ in 0..max_steps {
        let current = journal.view(run_id)?;
        let states = current
            .steps
            .iter()
            .map(|step| (step.id.as_str(), step.state))
            .collect::<BTreeMap<_, _>>();
        let Some(step) = plan.ordered_steps().iter().find(|step| {
            matches!(
                states.get(step.id.as_str()),
                Some(StepState::Pending | StepState::Running)
            )
        }) else {
            break;
        };

        let unsuccessful = step.needs.iter().find(|dependency| {
            matches!(
                states.get(dependency.as_str()),
                Some(StepState::Failed | StepState::Blocked | StepState::Cancelled)
            )
        });
        if let Some(dependency) = unsuccessful {
            journal.begin_step(run_id, &step.id)?;
            journal.finish_step(
                run_id,
                step,
                StepResult {
                    state: StepState::Blocked,
                    observations: Vec::new(),
                    detail: format!("dependency {dependency} did not succeed"),
                },
            )?;
            journal.refresh_run_state(run_id)?;
            continue;
        }
        if step.needs.iter().any(|dependency| {
            !matches!(states.get(dependency.as_str()), Some(StepState::Succeeded))
        }) {
            return Err(Error::Invalid(format!(
                "workflow step {} was ordered before an unfinished dependency",
                step.id
            )));
        }

        if let Some(guard) = guard {
            guard()?;
        }

        #[cfg(unix)]
        if matches!(step.action, StepAction::ClaudeCodeReview { .. }) {
            let backend = claude.ok_or_else(|| {
                Error::Invalid(
                    "Claude review requires an operator-selected --claude executable".into(),
                )
            })?;
            let record = if states.get(step.id.as_str()) == Some(&StepState::Running) {
                review::read_record(&journal.connection,run_id,&step.id)?.ok_or_else(||
                    Error::Conflict("review dispatch was interrupted; cancel this run and explicitly refresh the context, never replay the call".into()))?
            } else {
                journal.begin_step(run_id, &step.id)?;
                drop(journal);
                let result = review::execute(data_dir, run_id, &plan, &step.id, backend, guard);
                journal = Journal::open_after_dispatch(data_dir)?;
                match result {
                    Ok(record) => record,
                    Err(error) => {
                        let current = journal.view(run_id)?;
                        if current
                            .steps
                            .iter()
                            .any(|view| view.id == step.id && view.state == StepState::Running)
                        {
                            journal.finish_step(run_id,step,StepResult {state:StepState::Blocked,observations:Vec::new(),
                                detail:"Claude review did not complete; inspect local diagnostics and refresh context before retrying".into()})?;
                            journal.refresh_run_state(run_id)?;
                        }
                        return Err(error);
                    }
                }
            };
            let current = journal.view(run_id)?;
            if current
                .steps
                .iter()
                .any(|view| view.id == step.id && view.state == StepState::Running)
            {
                journal.finish_step(
                    run_id,
                    step,
                    StepResult {
                        state: review::state(&record),
                        observations: Vec::new(),
                        detail: format!("Claude review: {}", serde_json::to_value(record.stop)?),
                    },
                )?;
                journal.refresh_run_state(run_id)?;
            }
            continue;
        }

        #[cfg(unix)]
        if matches!(step.action, StepAction::Command { .. }) {
            let executor = executor.ok_or_else(|| {
                Error::Invalid("command skill step requires --docker and --docker-socket".into())
            })?;
            let mut authority = Authority::open(data_dir)?;
            let request = command_request(run_id, &plan, step)?;
            // Creation and workflow intent share the journal lease. Cancellation
            // cannot pass between these writes and miss the pending attempt.
            let attempt = authority.create_command_attempt(&request)?;
            if attempt.state == CommandState::Running {
                return Err(Error::Conflict(format!(
                    "workflow command {} is running; inspect it or recover the step, never rerun it",
                    attempt.attempt_id
                )));
            }
            journal.begin_step(run_id, &step.id)?;
            // Do not hold the journal lease during execution: status and a second
            // controller's cancel/recovery must remain available. The authority
            // generation CAS fences concurrent claims and late finishers.
            drop(journal);
            if let Some(guard) = guard {
                guard()?;
            }
            let attempt = authority.run_command_attempt(
                &engine,
                executor.backend,
                executor.work_dir,
                &attempt.attempt_id,
                attempt.run_generation,
                executor.cancellation,
            )?;
            journal = Journal::open_after_dispatch(data_dir)?;
            let current = journal.view(run_id)?;
            if matches!(
                current
                    .steps
                    .iter()
                    .find(|view| view.id == step.id)
                    .map(|view| view.state),
                Some(StepState::Pending | StepState::Running)
            ) {
                journal.finish_step(run_id, step, command_result(&attempt)?)?;
                journal.refresh_run_state(run_id)?;
            }
            if is_terminal_run(journal.stored_run(run_id)?.state) {
                break;
            }
            continue;
        }
        journal.begin_step(run_id, &step.id)?;
        let result = execute_step(&engine, &plan.definition().source, step);
        journal.finish_step(run_id, step, result)?;
        journal.refresh_run_state(run_id)?;
    }
    journal.refresh_run_state(run_id)?;
    journal.view(run_id)
}

/// Reads retained workflow rows. SQLite may manage existing WAL/SHM sidecars.
pub fn status(data_dir: &Path, run_id: &str) -> Result<RunView> {
    validate_run_id(run_id)?;
    Journal::open(data_dir, OpenMode::Read)?.view(run_id)
}

/// Historical declaration for read-only clients; not an executable plan.
pub fn retained_definition(data_dir: &Path, run_id: &str) -> Result<WorkflowDefinition> {
    validate_run_id(run_id)?;
    Ok(Journal::open(data_dir, OpenMode::Read)?
        .stored_run(run_id)?
        .definition)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunPage {
    pub runs: Vec<RunSummary>,
    pub next_cursor: Option<String>,
}

pub fn list_runs(data_dir: &Path, limit: usize) -> Result<Vec<RunSummary>> {
    Ok(list_run_page(data_dir, limit, None)?.runs)
}

/// Newest-created runs using a bounded keyset cursor, never an unbounded scan.
pub fn list_run_page(data_dir: &Path, limit: usize, before: Option<i64>) -> Result<RunPage> {
    if !(1..=16).contains(&limit) || before.is_some_and(|value| value <= 0) {
        return Err(Error::Limit(
            "workflow list requires 1..16 runs and a positive cursor".into(),
        ));
    }
    let journal = Journal::open(data_dir, OpenMode::Read)?;
    let mut statement = journal.connection.prepare(
        "SELECT run_id,rowid FROM workflow_runs WHERE rowid<=?1 ORDER BY rowid DESC LIMIT ?2",
    )?;
    let mut ids = statement
        .query_map(
            params![before.map_or(i64::MAX, |value| value - 1), limit + 1],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = ids.len() > limit;
    ids.truncate(limit);
    let runs = ids
        .into_iter()
        .map(|(run_id, sequence)| {
            validate_run_id(&run_id)?;
            if sequence <= 0 {
                return Err(Error::Invalid("invalid workflow creation sequence".into()));
            }
            let run = journal.stored_run(&run_id)?;
            Ok(RunSummary {
                run_id,
                name: run.definition.name,
                sequence: sequence.to_string(),
                revision: run.definition.source.revision,
                state: run.state,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let next_cursor = if more {
        runs.last().map(|run| run.sequence.clone())
    } else {
        None
    };
    Ok(RunPage { runs, next_cursor })
}

/// Revalidate the entire retained graph before publishing an authority receipt.
pub fn finalize(
    data_dir: &Path,
    run_id: &str,
    context: &crate::context::ReviewContext,
) -> Result<String> {
    validate_run_id(run_id)?;
    let journal = Journal::open(data_dir, OpenMode::Read)?;
    let stored = journal.stored_run(run_id)?;
    let plan = compile(data_dir, &stored.definition)?;
    let view = journal.view(run_id)?;
    let mut authority = crate::authority::Authority::open(data_dir)?;
    let requirement = authority
        .workflow_requirement(&context.target)?
        .ok_or_else(|| Error::Invalid("target has no installed workflow requirement".into()))?;
    authority.record_workflow_success(&requirement, &plan, &view, context)
}

pub fn cancel(data_dir: &Path, run_id: &str) -> Result<RunView> {
    cancel_inner(
        data_dir,
        run_id,
        #[cfg(unix)]
        None,
    )
}

#[cfg(unix)]
pub fn cancel_with_executor(
    data_dir: &Path,
    run_id: &str,
    executor: &CommandExecutor<'_>,
) -> Result<RunView> {
    cancel_inner(data_dir, run_id, Some(executor))
}

fn cancel_inner(
    data_dir: &Path,
    run_id: &str,
    #[cfg(unix)] executor: Option<&CommandExecutor<'_>>,
) -> Result<RunView> {
    validate_run_id(run_id)?;
    let mut journal = Journal::open(data_dir, OpenMode::Write)?;
    let current = journal.view(run_id)?;
    if is_terminal_run(current.state) {
        return Ok(current);
    }
    if current
        .steps
        .iter()
        .all(|step| !matches!(step.state, StepState::Pending | StepState::Running))
    {
        journal.refresh_run_state(run_id)?;
        return journal.view(run_id);
    }
    #[cfg(unix)]
    {
        let stored = journal.stored_run(run_id)?;
        for (step, view) in stored.ordered_steps.iter().zip(&current.steps) {
            if !matches!(step.action, StepAction::Command { .. })
                || !matches!(view.state, StepState::Pending | StepState::Running)
            {
                continue;
            }
            let mut authority = Authority::open(data_dir)?;
            let id = command_attempt_id(run_id, &stored.plan_digest, &step.id);
            let attempt = match authority.command_attempt(&id) {
                Ok(attempt) => attempt,
                Err(Error::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            let attempt = if attempt.state == CommandState::Finished {
                attempt
            } else {
                if attempt.state == CommandState::Running && executor.is_none() {
                    return Err(Error::Invalid(
                        "cancelling a running workflow command requires --docker and --docker-socket"
                            .into(),
                    ));
                }
                authority.cancel_command_attempt(
                    executor.map(|e| e.backend),
                    executor.map_or(data_dir, |e| e.work_dir),
                    &id,
                    attempt.run_generation,
                    "workflow cancelled by operator",
                )?
            };
            journal.finish_step(run_id, step, command_result(&attempt)?)?;
        }
    }
    let tx = journal
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE workflow_steps SET state='cancelled',detail='run cancelled' \
         WHERE run_id=?1 AND state IN ('pending','running')",
        [run_id],
    )?;
    tx.execute(
        "UPDATE workflow_runs SET state='cancelled' WHERE run_id=?1",
        [run_id],
    )?;
    tx.commit()?;
    journal.view(run_id)
}

impl Journal {
    // External work has already run. Brief contention with cancellation must
    // not discard its observation or cause another dispatch. Only lease
    // acquisition is retried; all state/generation checks still run afterward.
    #[cfg(unix)]
    fn open_after_dispatch(data_dir: &Path) -> Result<Self> {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match Self::open(data_dir, OpenMode::Write) {
                Err(Error::Conflict(message))
                    if message.starts_with("workflow journal is active:")
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => return result,
            }
        }
    }

    fn open(data_dir: &Path, mode: OpenMode) -> Result<Self> {
        let access = crate::local_recovery::AccessGuard::shared_existing(data_dir)?;
        // The operator-selected native root may use a platform alias such as
        // macOS /var -> /private/var. Resolve only that already-guarded root;
        // journal components below remain subject to explicit no-follow checks.
        let canonical_data_dir = std::fs::canonicalize(data_dir)?;
        let directory = canonical_data_dir.join("skill-workflows");
        if matches!(mode, OpenMode::Create) {
            match symlink_metadata(&directory) {
                Ok(metadata) => require_directory_metadata(&directory, &metadata)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    create_directory_durable(&directory)?;
                }
                Err(error) => return Err(error.into()),
            }
            require_directory(&directory)?;
        } else {
            require_directory(&directory)?;
        }
        let lock_path = directory.join("journal.lock");
        if !matches!(mode, OpenMode::Create) {
            require_regular(&lock_path)?;
        } else {
            require_optional_regular(&lock_path)?;
        }
        let mut lock_options = journal_open_options();
        let lock_file = lock_options
            .read(true)
            .write(!matches!(mode, OpenMode::Read))
            .create(matches!(mode, OpenMode::Create))
            .truncate(false)
            .open(&lock_path)?;
        let lease_result = if matches!(mode, OpenMode::Read) {
            Lease::try_shared(lock_file)
        } else {
            Lease::try_exclusive(lock_file)
        };
        let lease = lease_result
            .map_err(|error| Error::Conflict(format!("workflow journal is active: {error}")))?;
        let database = directory.join("state.sqlite");
        if !matches!(mode, OpenMode::Create) {
            require_regular(&database)?;
        } else {
            require_optional_regular(&database)?;
        }
        let flags = if matches!(mode, OpenMode::Read) {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } | if matches!(mode, OpenMode::Create) {
            OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::empty()
        } | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut connection = Connection::open_with_flags(database, flags)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        if matches!(mode, OpenMode::Read) {
            validate_schema(&connection)?;
        } else {
            connection.pragma_update(None, "foreign_keys", true)?;
            connection.pragma_update(None, "journal_mode", "WAL")?;
            connection.pragma_update(None, "synchronous", "FULL")?;
            initialize_schema(&mut connection, matches!(mode, OpenMode::Create))?;
        }
        Ok(Self {
            data_dir: canonical_data_dir,
            connection,
            _access: access,
            _lease: lease,
        })
    }

    fn stored_run(&self, run_id: &str) -> Result<StoredRun> {
        load_stored_run(&self.connection, run_id)?
            .ok_or_else(|| Error::NotFound(format!("workflow run {run_id}")))
    }

    fn view(&self, run_id: &str) -> Result<RunView> {
        let run = self.stored_run(run_id)?;
        let mut statement = self.connection.prepare(
            "SELECT position,step_id,required,state,operation_id,generation,observations_json,detail \
             FROM workflow_steps WHERE run_id=?1 ORDER BY position",
        )?;
        let rows = statement.query_map([run_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Vec<u8>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        let mut steps = Vec::new();
        for (index, row) in rows.enumerate() {
            let (position, id, required, state, stored_operation, generation, observations, detail) =
                row?;
            let Some(declared) = run.ordered_steps.get(index) else {
                return Err(Error::Invalid(
                    "workflow journal has more steps than its retained definition".into(),
                ));
            };
            if position != sql_i64("step position", index)?
                || id != declared.id
                || required != declared.required
                || stored_operation != operation_id(run_id, &id)
                || generation < 0
            {
                return Err(Error::Invalid(
                    "workflow journal step identity does not match its definition".into(),
                ));
            }
            let observations: Vec<PathObservation> = serde_json::from_slice(&observations)?;
            let state = parse_step_state(&state)?;
            validate_step_record(declared, state, &observations, detail.as_deref())?;
            steps.push(StepView {
                id,
                needs: declared.needs.clone(),
                required,
                state,
                observations,
                detail,
                execution: command_projection(&self.data_dir, run_id, &run, declared, state)?,
                review: review_projection(&self.connection, run_id, &run, declared, state)?,
            });
        }
        if steps.len() != run.ordered_steps.len() {
            return Err(Error::Invalid(
                "workflow journal step count does not match its definition".into(),
            ));
        }
        validate_dependency_results(&run.ordered_steps, &steps)?;
        if is_terminal_run(run.state) {
            let consistent = if run.state == RunState::Cancelled {
                steps
                    .iter()
                    .all(|step| !matches!(step.state, StepState::Pending | StepState::Running))
            } else {
                aggregate_state(&steps) == run.state
            };
            if !consistent {
                return Err(Error::Invalid(
                    "terminal workflow run state does not match its step results".into(),
                ));
            }
        }
        Ok(RunView {
            version: VIEW_VERSION,
            run_id: run_id.into(),
            plan_digest: run.plan_digest,
            skill_digest: run.skill_digest,
            source: run.definition.source,
            state: run.state,
            steps,
            admission: "not_assessed".into(),
        })
    }

    fn set_run_state(&mut self, run_id: &str, state: RunState) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "UPDATE workflow_runs SET state=?1 WHERE run_id=?2 AND state IN ('pending','running')",
            params![run_state_name(state), run_id],
        )? != 1
        {
            return Err(Error::Conflict(format!(
                "workflow run {run_id} changed before it could advance"
            )));
        }
        tx.commit()?;
        Ok(())
    }

    fn begin_step(&mut self, run_id: &str, step_id: &str) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE workflow_steps SET state='running',generation=generation+1,\
                    observations_json=?1,detail=NULL \
             WHERE run_id=?2 AND step_id=?3 AND state IN ('pending','running')",
            params![b"[]".as_slice(), run_id, step_id],
        )?;
        if changed != 1 {
            return Err(Error::Conflict(format!(
                "workflow step {step_id} changed before its intent was recorded"
            )));
        }
        tx.commit()?;
        Ok(())
    }

    fn finish_step(
        &mut self,
        run_id: &str,
        step: &StepDefinition,
        result: StepResult,
    ) -> Result<()> {
        validate_step_record(
            step,
            result.state,
            &result.observations,
            Some(&result.detail),
        )?;
        let step_id = &step.id;
        let observations = serde_json::to_vec(&result.observations)?;
        if observations.len() > MAX_OBSERVATIONS_BYTES {
            return Err(Error::Limit(
                "workflow step observations exceed journal limit".into(),
            ));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "UPDATE workflow_steps SET state=?1,observations_json=?2,detail=?3 \
             WHERE run_id=?4 AND step_id=?5 AND state IN ('pending','running')",
            params![
                step_state_name(result.state),
                observations,
                result.detail,
                run_id,
                step_id,
            ],
        )? != 1
        {
            return Err(Error::Conflict(format!(
                "workflow step {step_id} changed before its result was recorded"
            )));
        }
        tx.commit()?;
        Ok(())
    }

    fn refresh_run_state(&mut self, run_id: &str) -> Result<()> {
        let view = self.view(run_id)?;
        if view.state == RunState::Cancelled {
            return Ok(());
        }
        let next = aggregate_state(&view.steps);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE workflow_runs SET state=?1 WHERE run_id=?2 AND state!='cancelled'",
            params![run_state_name(next), run_id],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn initialize_schema(connection: &mut Connection, create: bool) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let application: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application == 0 && version == 0 && create {
        let tables: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if tables != 0 {
            return Err(Error::Invalid(
                "workflow journal contains an unknown schema".into(),
            ));
        }
        tx.execute_batch(
            "CREATE TABLE workflow_runs(\
                run_id TEXT PRIMARY KEY CHECK(length(run_id) BETWEEN 1 AND 64),\
                plan_digest TEXT NOT NULL,\
                skill_digest TEXT NOT NULL,\
                definition_json BLOB NOT NULL CHECK(length(definition_json)<=262144),\
                state TEXT NOT NULL CHECK(state IN ('pending','running','succeeded','completed_with_omissions','failed','blocked','cancelled'))\
             ) STRICT;\
             CREATE TABLE workflow_steps(\
                run_id TEXT NOT NULL REFERENCES workflow_runs(run_id),\
                position INTEGER NOT NULL CHECK(position>=0 AND position<32),\
                step_id TEXT NOT NULL CHECK(length(step_id) BETWEEN 1 AND 64),\
                required INTEGER NOT NULL CHECK(required IN (0,1)),\
                state TEXT NOT NULL CHECK(state IN ('pending','running','succeeded','failed','blocked','cancelled')),\
                operation_id TEXT NOT NULL CHECK(length(operation_id)=64),\
                generation INTEGER NOT NULL CHECK(generation>=0),\
                observations_json BLOB NOT NULL CHECK(length(observations_json)<=1048576),\
                detail TEXT CHECK(detail IS NULL OR length(CAST(detail AS BLOB))<=4096),\
                PRIMARY KEY(run_id,step_id), UNIQUE(run_id,position), UNIQUE(operation_id)\
             ) STRICT;",
        )?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        review::create_schema(&tx)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    } else if application == APPLICATION_ID && version == 1 {
        review::create_schema(&tx)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    } else if application != APPLICATION_ID || version != SCHEMA_VERSION {
        return Err(Error::Invalid(
            "unsupported workflow journal identity or version".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn validate_schema(connection: &Connection) -> Result<()> {
    let application: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application != APPLICATION_ID || !(1..=SCHEMA_VERSION).contains(&version) {
        return Err(Error::Invalid(
            "unsupported workflow journal identity or version".into(),
        ));
    }
    for query in [
        "SELECT run_id,plan_digest,skill_digest,definition_json,state FROM workflow_runs LIMIT 0",
        "SELECT run_id,position,step_id,required,state,operation_id,generation,observations_json,detail FROM workflow_steps LIMIT 0",
    ] {
        connection.prepare(query)?;
    }
    if version >= 2 {
        connection.prepare("SELECT run_id,step_id,call_index,reservation_digest,reservation_json,observation_json FROM workflow_review_calls LIMIT 0")?;
        connection.prepare(
            "SELECT run_id,step_id,record_digest,record_json FROM workflow_reviews LIMIT 0",
        )?;
    }
    Ok(())
}

fn load_stored_run(connection: &Connection, run_id: &str) -> Result<Option<StoredRun>> {
    let row = connection
        .query_row(
            "SELECT definition_json,plan_digest,skill_digest,state FROM workflow_runs WHERE run_id=?1",
            [run_id],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(definition, plan_digest, skill_digest, state)| {
        if definition.len() > MAX_DEFINITION_BYTES {
            return Err(Error::Limit(
                "stored workflow definition exceeds journal limit".into(),
            ));
        }
        let plan_hex = plan_digest.strip_prefix("sha256:").ok_or_else(|| {
            Error::Invalid("stored workflow plan digest does not use sha256".into())
        })?;
        validate_raw_sha256("stored workflow plan digest", plan_hex)?;
        let definition: WorkflowDefinition = serde_json::from_slice(&definition)?;
        let ordered_steps = validate_retained_definition(&definition, &skill_digest, &plan_digest)?;
        Ok(StoredRun {
            definition,
            ordered_steps,
            plan_digest,
            skill_digest,
            state: parse_run_state(&state)?,
        })
    })
    .transpose()
}

fn validate_raw_sha256(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(format!(
            "{label} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

fn validate_step_record(
    definition: &StepDefinition,
    state: StepState,
    observations: &[PathObservation],
    detail: Option<&str>,
) -> Result<()> {
    if detail.is_some_and(|value| value.len() > MAX_DETAIL_BYTES) {
        return Err(Error::Limit(
            "stored workflow step detail exceeds journal limit".into(),
        ));
    }
    if matches!(state, StepState::Pending | StepState::Running) {
        if !observations.is_empty() || detail.is_some() {
            return Err(Error::Invalid(
                "unfinished workflow step contains terminal evidence".into(),
            ));
        }
        return Ok(());
    }
    if state == StepState::Cancelled {
        if !observations.is_empty() || detail.is_none() {
            return Err(Error::Invalid(
                "cancelled workflow step has inconsistent evidence".into(),
            ));
        }
        return Ok(());
    }
    if detail.is_none() {
        return Err(Error::Invalid(
            "terminal workflow step is missing result detail".into(),
        ));
    }
    match &definition.action {
        StepAction::Command { .. } | StepAction::ClaudeCodeReview { .. } => {
            if !observations.is_empty() {
                return Err(Error::Invalid(
                    "command step contains source-path observations".into(),
                ));
            }
            // Terminal command state is additionally checked against the durable
            // authority record by command_projection, including on historical reads.
        }
        StepAction::RequireCapability { .. } => {
            if !observations.is_empty() || state != StepState::Blocked {
                return Err(Error::Invalid(
                    "required-capability step contains an impossible result".into(),
                ));
            }
        }
        StepAction::SourcePaths { expectations } => {
            if observations.len() > expectations.len() {
                return Err(Error::Invalid(
                    "source-path step has too many observations".into(),
                ));
            }
            let mut total = 0u64;
            let mut mismatch = None;
            for (index, observation) in observations.iter().enumerate() {
                let expected = &expectations[index];
                if observation.path != expected.path {
                    return Err(Error::Invalid(
                        "source-path observation order does not match its declaration".into(),
                    ));
                }
                validate_raw_sha256("stored source-path observation digest", &observation.sha256)?;
                total = total.checked_add(observation.bytes).ok_or_else(|| {
                    Error::Limit("stored source-path observation byte count overflow".into())
                })?;
                if total > MAX_SOURCE_BYTES {
                    return Err(Error::Limit(
                        "stored source-path observations exceed 8 MiB".into(),
                    ));
                }
                if observation.sha256 != expected.sha256 && mismatch.replace(index).is_some() {
                    return Err(Error::Invalid(
                        "source-path result contains multiple digest mismatches".into(),
                    ));
                }
            }
            match state {
                StepState::Succeeded => {
                    if observations.len() != expectations.len() || mismatch.is_some() {
                        return Err(Error::Invalid(
                            "successful source-path step lacks exact matching observations".into(),
                        ));
                    }
                }
                StepState::Failed => {
                    if mismatch.is_some_and(|index| index + 1 != observations.len())
                        || (observations.len() == expectations.len() && mismatch.is_none())
                    {
                        return Err(Error::Invalid(
                            "failed source-path step has success-shaped observations".into(),
                        ));
                    }
                }
                StepState::Blocked => {
                    if mismatch.is_some() || observations.len() == expectations.len() {
                        return Err(Error::Invalid(
                            "blocked source-path step contains completion-shaped observations"
                                .into(),
                        ));
                    }
                }
                StepState::Pending | StepState::Running | StepState::Cancelled => {
                    unreachable!("handled above")
                }
            }
        }
    }
    Ok(())
}

fn review_projection(
    connection: &Connection,
    run_id: &str,
    run: &StoredRun,
    step: &StepDefinition,
    state: StepState,
) -> Result<Option<ClaudeReviewView>> {
    let Some(record) = review::read_record(connection, run_id, &step.id)? else {
        if matches!(step.action, StepAction::ClaudeCodeReview { .. })
            && matches!(state, StepState::Succeeded | StepState::Failed)
        {
            return Err(Error::Invalid(
                "completed review step has no retained controller record".into(),
            ));
        }
        return Ok(None);
    };
    let StepAction::ClaudeCodeReview { paths, model } = &step.action else {
        return Err(Error::Invalid(
            "review record attached to a non-review step".into(),
        ));
    };
    let command = run
        .definition
        .steps
        .iter()
        .find_map(|step| match &step.action {
            StepAction::Command {
                change,
                change_version,
                context_digest,
                ..
            } => Some((change, change_version, context_digest)),
            _ => None,
        })
        .ok_or_else(|| Error::Invalid("retained review lacks exact command context".into()))?;
    let subject = &record.request.subject;
    let mut expected_paths = paths.clone();
    expected_paths.sort();
    let mut actual_paths = record
        .request
        .recipe
        .source
        .iter()
        .map(|row| row.path.clone())
        .collect::<Vec<_>>();
    actual_paths.sort();
    if subject.repository != run.definition.source.repository
        || subject.revision != run.definition.source.revision
        || subject.change != *command.0
        || subject.change_version != *command.1
        || subject.context_digest != *command.2
        || record.request.attempt_id != command_attempt_id(run_id, &run.plan_digest, &step.id)
        || record.request.recipe.model != *model
        || record.request.recipe.policy_epoch != run.definition.source.policy_epoch
        || record.request.recipe.installed_policy_sha256
            != run
                .plan_digest
                .strip_prefix("sha256:")
                .unwrap_or(&run.plan_digest)
        || actual_paths != expected_paths
        || (matches!(state, StepState::Succeeded | StepState::Failed)
            && review::state(&record) != state)
    {
        return Err(Error::Invalid(
            "review record does not bind its workflow source, context and result".into(),
        ));
    }
    let mut projection = review::projection(&record);
    // Cancellation belongs to the workflow even when the last controller
    // observation was incomplete. Retain its stop reason for inspection.
    if state == StepState::Cancelled {
        projection.status = step_state_name(state).into();
    }
    Ok(Some(projection))
}

fn validate_dependency_results(definitions: &[StepDefinition], steps: &[StepView]) -> Result<()> {
    let states = steps
        .iter()
        .map(|step| (step.id.as_str(), step.state))
        .collect::<BTreeMap<_, _>>();
    for (definition, step) in definitions.iter().zip(steps) {
        if matches!(step.state, StepState::Succeeded | StepState::Failed)
            && definition
                .needs
                .iter()
                .any(|dependency| states.get(dependency.as_str()) != Some(&StepState::Succeeded))
        {
            return Err(Error::Invalid(format!(
                "completed workflow step {} has an unsuccessful dependency",
                step.id
            )));
        }
    }
    Ok(())
}

fn execute_step(engine: &Engine, source: &WorkflowSource, step: &StepDefinition) -> StepResult {
    match &step.action {
        StepAction::Command { .. } | StepAction::ClaudeCodeReview { .. } => StepResult {
            state: StepState::Blocked,
            observations: Vec::new(),
            detail: "command executor unavailable on this platform".into(),
        },
        StepAction::RequireCapability { capability } => StepResult {
            state: StepState::Blocked,
            observations: Vec::new(),
            detail: format!("required capability is unavailable: {capability}"),
        },
        StepAction::SourcePaths { expectations } => {
            let (engine, budget) = engine.with_read_budget(MAX_ENCODED_SOURCE_BYTES);
            let mut total = 0u64;
            let mut observations = Vec::with_capacity(expectations.len());
            for expectation in expectations {
                let bytes = match engine.read_regular_file_bounded(
                    &source.revision,
                    &expectation.path,
                    (MAX_SOURCE_BYTES - total) as usize,
                ) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        return StepResult {
                            state: StepState::Blocked,
                            observations,
                            detail: "source-path step exceeded its 8 MiB byte limit".into(),
                        };
                    }
                    Err(Error::NotFound(message))
                        if message == format!("path: {}", expectation.path) =>
                    {
                        return StepResult {
                            state: StepState::Failed,
                            observations,
                            detail: format!(
                                "required source path is missing: {}",
                                expectation.path
                            ),
                        };
                    }
                    Err(Error::Invalid(message))
                        if message.starts_with("unsupported entry kind: ") =>
                    {
                        return StepResult {
                            state: StepState::Blocked,
                            observations,
                            detail: message,
                        };
                    }
                    Err(error) => {
                        return StepResult {
                            state: StepState::Blocked,
                            observations,
                            detail: format!("could not read {}: {error}", expectation.path),
                        };
                    }
                };
                total = match total.checked_add(bytes.len() as u64) {
                    Some(total) if total <= MAX_SOURCE_BYTES => total,
                    _ => {
                        return StepResult {
                            state: StepState::Blocked,
                            observations,
                            detail: "source-path step exceeded its 8 MiB byte limit".into(),
                        };
                    }
                };
                let digest = format!("{:x}", Sha256::digest(&bytes));
                observations.push(PathObservation {
                    path: expectation.path.clone(),
                    sha256: digest.clone(),
                    bytes: bytes.len() as u64,
                });
                if digest != expectation.sha256 {
                    return StepResult {
                        state: StepState::Failed,
                        observations,
                        detail: format!("content digest does not match for {}", expectation.path),
                    };
                }
            }
            if budget.finish(Ok(())).is_err() {
                return StepResult::encoded_budget_failure();
            }
            StepResult {
                state: StepState::Succeeded,
                observations,
                detail: format!("{} source path expectations matched", expectations.len()),
            }
        }
    }
}

fn aggregate_state(steps: &[StepView]) -> RunState {
    if steps.iter().any(|step| step.state == StepState::Running) {
        return RunState::Running;
    }
    if steps.iter().any(|step| step.state == StepState::Pending) {
        return if steps.iter().all(|step| step.state == StepState::Pending) {
            RunState::Pending
        } else {
            RunState::Running
        };
    }
    if steps.iter().any(|step| step.state == StepState::Cancelled) {
        return RunState::Cancelled;
    }
    if steps
        .iter()
        .any(|step| step.required && step.state == StepState::Failed)
    {
        return RunState::Failed;
    }
    if steps
        .iter()
        .any(|step| step.required && step.state == StepState::Blocked)
    {
        return RunState::Blocked;
    }
    if steps
        .iter()
        .any(|step| !step.required && matches!(step.state, StepState::Failed | StepState::Blocked))
    {
        RunState::CompletedWithOmissions
    } else {
        RunState::Succeeded
    }
}

fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || run_id.len() > MAX_RUN_ID_BYTES
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(Error::Invalid(
            "workflow run ID must be 1..64 canonical ASCII bytes".into(),
        ));
    }
    Ok(())
}

fn operation_id(run_id: &str, step_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(OPERATION_DOMAIN);
    hash_field(&mut hasher, run_id.as_bytes());
    hash_field(&mut hasher, step_id.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn require_directory(path: &Path) -> Result<()> {
    let metadata = symlink_metadata(path)?;
    require_directory_metadata(path, &metadata)
}

fn require_directory_metadata(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid(format!(
            "workflow journal directory is not a regular directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_optional_regular(path: &Path) -> Result<()> {
    match symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(Error::Invalid(format!(
            "workflow journal file is not regular: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn journal_open_options() -> OpenOptions {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options
}

fn require_regular(path: &Path) -> Result<()> {
    let metadata = symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid(format!(
            "workflow journal file is not regular: {}",
            path.display()
        )));
    }
    Ok(())
}

fn sql_i64(label: &str, value: usize) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Limit(format!("{label} exceeds SQLite range")))
}

fn is_terminal_run(state: RunState) -> bool {
    matches!(
        state,
        RunState::Succeeded
            | RunState::CompletedWithOmissions
            | RunState::Failed
            | RunState::Blocked
            | RunState::Cancelled
    )
}

fn run_state_name(state: RunState) -> &'static str {
    match state {
        RunState::Pending => "pending",
        RunState::Running => "running",
        RunState::Succeeded => "succeeded",
        RunState::CompletedWithOmissions => "completed_with_omissions",
        RunState::Failed => "failed",
        RunState::Blocked => "blocked",
        RunState::Cancelled => "cancelled",
    }
}

fn step_state_name(state: StepState) -> &'static str {
    match state {
        StepState::Pending => "pending",
        StepState::Running => "running",
        StepState::Succeeded => "succeeded",
        StepState::Failed => "failed",
        StepState::Blocked => "blocked",
        StepState::Cancelled => "cancelled",
    }
}

fn parse_run_state(value: &str) -> Result<RunState> {
    match value {
        "pending" => Ok(RunState::Pending),
        "running" => Ok(RunState::Running),
        "succeeded" => Ok(RunState::Succeeded),
        "completed_with_omissions" => Ok(RunState::CompletedWithOmissions),
        "failed" => Ok(RunState::Failed),
        "blocked" => Ok(RunState::Blocked),
        "cancelled" => Ok(RunState::Cancelled),
        _ => Err(Error::Invalid("invalid workflow run state".into())),
    }
}

fn parse_step_state(value: &str) -> Result<StepState> {
    match value {
        "pending" => Ok(StepState::Pending),
        "running" => Ok(StepState::Running),
        "succeeded" => Ok(StepState::Succeeded),
        "failed" => Ok(StepState::Failed),
        "blocked" => Ok(StepState::Blocked),
        "cancelled" => Ok(StepState::Cancelled),
        _ => Err(Error::Invalid("invalid workflow step state".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn post_dispatch_writer_waits_for_a_brief_observer_without_replaying_work() {
        let (root, _) = result_fixture();
        let observer = Journal::open(root.path(), OpenMode::Read).unwrap();
        let path = root.path().to_path_buf();
        let (send, receive) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            send.send(Journal::open_after_dispatch(&path).map(drop))
                .unwrap();
        });
        assert!(matches!(
            receive.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(observer);
        receive
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        writer.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn post_dispatch_writer_reports_persistent_contention_and_unrelated_errors() {
        let (root, _) = result_fixture();
        let observer = Journal::open(root.path(), OpenMode::Read).unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(
            Journal::open_after_dispatch(root.path()),
            Err(Error::Conflict(_))
        ));
        assert!(started.elapsed() >= Duration::from_secs(1));
        drop(observer);
        let absent = root.path().join("absent");
        assert!(Journal::open_after_dispatch(&absent).is_err());
        assert!(!absent.exists());
    }

    fn result_fixture() -> (tempfile::TempDir, CompiledPlan) {
        let root = tempfile::tempdir().unwrap();
        let engine = Engine::open(root.path()).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[
                    crate::model::FileChange {
                        path: "SKILL.md".into(),
                        content: Some(b"# Test".to_vec()),
                    },
                    crate::model::FileChange {
                        path: "src/file".into(),
                        content: Some(b"abc".to_vec()),
                    },
                ],
            )
            .unwrap();
        let authority = crate::authority::Authority::open(root.path()).unwrap();
        let definition = WorkflowDefinition {
            version: 1,
            name: "result-guard".into(),
            source: WorkflowSource {
                repository: authority.repository_id().unwrap(),
                revision,
                policy_epoch: 0,
            },
            skill: super::super::contracts::SkillReference {
                entrypoint: "SKILL.md".into(),
                resources: vec![],
            },
            steps: vec![StepDefinition {
                id: "source".into(),
                needs: vec![],
                required: true,
                action: StepAction::SourcePaths {
                    expectations: vec![crate::checks::SourcePathExpectation {
                        path: "src/file".into(),
                        sha256: crate::store::digest(b"abc"),
                    }],
                },
            }],
        };
        let plan = compile(root.path(), &definition).unwrap();
        start(root.path(), "result-run", &plan).unwrap();
        (root, plan)
    }

    #[test]
    fn invalid_terminal_results_are_refused_before_journal_commit() {
        let (root, plan) = result_fixture();
        let step = &plan.ordered_steps()[0];
        let mut journal = Journal::open(root.path(), OpenMode::Write).unwrap();
        journal
            .set_run_state("result-run", RunState::Running)
            .unwrap();
        journal.begin_step("result-run", &step.id).unwrap();
        let before = journal.view("result-run").unwrap();
        let engine = Engine::open(root.path()).unwrap();
        let matched = execute_step(&engine, &plan.definition().source, step);
        assert_eq!(matched.state, StepState::Succeeded);
        for invalid in [
            StepResult {
                state: StepState::Succeeded,
                observations: Vec::new(),
                detail: "missing evidence".into(),
            },
            StepResult {
                state: StepState::Blocked,
                observations: matched.observations,
                detail: "source-path step exceeded its 16 MiB encoded-object read limit".into(),
            },
        ] {
            assert!(journal.finish_step("result-run", step, invalid).is_err());
            assert_eq!(journal.view("result-run").unwrap(), before);
        }
    }

    #[test]
    fn encoded_budget_failure_is_retained_as_a_readable_blocked_result() {
        let (root, plan) = result_fixture();
        let step = &plan.ordered_steps()[0];
        let mut journal = Journal::open(root.path(), OpenMode::Write).unwrap();
        journal
            .set_run_state("result-run", RunState::Running)
            .unwrap();
        journal.begin_step("result-run", &step.id).unwrap();
        journal
            .finish_step("result-run", step, StepResult::encoded_budget_failure())
            .unwrap();
        journal.refresh_run_state("result-run").unwrap();
        let view = journal.view("result-run").unwrap();
        assert_eq!(view.state, RunState::Blocked);
        assert!(view.steps[0].observations.is_empty());
        assert!(
            view.steps[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("16 MiB encoded-object")
        );
    }

    fn step(required: bool, state: StepState) -> StepView {
        StepView {
            needs: vec![],
            id: "step".into(),
            required,
            state,
            observations: Vec::new(),
            detail: None,
            execution: None,
            review: None,
        }
    }

    #[test]
    fn required_failures_and_blocks_remain_distinct() {
        assert_eq!(
            aggregate_state(&[step(true, StepState::Failed)]),
            RunState::Failed
        );
        assert_eq!(
            aggregate_state(&[step(true, StepState::Blocked)]),
            RunState::Blocked
        );
        assert_eq!(
            aggregate_state(&[step(false, StepState::Failed)]),
            RunState::CompletedWithOmissions
        );
    }

    #[test]
    fn operation_identity_is_stable_and_unambiguous() {
        assert_eq!(operation_id("a", "bc"), operation_id("a", "bc"));
        assert_ne!(operation_id("a", "bc"), operation_id("ab", "c"));
    }

    #[test]
    fn run_ids_are_bounded_canonical_ascii() {
        assert!(validate_run_id("run-1.ok").is_ok());
        for invalid in ["", "a/b", "two words", "é"] {
            assert!(validate_run_id(invalid).is_err());
        }
        assert!(validate_run_id(&"a".repeat(65)).is_err());
    }
}
