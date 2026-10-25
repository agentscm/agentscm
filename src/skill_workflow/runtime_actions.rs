//! Productive attempt reservation and artifact retention in the existing journal.
use super::*;
use crate::skill_workflow::action::{ActionOutcome, ActionView};
#[cfg(unix)]
use crate::skill_workflow::action::{ArtifactMetadata, CompiledAction};

pub(super) fn create_schema(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE workflow_action_attempts(
        run_id TEXT NOT NULL,step_id TEXT NOT NULL,profile_digest TEXT NOT NULL CHECK(length(profile_digest)=64),
        record_json BLOB CHECK(record_json IS NULL OR length(record_json)<=16384),
        artifact BLOB CHECK(artifact IS NULL OR length(artifact)<=262144),
        PRIMARY KEY(run_id,step_id),FOREIGN KEY(run_id,step_id) REFERENCES workflow_steps(run_id,step_id)
    ) STRICT;
    CREATE TABLE workflow_action_inputs(run_id TEXT PRIMARY KEY REFERENCES workflow_runs(run_id),digest TEXT NOT NULL CHECK(length(digest)=64),canonical_json BLOB NOT NULL CHECK(length(canonical_json)<=32768)) STRICT;")?;
    Ok(())
}

pub(super) fn projection(
    connection: &Connection,
    run_id: &str,
    step: &StepDefinition,
    state: StepState,
) -> Result<Option<ActionView>> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version < 5 {
        return Ok(None);
    }
    let row = connection.query_row("SELECT profile_digest,record_json,artifact FROM workflow_action_attempts WHERE run_id=?1 AND step_id=?2",
        params![run_id,step.id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,Option<Vec<u8>>>(1)?,row.get::<_,Option<Vec<u8>>>(2)?))).optional()?;
    let Some((profile_digest, record, artifact)) = row else {
        if matches!(step.action, StepAction::AgentAction { .. })
            && matches!(state, StepState::Succeeded | StepState::Failed)
        {
            return Err(Error::Invalid(
                "completed action has no durable record".into(),
            ));
        }
        return Ok(None);
    };
    if !matches!(step.action, StepAction::AgentAction { .. }) {
        return Err(Error::Invalid(
            "productive record belongs to another action kind".into(),
        ));
    }
    let Some(record) = record else {
        if artifact.is_some() || state == StepState::Succeeded || state == StepState::Failed {
            return Err(Error::Invalid(
                "unfinished action contains terminal evidence".into(),
            ));
        }
        return Ok(Some(ActionView {
            profile_digest,
            outcome: None,
            artifact: None,
            observed_model: None,
            verification: "not_independently_verified".into(),
        }));
    };
    let view: ActionView = serde_json::from_slice(&record)?;
    if view.profile_digest != profile_digest
        || view.outcome.is_none()
        || view.verification != "not_independently_verified"
    {
        return Err(Error::Invalid(
            "retained action identity is inconsistent".into(),
        ));
    }
    match (&view.artifact, artifact) {
        (Some(metadata), Some(bytes))
            if view.outcome == Some(ActionOutcome::Succeeded)
                && metadata.run_id == run_id
                && metadata.step_id == step.id
                && metadata.bytes == bytes.len()
                && metadata.sha256 == crate::store::digest(&bytes) => {}
        (None, None) if view.outcome != Some(ActionOutcome::Succeeded) => {}
        _ => {
            return Err(Error::Invalid(
                "retained action artifact is inconsistent".into(),
            ));
        }
    }
    if !matches!(state, StepState::Running | StepState::Cancelled)
        && step_result(view.outcome.as_ref().unwrap()).state != state
    {
        return Err(Error::Invalid("action state disagrees with outcome".into()));
    }
    Ok(Some(view))
}

/// Pin bounded event metadata before dispatch. Reusing the run with different inputs
/// conflicts, including after restart; metadata never grants execution authority.
pub fn bind_inputs(
    data_dir: &Path,
    run_id: &str,
    inputs: &BTreeMap<String, String>,
) -> Result<String> {
    validate_run_id(run_id)?;
    if inputs.len() > 32 {
        return Err(Error::Limit("too many workflow event fields".into()));
    }
    for name in inputs.keys() {
        crate::skill_workflow::compiler::validate_identifier("event input field", name)?;
    }
    let canonical = serde_json::to_vec(inputs)?;
    if canonical.len() > 32 * 1024 {
        return Err(Error::Limit("workflow event fields exceed 32 KiB".into()));
    }
    let digest = crate::store::digest(&canonical);
    let mut journal = Journal::open(data_dir, OpenMode::Write)?;
    journal.stored_run(run_id)?;
    let tx = journal
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let prior: Option<(String, Vec<u8>)> = tx
        .query_row(
            "SELECT digest,canonical_json FROM workflow_action_inputs WHERE run_id=?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((prior_digest, prior_bytes)) = prior {
        if prior_digest != digest || prior_bytes != canonical {
            return Err(Error::Conflict(
                "workflow event inputs already pinned to different bytes".into(),
            ));
        }
        tx.commit()?;
        return Ok(digest);
    }
    let dispatched: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_action_attempts WHERE run_id=?1)",
        [run_id],
        |row| row.get(0),
    )?;
    if dispatched {
        return Err(Error::Conflict(
            "cannot introduce event inputs after action dispatch".into(),
        ));
    }
    tx.execute(
        "INSERT INTO workflow_action_inputs VALUES(?1,?2,?3)",
        params![run_id, digest, canonical],
    )?;
    tx.commit()?;
    Ok(digest)
}

fn event_inputs(journal: &Journal, run_id: &str) -> Result<BTreeMap<String, String>> {
    let row: Option<(String, Vec<u8>)> = journal
        .connection
        .query_row(
            "SELECT digest,canonical_json FROM workflow_action_inputs WHERE run_id=?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((digest, bytes)) = row else {
        return Ok(BTreeMap::new());
    };
    if bytes.len() > 32 * 1024 || crate::store::digest(&bytes) != digest {
        return Err(Error::Invalid(
            "retained event input identity changed".into(),
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// Explicitly read one named, run-bound artifact. Status projections contain metadata only.
pub fn artifact(data_dir: &Path, run_id: &str, step_id: &str, name: &str) -> Result<Vec<u8>> {
    validate_run_id(run_id)?;
    let journal = Journal::open(data_dir, OpenMode::Read)?;
    artifact_from(&journal, run_id, step_id, name)
}

fn artifact_from(journal: &Journal, run_id: &str, step_id: &str, name: &str) -> Result<Vec<u8>> {
    let run = journal.view(run_id)?;
    let step = run
        .steps
        .iter()
        .find(|step| step.id == step_id)
        .ok_or_else(|| Error::NotFound("artifact producer".into()))?;
    let metadata = step
        .action
        .as_ref()
        .and_then(|action| action.artifact.as_ref())
        .filter(|metadata| {
            metadata.name == name
                && metadata.revision == run.source.revision
                && step.state == StepState::Succeeded
        })
        .ok_or_else(|| Error::NotFound("validated action artifact".into()))?;
    let bytes: Vec<u8> = journal.connection.query_row(
        "SELECT artifact FROM workflow_action_attempts WHERE run_id=?1 AND step_id=?2",
        params![run_id, step_id],
        |row| row.get(0),
    )?;
    if bytes.len() != metadata.bytes || crate::store::digest(&bytes) != metadata.sha256 {
        return Err(Error::Invalid("artifact content changed".into()));
    }
    Ok(bytes)
}

#[cfg(unix)]
pub(super) fn execute(
    mut journal: Journal,
    run_id: &str,
    plan: &CompiledPlan,
    step: &StepDefinition,
    action: &CompiledAction,
    executor: Option<&crate::skill_workflow::action::ActionExecutor<'_>>,
    guard: Option<&DispatchGuard<'_>>,
) -> Result<Journal> {
    let current = journal.view(run_id)?;
    let running = current
        .steps
        .iter()
        .any(|view| view.id == step.id && view.state == StepState::Running);
    if running {
        // A completed observation can be projected after a crash between retention and
        // step completion; an unresolved reservation can never trigger another dispatch.
        let view = projection(&journal.connection, run_id, step, StepState::Running)?;
        let outcome = view.as_ref().and_then(|view| view.outcome.clone()).ok_or_else(|| Error::Conflict("productive dispatch is running or interrupted; inspect, cancel or explicitly recover the action; never replay it".into()))?;
        journal.finish_step(run_id, step, step_result(&outcome))?;
        journal.refresh_run_state(run_id)?;
        return Ok(journal);
    }
    // No attempt exists yet. A stopped caller or unavailable operator runtime
    // can be retried explicitly under the same pinned plan without replaying work.
    let Some(executor) = executor else {
        return Err(Error::Conflict(
            "productive action needs an operator profile; no action was reserved".into(),
        ));
    };
    if executor.cancellation.is_cancelled() {
        return Err(Error::Conflict(
            "productive caller stopped before reservation; no action was reserved".into(),
        ));
    }
    let profile_digest=executor.profile.executable_identities(&[&journal.data_dir])
        .and_then(|_|executor.profile.digest())
        .map_err(|_|Error::Conflict("operator executable profile is unavailable or outside its trusted scope; no action was reserved".into()))?;
    let other: Option<String> = journal.connection.query_row("SELECT profile_digest FROM workflow_action_attempts WHERE run_id=?1 AND profile_digest!=?2 LIMIT 1",params![run_id,profile_digest],|row| row.get(0)).optional()?;
    if other.is_some() {
        return Err(Error::Conflict(
            "action profile changed within the retained run".into(),
        ));
    }
    let event = event_inputs(&journal, run_id)?;
    if action.definition.inputs.values().any(|input| matches!(input,crate::skill_workflow::action::ActionInput::Event { field } if !event.contains_key(field))) {
        journal.begin_step(run_id,&step.id)?;
        journal.finish_step(run_id,step,StepResult { state:StepState::Blocked,observations:Vec::new(),detail:"required pinned event input is unavailable".into() })?;
        journal.refresh_run_state(run_id)?;
        return Ok(journal);
    }
    let engine = Engine::open_existing(&journal.data_dir)?;
    let inputs = crate::skill_workflow::action::inputs(
        &engine,
        plan.definition(),
        action,
        &event,
        |producer, name| artifact_from(&journal, run_id, producer, name),
    )?;
    let tx = journal
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let canonical_event = serde_json::to_vec(&event)?;
    let retained_event: Option<Vec<u8>> = tx
        .query_row(
            "SELECT canonical_json FROM workflow_action_inputs WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )
        .optional()?;
    match retained_event {
        Some(bytes) if bytes != canonical_event => {
            return Err(Error::Conflict(
                "event inputs changed before action reservation".into(),
            ));
        }
        None => {
            tx.execute(
                "INSERT INTO workflow_action_inputs VALUES(?1,?2,?3)",
                params![
                    run_id,
                    crate::store::digest(&canonical_event),
                    canonical_event
                ],
            )?;
        }
        _ => {}
    }
    if tx.execute("UPDATE workflow_steps SET state='running',generation=generation+1,observations_json=?1,detail=NULL WHERE run_id=?2 AND step_id=?3 AND state='pending'",params![b"[]".as_slice(),run_id,step.id])? != 1 {
        return Err(Error::Conflict("productive action changed before reservation".into()));
    }
    let generation: i64 = tx.query_row(
        "SELECT generation FROM workflow_steps WHERE run_id=?1 AND step_id=?2",
        params![run_id, step.id],
        |row| row.get(0),
    )?;
    tx.execute("INSERT INTO workflow_action_attempts(run_id,step_id,profile_digest,record_json,artifact) VALUES(?1,?2,?3,NULL,NULL)",params![run_id,step.id,profile_digest])?;
    tx.commit()?;
    let data_dir = journal.data_dir.clone();
    drop(journal);
    let cancelled = || {
        if guard.is_some_and(|guard| guard().is_err()) {
            return true;
        }
        for attempt in 0..5 {
            match status(&data_dir, run_id) {
                Ok(view) => {
                    return view.state == RunState::Cancelled
                        || view
                            .steps
                            .iter()
                            .any(|view| view.id == step.id && view.state != StepState::Running);
                }
                Err(Error::Conflict(message))
                    if message.starts_with("workflow journal is active:") && attempt < 4 =>
                {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(_) => return true,
            }
        }
        true
    };
    let result = executor.dispatch(action, &inputs, &profile_digest, &cancelled);
    journal = Journal::open_after_dispatch(&data_dir)?;
    let (mut outcome, output, observed_model) = match result {
        Ok(result) => (result.outcome, result.output, result.observed_model),
        Err(_) => (ActionOutcome::Blocked, Vec::new(), None),
    };
    if journal.view(run_id)?.state == RunState::Cancelled {
        outcome = ActionOutcome::Cancelled;
    }
    if outcome == ActionOutcome::Succeeded
        && crate::skill_workflow::action::validate_output(&action.definition.output, &output)
            .is_err()
    {
        outcome = ActionOutcome::InvalidOutput;
    }
    let artifact = (outcome == ActionOutcome::Succeeded).then(|| ArtifactMetadata {
        name: action.definition.output.name.clone(),
        media_type: action.definition.output.media_type.clone(),
        sha256: crate::store::digest(&output),
        bytes: output.len(),
        run_id: run_id.into(),
        step_id: step.id.clone(),
        revision: plan.definition().source.revision.clone(),
    });
    let view = ActionView {
        profile_digest,
        outcome: Some(outcome.clone()),
        artifact,
        observed_model,
        verification: "not_independently_verified".into(),
    };
    let tx = journal
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    if tx.execute("UPDATE workflow_action_attempts SET record_json=?1,artifact=?2 WHERE run_id=?3 AND step_id=?4 AND record_json IS NULL AND EXISTS(SELECT 1 FROM workflow_steps s WHERE s.run_id=?3 AND s.step_id=?4 AND s.generation=?5 AND s.state IN ('running','cancelled'))",
        params![serde_json::to_vec(&view)?,(outcome == ActionOutcome::Succeeded).then_some(output),run_id,step.id,generation])? != 1 {
        // Recovery has fenced this dispatch. Preserve its retained outcome.
        tx.commit()?;
        return Ok(journal);
    }
    tx.commit()?;
    if journal
        .view(run_id)?
        .steps
        .iter()
        .any(|view| view.id == step.id && view.state == StepState::Running)
    {
        journal.finish_step(run_id, step, step_result(&outcome))?;
        journal.refresh_run_state(run_id)?;
    }
    Ok(journal)
}

/// Explicitly fence an unresolved productive dispatch. It may still be running;
/// its late observation cannot replace this uncertainty or authorize a replay.
pub fn recover_action(data_dir: &Path, run_id: &str, step_id: &str) -> Result<RunView> {
    validate_run_id(run_id)?;
    let mut journal = Journal::open(data_dir, OpenMode::Write)?;
    let stored = journal.stored_run(run_id)?;
    let step = stored
        .ordered_steps
        .iter()
        .find(|step| step.id == step_id && matches!(step.action, StepAction::AgentAction { .. }))
        .ok_or_else(|| Error::Invalid("not a productive action step".into()))?;
    let view = journal.view(run_id)?;
    if !view
        .steps
        .iter()
        .any(|view| view.id == step_id && view.state == StepState::Running)
    {
        return Ok(view);
    }
    let tx = journal
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (state, generation): (String, i64) = tx.query_row(
        "SELECT state,generation FROM workflow_steps WHERE run_id=?1 AND step_id=?2",
        params![run_id, step_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if state != "running" {
        tx.commit()?;
        return journal.view(run_id);
    }
    let (profile_digest,record):(String,Option<Vec<u8>>)=tx.query_row("SELECT profile_digest,record_json FROM workflow_action_attempts WHERE run_id=?1 AND step_id=?2",params![run_id,step_id],|row|Ok((row.get(0)?,row.get(1)?)))?;
    let prior: Option<ActionView> = record.as_deref().map(serde_json::from_slice).transpose()?;
    let outcome = prior
        .as_ref()
        .and_then(|view| view.outcome.clone())
        .unwrap_or(ActionOutcome::Uncertain);
    if record.is_none() {
        let record = ActionView {
            profile_digest,
            outcome: Some(ActionOutcome::Uncertain),
            artifact: None,
            observed_model: None,
            verification: "not_independently_verified".into(),
        };
        if tx.execute("UPDATE workflow_action_attempts SET record_json=?1 WHERE run_id=?2 AND step_id=?3 AND record_json IS NULL",params![serde_json::to_vec(&record)?,run_id,step_id])?!=1 { return Err(Error::Conflict("action outcome changed before recovery".into())); }
    }
    let result = step_result(&outcome);
    validate_step_record(
        step,
        result.state,
        &result.observations,
        Some(&result.detail),
    )?;
    if tx.execute("UPDATE workflow_steps SET state=?1,observations_json=?2,detail=?3 WHERE run_id=?4 AND step_id=?5 AND state='running' AND generation=?6",params![step_state_name(result.state),b"[]".as_slice(),result.detail,run_id,step_id,generation])?!=1 { return Err(Error::Conflict("action generation changed before recovery".into())); }
    tx.commit()?;
    journal.refresh_run_state(run_id)?;
    journal.view(run_id)
}

fn step_result(outcome: &ActionOutcome) -> StepResult {
    let state = match outcome {
        ActionOutcome::Succeeded => StepState::Succeeded,
        ActionOutcome::Failed
        | ActionOutcome::InvalidOutput
        | ActionOutcome::OutputLimit
        | ActionOutcome::TimedOut => StepState::Failed,
        ActionOutcome::Cancelled => StepState::Cancelled,
        ActionOutcome::Blocked | ActionOutcome::Uncertain => StepState::Blocked,
    };
    StepResult {
        state,
        observations: Vec::new(),
        detail: format!(
            "productive action: {}; output validation is not independent verification",
            serde_json::to_value(outcome).unwrap_or_default()
        ),
    }
}
