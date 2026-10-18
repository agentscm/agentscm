//! Durable `command/v1` attempts. Only the controller's run, cancel and recovery write records,
//! and admission re-reads them against the exact current context; nothing here accepts an
//! external record.
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::snapshot::remove_private_tree;
use super::{
    Cancellation, CleanupHandle, CommandRecipe, ExecutionOutcome, ExecutionRecord,
    ExecutionRequest, OciBackend, Snapshot,
};
use crate::checks::{
    CheckSubject, ensure_repository, load_current_context, required_binding, sha256, sql_i64,
    sql_u64, truncate_utf8, validate_digest, validate_identifier, validate_positive_sql_integer,
    validate_reason, validate_subject,
};
use crate::context::{ChangeId, ReviewContext};
use crate::store::Engine;
use crate::{Error, Result};

pub const COMMAND_CAPABILITY: &str = "agentscm.checks.command/v1";
pub const COMMAND_EVIDENCE: &str = "command/v1";

const ATTEMPT_VERSION: u32 = 1;
const REQUEST_DOMAIN: &[u8] = b"agentscm.command-check-request.v1\0";
const MAX_DETAIL_BYTES: usize = 8 * 1024;
const SNAPSHOT_MAX_FILES: u32 = 10_000;
const SNAPSHOT_MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandCheckRequest {
    pub attempt_id: String,
    pub change: ChangeId,
    pub change_version: u64,
    pub context_digest: String,
    pub revision: String,
    pub recipe: CommandRecipe,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandState {
    Pending,
    Running,
    Finished,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandAttempt {
    pub version: u32,
    pub attempt_id: String,
    pub request_digest: String,
    pub subject: CheckSubject,
    pub state: CommandState,
    pub run_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ExecutionOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custody: Option<CleanupCustody>,
}

/// Durable cleanup ownership of a possibly-created container of a running attempt.
/// `run_generation` is the claimed generation whose request owns the handle.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanupCustody {
    pub run_generation: u64,
    pub handle: CleanupHandle,
}

#[derive(Serialize)]
struct CanonicalRequest<'a> {
    attempt_id: &'a str,
    subject: &'a CheckSubject,
    recipe: &'a CommandRecipe,
}

impl CommandState {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "finished" => Ok(Self::Finished),
            _ => Err(Error::Invalid(format!("unknown command state {value}"))),
        }
    }
}

pub(crate) fn create_command_attempt(
    connection: &mut Connection,
    request: &CommandCheckRequest,
) -> Result<CommandAttempt> {
    validate_identifier("attempt ID", &request.attempt_id)?;
    request.change.validate()?;
    validate_positive_sql_integer("change version", request.change_version)?;
    validate_digest("context digest", &request.context_digest)?;
    validate_digest("revision", &request.revision)?;
    let recipe_bytes = request.recipe.canonical_bytes()?;

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_repository(&tx, &request.change.repository)?;
    if let Some((attempt, stored_recipe)) = read_attempt_with_recipe(&tx, &request.attempt_id)? {
        if attempt.subject.change != request.change
            || attempt.subject.change_version != request.change_version
            || attempt.subject.context_digest != request.context_digest
            || attempt.subject.revision != request.revision
            || stored_recipe != request.recipe
        {
            return Err(Error::Conflict(format!(
                "command attempt ID {} is already bound to different input",
                request.attempt_id
            )));
        }
        tx.commit()?;
        return Ok(attempt);
    }
    let context = load_current_context(
        &tx,
        &request.change,
        request.change_version,
        &request.context_digest,
        &request.revision,
    )?;
    let subject = subject_from_context(&context, &request.recipe)?;
    // Refuse attempts the collector could never run, such as unsupported attempt ID characters.
    ExecutionRequest {
        attempt_id: request.attempt_id.clone(),
        subject: subject.clone(),
        run_generation: 1,
        recipe: request.recipe.clone(),
    }
    .validate()?;
    tx.execute(
        "INSERT INTO command_attempts(
            attempt_id, request_digest, repository, change_id, change_version,
            context_digest, revision, recipe_sha256, toolchain_sha256,
            environment_sha256, recipe_json, state, run_generation
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending', 0)",
        params![
            request.attempt_id,
            request_digest(&request.attempt_id, &subject, &request.recipe)?,
            subject.repository,
            subject.change.id,
            sql_i64("change version", subject.change_version)?,
            subject.context_digest,
            subject.revision,
            subject.recipe_sha256,
            subject.toolchain_sha256,
            subject.environment_sha256,
            recipe_bytes,
        ],
    )?;
    let attempt = read_attempt_with_recipe(&tx, &request.attempt_id)?
        .expect("inserted command attempt")
        .0;
    tx.commit()?;
    Ok(attempt)
}

pub(crate) fn run_command_attempt(
    connection: &mut Connection,
    engine: &Engine,
    backend: &OciBackend,
    work_dir: &Path,
    attempt_id: &str,
    expected_generation: u64,
    cancel: &Cancellation,
) -> Result<CommandAttempt> {
    validate_identifier("attempt ID", attempt_id)?;
    let expected = sql_i64("run generation", expected_generation)?;
    let (running, recipe) = {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (attempt, recipe) = read_attempt_with_recipe(&tx, attempt_id)?
            .ok_or_else(|| Error::NotFound(format!("command attempt {attempt_id}")))?;
        if attempt.state == CommandState::Finished {
            tx.commit()?;
            return Ok(attempt);
        }
        if attempt.state == CommandState::Running {
            return Err(Error::Conflict(format!(
                "command attempt {attempt_id} is already running"
            )));
        }
        if attempt.run_generation != expected_generation {
            return Err(Error::Conflict(format!(
                "command attempt {attempt_id} is no longer at run generation {expected_generation}"
            )));
        }
        let context = load_current_context(
            &tx,
            &attempt.subject.change,
            attempt.subject.change_version,
            &attempt.subject.context_digest,
            &attempt.subject.revision,
        )?;
        // Compilation and attempt creation may precede a policy revocation.
        // Compare inside the claim transaction so a revoked context cannot
        // authorize a new execution after that revocation commits.
        let policy: i64 = tx.query_row(
            "SELECT policy_epoch FROM authority_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if sql_u64("policy epoch", policy)? != context.policy_epoch {
            return Err(Error::Conflict(
                "command review context no longer matches the current policy epoch".into(),
            ));
        }
        if subject_from_context(&context, &recipe)? != attempt.subject {
            return Err(Error::Conflict(
                "command subject no longer matches the current review context".to_owned(),
            ));
        }
        let next = expected
            .checked_add(1)
            .ok_or_else(|| Error::Limit("run generation exhausted".to_owned()))?;
        if tx.execute(
            "UPDATE command_attempts SET state = 'running', run_generation = ?1
             WHERE attempt_id = ?2 AND state = 'pending' AND run_generation = ?3",
            params![next, attempt_id, expected],
        )? != 1
        {
            return Err(Error::Conflict(format!(
                "command attempt {attempt_id} changed before it could run"
            )));
        }
        let running = read_attempt_with_recipe(&tx, attempt_id)?
            .expect("running command attempt")
            .0;
        tx.commit()?;
        (running, recipe)
    };

    let request = ExecutionRequest {
        attempt_id: running.attempt_id.clone(),
        subject: running.subject.clone(),
        run_generation: running.run_generation,
        recipe,
    };
    let claim = running.run_generation;
    let snapshot = Snapshot::materialize(
        engine,
        &request.subject.revision,
        &snapshot_path(work_dir, attempt_id, claim),
        SNAPSHOT_MAX_FILES,
        SNAPSHOT_MAX_BYTES,
    );
    let record = match snapshot {
        Ok(snapshot) => {
            // A failed persist means recovery fenced this run, so nothing may start.
            let mut persist = |handle: &CleanupHandle| {
                store_custody(connection, attempt_id, claim, claim, handle)
            };
            // A collector error leaves the attempt running with the custody persisted so far;
            // it never admits and needs recovery.
            backend.collect_with(&request, &snapshot, cancel, &mut persist)?
        }
        Err(error) => unobserved_record(
            request,
            ExecutionOutcome::Incomplete,
            format!("source snapshot unavailable: {error}"),
        )?,
    };
    if record.outcome == ExecutionOutcome::Indeterminate {
        let hint = match record
            .cleanup_handle
            .as_ref()
            .map(|handle| store_custody(connection, attempt_id, claim, claim, handle))
        {
            Some(Err(_)) => "recovery has already fenced this run".to_owned(),
            _ => format!("run check-recover-command {attempt_id} {claim}"),
        };
        return Err(Error::Conflict(format!(
            "cleanup of command attempt {attempt_id} is unconfirmed; {hint}"
        )));
    }
    store_record(connection, &running, &record)
}

/// Cancel (from pending or running) or recover (from running) an attempt. Running attempts are
/// fenced first, so a late finisher's compare-and-swap fails, and finish only once any container
/// is confirmed removed; otherwise they stay running for a retry at the new generation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stop_command_attempt(
    connection: &mut Connection,
    backend: Option<&OciBackend>,
    work_dir: &Path,
    attempt_id: &str,
    expected_generation: u64,
    terminal: ExecutionOutcome,
    abandon_unconfirmed: bool,
    reason: &str,
) -> Result<CommandAttempt> {
    validate_identifier("attempt ID", attempt_id)?;
    validate_reason(reason)?;
    let cancelling = match terminal {
        ExecutionOutcome::Cancelled => true,
        ExecutionOutcome::Incomplete => false,
        _ => {
            return Err(Error::Invalid(
                "a stopped command attempt is cancelled or incomplete".to_owned(),
            ));
        }
    };
    let next = expected_generation
        .checked_add(1)
        .ok_or_else(|| Error::Limit("run generation exhausted".to_owned()))?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (attempt, recipe) = read_attempt_with_recipe(&tx, attempt_id)?
        .ok_or_else(|| Error::NotFound(format!("command attempt {attempt_id}")))?;
    let request_for = |run_generation| ExecutionRequest {
        attempt_id: attempt.attempt_id.clone(),
        subject: attempt.subject.clone(),
        run_generation,
        recipe: recipe.clone(),
    };
    if attempt.state == CommandState::Finished {
        if attempt.outcome == Some(terminal)
            || (abandon_unconfirmed && attempt.outcome == Some(ExecutionOutcome::Indeterminate))
        {
            return Ok(attempt);
        }
        return Err(Error::Conflict(format!(
            "command attempt {attempt_id} already has a different terminal outcome"
        )));
    }
    if attempt.run_generation != expected_generation {
        return Err(Error::Conflict(format!(
            "command attempt {attempt_id} is no longer at run generation {expected_generation}"
        )));
    }
    if attempt.state == CommandState::Pending {
        if !cancelling {
            return Err(Error::Conflict(format!(
                "command attempt {attempt_id} is pending; cancel it instead of recovering it"
            )));
        }
        let record = unobserved_record(request_for(next), terminal, reason.to_owned())?;
        finish_attempt(
            &tx,
            attempt_id,
            "pending",
            expected_generation,
            next,
            &record,
        )?;
        let current = read_attempt_with_recipe(&tx, attempt_id)?
            .expect("cancelled command attempt")
            .0;
        tx.commit()?;
        return Ok(current);
    }
    if attempt.custody.is_some() && backend.is_none() {
        return Err(Error::Invalid(
            "a Docker runtime is required to reconcile this attempt's container; \
             pass --docker and --docker-socket"
                .to_owned(),
        ));
    }
    if tx.execute(
        "UPDATE command_attempts SET run_generation = ?1
         WHERE attempt_id = ?2 AND state = 'running' AND run_generation = ?3",
        params![
            sql_i64("run generation", next)?,
            attempt_id,
            sql_i64("run generation", expected_generation)?
        ],
    )? != 1
    {
        return Err(Error::Conflict(format!(
            "command attempt {attempt_id} changed before it could be fenced"
        )));
    }
    tx.commit()?;
    // No custody means the run was claimed at the fenced generation and never requested a container.
    let claim = attempt
        .custody
        .as_ref()
        .map_or(expected_generation, |custody| custody.run_generation);
    let mut record = unobserved_record(request_for(claim), terminal, reason.to_owned())?;
    if let (Some(mut custody), Some(backend)) = (attempt.custody.clone(), backend) {
        let reconciled = backend.reconcile(&request_for(claim), &mut custody.handle);
        if !matches!(reconciled, Ok(true)) {
            if !abandon_unconfirmed {
                let name = custody.handle.container_name.clone();
                store_custody(connection, attempt_id, next, claim, &custody.handle)?;
                let cause = reconciled
                    .err()
                    .map_or(String::new(), |error| format!(" ({error})"));
                return Err(Error::Conflict(format!(
                    "cleanup of container {name} is unconfirmed{cause}; retry recovery at run generation {next}"
                )));
            }
            // The operator accepted uncertain cleanup; the record keeps the handle and never admits.
            record.outcome = ExecutionOutcome::Indeterminate;
            record.cleanup_confirmed = false;
            record.runtime_identity = Some(custody.handle.runtime_identity.clone());
            record.cleanup_handle = Some(custody.handle);
        }
    }
    // A container that may still exist may still mount its snapshot.
    if record.cleanup_confirmed {
        // An earlier fence may have moved past the claim, so remove this attempt's snapshot of any generation.
        if let Ok(entries) = std::fs::read_dir(work_dir) {
            let prefix = format!("snapshot-{attempt_id}-");
            for entry in entries.flatten() {
                if entry.file_name().to_str().is_some_and(|name| {
                    name.strip_prefix(&prefix)
                        .is_some_and(|generation| generation.parse::<u64>().is_ok())
                }) {
                    remove_private_tree(&entry.path());
                }
            }
        }
    }
    let fenced = CommandAttempt {
        run_generation: next,
        ..attempt
    };
    let stored = store_record(connection, &fenced, &record)?;
    if stored.outcome != Some(record.outcome) {
        return Err(Error::Conflict(format!(
            "command attempt {attempt_id} already has a different terminal outcome"
        )));
    }
    Ok(stored)
}

pub(crate) fn read_command_attempt(
    connection: &Connection,
    attempt_id: &str,
) -> Result<CommandAttempt> {
    validate_identifier("attempt ID", attempt_id)?;
    read_attempt_with_recipe(connection, attempt_id)?
        .map(|value| value.0)
        .ok_or_else(|| Error::NotFound(format!("command attempt {attempt_id}")))
}

pub(crate) fn read_command_record(
    connection: &Connection,
    digest: &str,
) -> Result<ExecutionRecord> {
    validate_digest("record digest", digest)?;
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT canonical_json FROM command_evidence WHERE digest = ?1",
            [digest],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("command record {digest}")))?;
    let record: ExecutionRecord = serde_json::from_slice(&bytes)?;
    if record.canonical_bytes()? != bytes || record.digest()? != digest {
        return Err(Error::Invalid(format!(
            "stored command record {digest} failed canonical integrity validation"
        )));
    }
    Ok(record)
}

pub(crate) fn require_current_command_success(
    connection: &Connection,
    context: &ReviewContext,
) -> Result<()> {
    let Some(subject) = expected_subject(context)? else {
        return Ok(());
    };
    let mut statement = connection.prepare(
        "SELECT attempt_id FROM command_attempts
         WHERE repository = ?1 AND change_id = ?2 AND change_version = ?3
           AND context_digest = ?4 AND revision = ?5 AND recipe_sha256 = ?6
           AND toolchain_sha256 = ?7 AND environment_sha256 = ?8
           AND state = 'finished' AND outcome = 'success'
         ORDER BY attempt_id",
    )?;
    let candidates = statement
        .query_map(
            params![
                subject.repository,
                subject.change.id,
                sql_i64("change version", subject.change_version)?,
                subject.context_digest,
                subject.revision,
                subject.recipe_sha256,
                subject.toolchain_sha256,
                subject.environment_sha256,
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if candidates
        .iter()
        .any(|attempt_id| verify_success(connection, attempt_id, &subject).is_ok())
    {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "required evidence {COMMAND_EVIDENCE} has no exact successful controller-collected attempt"
    )))
}

pub(super) fn verify_success(
    connection: &Connection,
    attempt_id: &str,
    subject: &CheckSubject,
) -> Result<ExecutionRecord> {
    let attempt = read_command_attempt(connection, attempt_id)?;
    let digest = attempt
        .record_digest
        .as_deref()
        .ok_or_else(|| Error::Invalid("finished command attempt has no record".to_owned()))?;
    let record = read_command_record(connection, digest)?;
    if attempt.subject != *subject
        || attempt.outcome != Some(ExecutionOutcome::Success)
        || record.outcome != ExecutionOutcome::Success
        || !record.isolation_observed
        || !record.cleanup_confirmed
        || record.cleanup_handle.is_some()
        || record.request.attempt_id != attempt.attempt_id
        || record.request.run_generation != attempt.run_generation
        || record.request.subject != *subject
        || record.request.recipe.digest()? != subject.recipe_sha256
    {
        return Err(Error::Invalid(
            "stored command record does not match its successful attempt".to_owned(),
        ));
    }
    Ok(record)
}

fn store_record(
    connection: &mut Connection,
    running: &CommandAttempt,
    record: &ExecutionRecord,
) -> Result<CommandAttempt> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !finish_attempt(
        &tx,
        &running.attempt_id,
        "running",
        running.run_generation,
        running.run_generation,
        record,
    )? {
        let current = read_attempt_with_recipe(&tx, &running.attempt_id)?
            .ok_or_else(|| Error::NotFound(format!("command attempt {}", running.attempt_id)))?
            .0;
        tx.commit()?;
        if current.state == CommandState::Finished {
            return Ok(current);
        }
        return Err(Error::Conflict(format!(
            "command attempt {} changed before its record was stored",
            running.attempt_id
        )));
    }
    let current = read_attempt_with_recipe(&tx, &running.attempt_id)?
        .expect("finished command attempt")
        .0;
    tx.commit()?;
    Ok(current)
}

/// Compare-and-swap `from` at `generation` to finished at `to_generation`, then add the evidence.
/// False means the attempt was no longer there, so a late writer changes nothing.
fn finish_attempt(
    tx: &Connection,
    attempt_id: &str,
    from: &str,
    generation: u64,
    to_generation: u64,
    record: &ExecutionRecord,
) -> Result<bool> {
    let digest = record.digest()?;
    let changed = tx.execute(
        "UPDATE command_attempts
         SET state = 'finished', run_generation = ?1, outcome = ?2, record_digest = ?3,
             custody_json = NULL
         WHERE attempt_id = ?4 AND state = ?5 AND run_generation = ?6 AND record_digest IS NULL",
        params![
            sql_i64("run generation", to_generation)?,
            outcome_str(record.outcome)?,
            digest,
            attempt_id,
            from,
            sql_i64("run generation", generation)?,
        ],
    )?;
    if changed == 1 {
        tx.execute(
            "INSERT INTO command_evidence(digest, attempt_id, canonical_json) VALUES (?1, ?2, ?3)",
            params![digest, attempt_id, record.canonical_bytes()?],
        )?;
    }
    Ok(changed == 1)
}

/// `owner_generation` is the claimed generation whose request owns the handle; the compare-and-swap
/// is against the attempt's current `generation`. Zero rows changed means recovery fenced the run.
fn store_custody(
    connection: &Connection,
    attempt_id: &str,
    generation: u64,
    owner_generation: u64,
    handle: &CleanupHandle,
) -> Result<()> {
    let custody = CleanupCustody {
        run_generation: owner_generation,
        handle: handle.clone(),
    };
    if connection.execute(
        "UPDATE command_attempts SET custody_json = ?1
         WHERE attempt_id = ?2 AND state = 'running' AND run_generation = ?3",
        params![
            serde_json::to_vec(&custody)?,
            attempt_id,
            sql_i64("run generation", generation)?
        ],
    )? != 1
    {
        return Err(Error::Conflict(format!(
            "command attempt {attempt_id} was fenced by recovery"
        )));
    }
    Ok(())
}

fn snapshot_path(work_dir: &Path, attempt_id: &str, generation: u64) -> PathBuf {
    work_dir.join(format!("snapshot-{attempt_id}-{generation}"))
}

/// No isolation was observed and the container is confirmed absent, so nothing can admit.
fn unobserved_record(
    request: ExecutionRequest,
    outcome: ExecutionOutcome,
    detail: String,
) -> Result<ExecutionRecord> {
    Ok(ExecutionRecord {
        version: 1,
        request_sha256: request.digest()?,
        request,
        outcome,
        exit_code: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        output_truncated: false,
        cleanup_confirmed: true,
        cleanup_handle: None,
        isolation_observed: false,
        runtime_identity: None,
        detail: truncate_utf8(detail.replace('\0', ""), MAX_DETAIL_BYTES),
    })
}

fn read_attempt_with_recipe(
    connection: &Connection,
    attempt_id: &str,
) -> Result<Option<(CommandAttempt, CommandRecipe)>> {
    connection
        .query_row(
            "SELECT request_digest, repository, change_id, change_version,
                    context_digest, revision, recipe_sha256, toolchain_sha256,
                    environment_sha256, recipe_json, state, outcome,
                    run_generation, record_digest, custody_json
             FROM command_attempts WHERE attempt_id = ?1",
            [attempt_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Vec<u8>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, Option<Vec<u8>>>(14)?,
                ))
            },
        )
        .optional()?
        .map(
            |(
                request_digest,
                repository,
                change_id,
                change_version,
                context_digest,
                revision,
                recipe_sha256,
                toolchain_sha256,
                environment_sha256,
                recipe_bytes,
                state,
                outcome,
                run_generation,
                record_digest,
                custody_bytes,
            )| {
                let invalid = || {
                    Error::Invalid(format!(
                        "stored command attempt {attempt_id} failed canonical integrity validation"
                    ))
                };
                let recipe: CommandRecipe = serde_json::from_slice(&recipe_bytes)?;
                if sha256(&recipe_bytes) != recipe_sha256
                    || recipe.canonical_bytes()? != recipe_bytes
                {
                    return Err(invalid());
                }
                let mut attempt = CommandAttempt {
                    version: ATTEMPT_VERSION,
                    attempt_id: attempt_id.to_owned(),
                    request_digest,
                    subject: CheckSubject {
                        repository: repository.clone(),
                        change: ChangeId {
                            repository,
                            id: change_id,
                        },
                        change_version: sql_u64("change version", change_version)?,
                        context_digest,
                        revision,
                        recipe_sha256,
                        toolchain_sha256,
                        environment_sha256,
                    },
                    state: CommandState::parse(&state)?,
                    run_generation: sql_u64("run generation", run_generation)?,
                    outcome: outcome.as_deref().map(parse_outcome).transpose()?,
                    record_digest,
                    custody: None,
                };
                validate_subject(&attempt.subject)?;
                // Attempt IDs name snapshot directories, so enforce the collector's stricter rule.
                super::contracts::validate_attempt_id(attempt_id)?;
                if (attempt.state == CommandState::Finished) != attempt.outcome.is_some()
                    || attempt.outcome.is_some() != attempt.record_digest.is_some()
                    || attempt.subject.toolchain_sha256 != recipe.toolchain_sha256
                    || attempt.subject.environment_sha256 != recipe.environment_sha256
                    || request_digest_for(&attempt, &recipe)? != attempt.request_digest
                {
                    return Err(invalid());
                }
                if let Some(bytes) = custody_bytes {
                    let custody: CleanupCustody = serde_json::from_slice(&bytes)?;
                    if attempt.state != CommandState::Running
                        || custody.run_generation > attempt.run_generation
                        || serde_json::to_vec(&custody)? != bytes
                    {
                        return Err(invalid());
                    }
                    custody.handle.validate(&ExecutionRequest {
                        attempt_id: attempt_id.to_owned(),
                        subject: attempt.subject.clone(),
                        run_generation: custody.run_generation,
                        recipe: recipe.clone(),
                    })?;
                    attempt.custody = Some(custody);
                }
                Ok((attempt, recipe))
            },
        )
        .transpose()
}

fn expected_subject(context: &ReviewContext) -> Result<Option<CheckSubject>> {
    let Some((recipe, toolchain, environment)) =
        required_binding(context, COMMAND_CAPABILITY, COMMAND_EVIDENCE)?
    else {
        return Ok(None);
    };
    Ok(Some(CheckSubject {
        repository: context.change.repository.clone(),
        change: context.change.clone(),
        change_version: context.change_version,
        context_digest: context.digest()?,
        revision: context.revision.clone(),
        recipe_sha256: recipe,
        toolchain_sha256: toolchain,
        environment_sha256: environment,
    }))
}

fn subject_from_context(context: &ReviewContext, recipe: &CommandRecipe) -> Result<CheckSubject> {
    let subject = expected_subject(context)?.ok_or_else(|| {
        Error::Invalid(format!(
            "review context does not request capability {COMMAND_CAPABILITY}"
        ))
    })?;
    if subject.recipe_sha256 != recipe.digest()? {
        return Err(Error::Conflict(
            "command recipe does not match the current review context".to_owned(),
        ));
    }
    if subject.toolchain_sha256 != recipe.toolchain_sha256
        || subject.environment_sha256 != recipe.environment_sha256
    {
        return Err(Error::Conflict(
            "command toolchain or environment does not match the current review context".to_owned(),
        ));
    }
    Ok(subject)
}

fn request_digest(
    attempt_id: &str,
    subject: &CheckSubject,
    recipe: &CommandRecipe,
) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(REQUEST_DOMAIN);
    hasher.update(serde_json::to_vec(&CanonicalRequest {
        attempt_id,
        subject,
        recipe,
    })?);
    Ok(format!("{:x}", hasher.finalize()))
}

fn request_digest_for(attempt: &CommandAttempt, recipe: &CommandRecipe) -> Result<String> {
    request_digest(&attempt.attempt_id, &attempt.subject, recipe)
}

fn outcome_str(outcome: ExecutionOutcome) -> Result<String> {
    match serde_json::to_value(outcome)? {
        serde_json::Value::String(value) => Ok(value),
        _ => Err(Error::Invalid(
            "execution outcome is not a string".to_owned(),
        )),
    }
}

fn parse_outcome(value: &str) -> Result<ExecutionOutcome> {
    Ok(serde_json::from_value(serde_json::Value::String(
        value.to_owned(),
    ))?)
}
