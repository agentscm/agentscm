//! Controller-owned review calls. Source bytes are data, never host commands.
#[cfg(unix)]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use rusqlite::TransactionBehavior;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use super::{Journal, OpenMode};
#[cfg(unix)]
use crate::checks::CheckSubject;
use crate::reviewer::*;
use crate::skill_workflow::StepState;
#[cfg(unix)]
use crate::skill_workflow::{CompiledPlan, StepAction};
#[cfg(unix)]
use crate::store::Engine;
use crate::store::digest;
use crate::{Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClaudeReviewFinding {
    pub path: Option<String>,
    pub start_line: Option<u64>,
    pub end_line: Option<u64>,
    pub title: String,
    pub detail: String,
    pub severity: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClaudeReviewView {
    pub model_requested: String,
    pub model_observed: Option<String>,
    pub status: String,
    pub stop: Option<String>,
    pub findings: Vec<ClaudeReviewFinding>,
    pub unresolved: Vec<String>,
    pub covered_paths: Vec<String>,
}

pub(super) fn create_schema(connection: &rusqlite::Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE workflow_review_calls (
            run_id TEXT NOT NULL, step_id TEXT NOT NULL, call_index INTEGER NOT NULL,
            reservation_digest TEXT NOT NULL UNIQUE, reservation_json BLOB NOT NULL,
            observation_json BLOB, PRIMARY KEY(run_id,step_id,call_index),
            FOREIGN KEY(run_id,step_id) REFERENCES workflow_steps(run_id,step_id)
         ) STRICT;
         CREATE TABLE workflow_reviews (
            run_id TEXT NOT NULL, step_id TEXT NOT NULL, record_digest TEXT NOT NULL,
            record_json BLOB NOT NULL, PRIMARY KEY(run_id,step_id),
            FOREIGN KEY(run_id,step_id) REFERENCES workflow_steps(run_id,step_id)
         ) STRICT;",
    )?;
    Ok(())
}

#[cfg(unix)]
struct NativeSource(BTreeMap<String, ReviewSourceObservation>);

#[cfg(unix)]
impl ReviewSource for NativeSource {
    fn read(
        &mut self,
        _: &ReviewRequest,
        selector: &ReviewSourceSelector,
        max_bytes: usize,
    ) -> std::result::Result<ReviewSourceObservation, SourceReadFailure> {
        let row = self
            .0
            .get(&selector.path)
            .ok_or(SourceReadFailure::Missing)?;
        if row.raw.len() > max_bytes {
            return Err(SourceReadFailure::Limit);
        }
        Ok(row.clone())
    }
}

// The reservation check is durable; this additional check is immediately before
// each actual model invocation, including subsequent source-retrieval calls.
#[cfg(unix)]
struct GuardedTransport<'a> {
    inner: &'a ClaudeCodeTransport,
    guard: Option<&'a super::DispatchGuard<'a>>,
}
#[cfg(unix)]
impl ModelTransport for GuardedTransport<'_> {
    fn scope_sha256(&self) -> String {
        self.inner.scope_sha256()
    }
    fn basis(&self) -> TransportBasis {
        self.inner.basis()
    }
    fn invoke<'a>(
        &'a self,
        call: &'a PreparedModelCall,
        cancel: &'a ReviewCancellation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelCallObservation> + Send + 'a>>
    {
        Box::pin(async move {
            if self.guard.is_some_and(|guard| guard().is_err()) {
                return ModelCallObservation::not_started(self.basis(), ModelFailure::Cancelled);
            }
            self.inner.invoke(call, cancel).await
        })
    }
}

#[cfg(unix)]
struct Calls<'a> {
    data_dir: &'a Path,
    plan: &'a CompiledPlan,
    run_id: &'a str,
    step_id: &'a str,
    generation: u64,
    guard: Option<&'a super::DispatchGuard<'a>>,
}

#[cfg(unix)]
impl ReviewJournal for Calls<'_> {
    fn reserve_call(&mut self, reservation: &CallReservation) -> Result<ReservationOutcome> {
        if let Some(guard) = self.guard {
            guard()?;
        }
        let mut journal = Journal::open(self.data_dir, OpenMode::Write)?;
        super::verify_current(self.data_dir, self.plan)?;
        let tx = journal
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: (String, u64) = tx.query_row(
            "SELECT state,generation FROM workflow_steps WHERE run_id=?1 AND step_id=?2",
            params![self.run_id, self.step_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if current != ("running".into(), self.generation)
            || reservation.run_generation != self.generation
            || reservation.call_index >= 4
        {
            return Err(Error::Conflict(
                "review dispatch lost its workflow claim".into(),
            ));
        }
        let digest = reservation.digest()?;
        let changed = tx.execute(
            "INSERT INTO workflow_review_calls(run_id,step_id,call_index,reservation_digest,reservation_json)
             VALUES(?1,?2,?3,?4,?5) ON CONFLICT(run_id,step_id,call_index) DO NOTHING",
            params![self.run_id,self.step_id,reservation.call_index,digest,serde_json::to_vec(reservation)?])?;
        tx.commit()?;
        if changed == 0 {
            return Ok(ReservationOutcome::Existing);
        }
        Ok(ReservationOutcome::Reserved(CallTicket {
            reservation_sha256: digest,
            run_generation: self.generation,
            durable_version: u64::from(reservation.call_index) + 1,
        }))
    }
    fn record_call(
        &mut self,
        ticket: &CallTicket,
        observation: &ModelCallObservation,
    ) -> Result<()> {
        let journal = Journal::open_after_dispatch(self.data_dir)?;
        if ticket.run_generation != self.generation || journal.connection.execute(
            "UPDATE workflow_review_calls SET observation_json=?1 WHERE run_id=?2 AND step_id=?3
             AND reservation_digest=?4 AND observation_json IS NULL",
            params![serde_json::to_vec(observation)?,self.run_id,self.step_id,ticket.reservation_sha256])? != 1
        { return Err(Error::Conflict("review call observation was already recorded or fenced".into())); }
        Ok(())
    }
}

#[cfg(unix)]
pub(super) fn execute(
    data_dir: &Path,
    run_id: &str,
    plan: &CompiledPlan,
    step_id: &str,
    executor: &super::ClaudeExecutor<'_>,
    guard: Option<&super::DispatchGuard<'_>>,
) -> Result<ReviewRecord> {
    let executable = executor.executable;
    let work_dir = executor.work_dir;
    let cancellation = executor.cancellation;
    let journal = Journal::open(data_dir, OpenMode::Read)?;
    let generation: u64 = journal.connection.query_row(
        "SELECT generation FROM workflow_steps WHERE run_id=?1 AND step_id=?2",
        params![run_id, step_id],
        |row| row.get(0),
    )?;
    drop(journal);
    let step = plan
        .ordered_steps()
        .iter()
        .find(|step| step.id == step_id)
        .ok_or_else(|| Error::Invalid("review step is not in plan".into()))?;
    let StepAction::ClaudeCodeReview { paths, model } = &step.action else {
        return Err(Error::Invalid("expected Claude review step".into()));
    };
    let command = plan
        .ordered_steps()
        .iter()
        .find_map(|step| match &step.action {
            StepAction::Command {
                change,
                change_version,
                context_digest,
                ..
            } => Some((change, *change_version, context_digest)),
            _ => None,
        })
        .ok_or_else(|| Error::Invalid("review has no exact command context".into()))?;
    let authority = crate::authority::Authority::open_read_only(data_dir)?;
    let context = authority.review_context(command.2)?;
    if let Some(guard) = guard {
        guard()?;
    }
    let native_transport = ClaudeCodeTransport::new(executable, work_dir)?;
    let transport = GuardedTransport {
        inner: &native_transport,
        guard,
    };
    let engine = Engine::open(data_dir)?;
    let (source, budget) = engine.with_read_budget(4 * 1024 * 1024);
    let (objective, rows, catalog) = budget.finish((|| {
        let read = |path: &str, max: usize| -> Result<Vec<u8>> {
            source
                .read_regular_file_bounded(&plan.definition().source.revision, path, max)?
                .ok_or_else(|| Error::Limit("review source exceeds its byte limit".into()))
        };
        let objective = String::from_utf8(read(&plan.definition().skill.entrypoint, 4096)?)
            .map_err(|_| Error::Invalid("review skill must be UTF-8".into()))?;
        let binding = ReviewSourceBinding {
            profile: "native-local/v1".into(),
            permit_sha256: digest(
                serde_json::to_vec(&(&plan.definition().source, plan.digest()))?.as_slice(),
            ),
        };
        let mut rows = BTreeMap::new();
        let mut catalog = Vec::new();
        for path in paths {
            let bytes = read(path, MAX_ROW_BYTES)?;
            let text = String::from_utf8(bytes.clone())
                .map_err(|_| Error::Invalid("review source must be UTF-8".into()))?;
            let lines = text.lines().count();
            if lines == 0 || lines > 16384 {
                return Err(Error::Limit(
                    "review source has unsupported line count".into(),
                ));
            }
            catalog.push(ReviewSourceSelector {
                source: binding.clone(),
                path: path.clone(),
                start_line: 1,
                end_line: lines as u32,
                raw_sha256: digest(&bytes),
                rendered_sha256: digest(&bytes),
                required: true,
            });
            if rows
                .values()
                .map(|row: &ReviewSourceObservation| row.raw.len())
                .sum::<usize>()
                + bytes.len()
                > 64 * 1024
            {
                return Err(Error::Limit(
                    "review source exceeds the 64 KiB aggregate profile".into(),
                ));
            }
            rows.insert(
                path.clone(),
                ReviewSourceObservation {
                    source: binding.clone(),
                    raw: bytes,
                    rendered: text,
                    rendering: SourceRendering::ExactUtf8,
                },
            );
        }
        Ok((objective, rows, catalog))
    })())?;
    let recipe = ReviewRecipe {
        version: 1,
        model: model.clone(),
        objective,
        provider_scope_sha256: transport.scope_sha256(),
        installed_policy_sha256: plan
            .digest()
            .strip_prefix("sha256:")
            .unwrap_or(plan.digest())
            .into(),
        policy_epoch: plan.definition().source.policy_epoch,
        source: catalog,
        limits: ReviewLimits {
            max_source_bytes: 64 * 1024,
            max_request_bytes: 256 * 1024,
            max_total_request_bytes: 1024 * 1024,
            max_response_bytes: 128 * 1024,
            max_calls: 4,
            max_input_tokens: 64_000,
            max_output_tokens: 4096,
            max_total_tokens: 80_000,
            max_cost_micros: None,
            timeout_ms: 60_000,
        },
    };
    let request = ReviewRequest {
        attempt_id: super::command_attempt_id(run_id, plan.digest(), step_id),
        run_generation: generation,
        subject: CheckSubject {
            repository: command.0.repository.clone(),
            change: command.0.clone(),
            change_version: command.1,
            context_digest: command.2.clone(),
            revision: context.revision.clone(),
            recipe_sha256: recipe.digest()?,
            toolchain_sha256: digest(b"agentscm.claude-code.tools-disabled/v1"),
            environment_sha256: transport.scope_sha256(),
        },
        recipe,
    };
    let mut calls = Calls {
        data_dir,
        plan,
        run_id,
        step_id,
        generation,
        guard,
    };
    // A cancellation from another CLI process is durable state, not this
    // process's signal token. Retain a validated read-only connection and the
    // recovery access guard, but release its journal lease: observation must
    // not contend with reservation writes or the cancelling controller.
    // This connection only observes cancellation. Writes and finalization
    // independently acquire their lease and revalidate source and generation.
    let Journal {
        connection: cancellation_db,
        _access: cancellation_access,
        _lease: cancellation_lease,
        ..
    } = Journal::open(data_dir, OpenMode::Read)?;
    drop(cancellation_lease);
    let done = std::sync::atomic::AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        let done_ref = &done;
        scope.spawn(move || {
            let _access = cancellation_access;
            use std::sync::atomic::Ordering;
            while !done_ref.load(Ordering::Acquire) {
                let state = cancellation_db.query_row(
                    "SELECT state FROM workflow_steps WHERE run_id=?1 AND step_id=?2",
                    params![run_id, step_id],
                    |row| row.get::<_, String>(0),
                );
                if state.is_ok_and(|state| state == "cancelled") {
                    cancellation.cancel();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        let result = tokio::runtime::Handle::current().block_on(ReviewController::run(
            request,
            &mut NativeSource(rows),
            &mut calls,
            &transport,
            cancellation,
        ));
        done.store(true, std::sync::atomic::Ordering::Release);
        result
    });
    let record = result?;
    let journal = Journal::open_after_dispatch(data_dir)?;
    let bytes = serde_json::to_vec(&record)?;
    journal.connection.execute(
        "INSERT INTO workflow_reviews(run_id,step_id,record_digest,record_json) VALUES(?1,?2,?3,?4)",
        params![run_id,step_id,digest(&bytes),bytes])?;
    Ok(record)
}

pub(super) fn read_record(
    connection: &rusqlite::Connection,
    run_id: &str,
    step_id: &str,
) -> Result<Option<ReviewRecord>> {
    // v1 journals remain readable without migration.
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version < 2 {
        return Ok(None);
    }
    let row: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT record_digest,record_json FROM workflow_reviews WHERE run_id=?1 AND step_id=?2",
            params![run_id, step_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(expected,bytes)| {
        if bytes.len() > 12*1024*1024 || digest(&bytes) != expected {
            return Err(Error::Invalid("retained review record integrity failed".into()));
        }
        let record: ReviewRecord = serde_json::from_slice(&bytes)?;
        if record.request.digest()? != record.request_sha256 {
            return Err(Error::Invalid("retained review request integrity failed".into()));
        }
        for call in &record.calls {
            let stored: (String,Vec<u8>,Option<Vec<u8>>) = connection.query_row(
                "SELECT reservation_digest,reservation_json,observation_json FROM workflow_review_calls
                 WHERE run_id=?1 AND step_id=?2 AND call_index=?3",
                params![run_id,step_id,call.reservation.call_index], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
            // A failed observation write leaves a reserved call and an incomplete
            // controller record. Keep that record inspectable without treating it
            // as durable successful execution or permitting a provider replay.
            let observation_matches = match stored.2.as_ref() {
                Some(bytes) => *bytes == serde_json::to_vec(&call.observation)?,
                None => !call.journal_recorded,
            };
            if stored.0 != call.reservation.digest()? || stored.1 != serde_json::to_vec(&call.reservation)?
                || !observation_matches
                || (!call.journal_recorded && (record.stop != ReviewStop::Journal
                    || record.completion != ReviewCompletion::Incomplete))
            { return Err(Error::Invalid("retained review call does not match its durable reservation".into())); }
            let prepared = PreparedModelCall {body:call.reservation.request_body.clone(),
                expected_model:record.request.recipe.model.clone(),
                max_response_bytes:record.request.recipe.limits.max_response_bytes as usize,
                max_output_tokens:record.request.recipe.limits.max_output_tokens,
                timeout_ms:record.request.recipe.limits.timeout_ms};
            if call.reservation.review_request_sha256 != record.request_sha256
                || call.reservation.subject != record.request.subject
                || call.reservation.run_generation != record.request.run_generation
                || call.reservation.request_sha256 != prepared.request_sha256()
                || call.ticket.reservation_sha256 != call.reservation.digest()?
                || !call.observation.consistent_with(&prepared,call.reservation.transport_basis) {
                return Err(Error::Invalid("retained review observation failed controller validation".into()));
            }
        }
        let ranges=record.coverage.iter().filter(|row|row.exposure==RowExposure::Acknowledged)
            .map(|row|(row.row_id.clone(),row.selector.start_line,row.selector.end_line)).collect::<Vec<_>>();
        for (ordinal,finding) in record.findings.iter().enumerate() {
            finding.claim.validate(&ranges)?;
            let call_index=record.calls.last().ok_or_else(||Error::Invalid("finding has no recorded model call".into()))?.reservation.call_index;
            if finding.identity != assign_new_finding(&record.request.subject,&finding.claim,
                &record.request.attempt_id,record.request.run_generation,u64::from(call_index),ordinal as u64)? {
                return Err(Error::Invalid("retained finding identity does not match its controller call".into()));
            }
        }
        Ok(record)
    }).transpose()
}

pub(super) fn state(record: &ReviewRecord) -> StepState {
    if record.completion == ReviewCompletion::Cancelled {
        return StepState::Cancelled;
    }
    if record.completion != ReviewCompletion::FinishedObservations
        || record.stop != ReviewStop::ModelFinished
        || !record.unresolved.is_empty()
        || record.calls.is_empty()
        || record.unconfirmed_reservation.is_some()
        || record.calls.iter().any(|call| {
            !call.journal_recorded
                || call.observation.state != CallState::Acknowledged
                || call.observation.failure.is_some()
                || !call.observation.response_capture_complete
        })
        || record
            .coverage
            .iter()
            .any(|row| row.selector.required && row.exposure != RowExposure::Acknowledged)
    {
        return StepState::Blocked;
    }
    if record
        .findings
        .iter()
        .any(|finding| finding.claim.severity != FindingSeverity::Note)
    {
        StepState::Failed
    } else {
        StepState::Succeeded
    }
}

pub(super) fn projection(record: &ReviewRecord) -> ClaudeReviewView {
    let model_observed = record.calls.last().and_then(|call| {
        serde_json::from_slice::<serde_json::Value>(&call.observation.captured_response)
            .ok()
            .and_then(|value| {
                value
                    .get("modelUsage")
                    .and_then(|usage| usage.as_object())
                    .filter(|usage| usage.len() == 1)
                    .and_then(|usage| usage.keys().next().cloned())
            })
    });
    ClaudeReviewView {
        model_requested: record.request.recipe.model.clone(),
        model_observed,
        status: super::step_state_name(state(record)).into(),
        stop: Some(enum_name(&record.stop)),
        findings: record
            .findings
            .iter()
            .map(|finding| {
                let citation = finding.claim.citations.first();
                let path = citation
                    .and_then(|citation| {
                        record
                            .coverage
                            .iter()
                            .find(|row| row.row_id == citation.row_id)
                    })
                    .map(|row| row.selector.path.clone());
                ClaudeReviewFinding {
                    path,
                    start_line: citation.map(|c| u64::from(c.start_line)),
                    end_line: citation.map(|c| u64::from(c.end_line)),
                    title: finding.claim.title.clone(),
                    detail: finding.claim.detail.clone(),
                    severity: enum_name(&finding.claim.severity),
                }
            })
            .collect(),
        unresolved: record.unresolved.clone(),
        covered_paths: record
            .coverage
            .iter()
            .filter(|row| row.exposure == RowExposure::Acknowledged)
            .map(|row| row.selector.path.clone())
            .collect(),
    }
}

fn enum_name(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}
