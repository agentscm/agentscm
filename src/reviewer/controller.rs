use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::contracts::*;
use super::findings::assign_new_finding;
use super::wire::prepare_call;
use crate::{Error, Result};

pub struct ReviewController;

/// Replays a retained finite trace without provider dispatch. The same controller rebuilds
/// prepared requests, source exposure, coverage, findings and the terminal outcome. Imported
/// success flags are never accepted independently of this exact replay.
pub async fn validate_replayed_record<S: ReviewSource>(
    expected_request: &ReviewRequest,
    source: &mut S,
    imported: &ReviewRecord,
) -> Result<()> {
    if &imported.request != expected_request
        || imported.request_sha256 != expected_request.digest()?
        || imported.calls.is_empty()
    {
        return Err(Error::Invalid(
            "imported review does not bind the expected finite request".into(),
        ));
    }
    let basis = imported.calls[0].reservation.transport_basis;
    if imported
        .calls
        .iter()
        .any(|call| call.reservation.transport_basis != basis)
    {
        return Err(Error::Invalid(
            "imported review changes transport basis between calls".into(),
        ));
    }
    let transport = ReplayTransport {
        scope: expected_request.recipe.provider_scope_sha256.clone(),
        basis,
        calls: imported.calls.clone(),
        next: std::sync::Mutex::new(0),
    };
    let mut journal = ReplayJournal {
        calls: &imported.calls,
        next: 0,
    };
    let replayed = ReviewController::run(
        expected_request.clone(),
        source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await?;
    if replayed != *imported
        || journal.next != imported.calls.len()
        || *transport
            .next
            .lock()
            .map_err(|_| Error::Invalid("review replay transport lock poisoned".into()))?
            != imported.calls.len()
    {
        return Err(Error::Invalid(
            "imported review trace does not replay to its claimed record".into(),
        ));
    }
    Ok(())
}

struct ReplayTransport {
    scope: String,
    basis: TransportBasis,
    calls: Vec<RecordedModelCall>,
    next: std::sync::Mutex<usize>,
}

impl ModelTransport for ReplayTransport {
    fn scope_sha256(&self) -> String {
        self.scope.clone()
    }

    fn basis(&self) -> TransportBasis {
        self.basis
    }

    fn invoke<'a>(
        &'a self,
        call: &'a PreparedModelCall,
        _: &'a ReviewCancellation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelCallObservation> + Send + 'a>>
    {
        Box::pin(async move {
            let mut next = self.next.lock().expect("replay transport lock");
            let Some(expected) = self.calls.get(*next) else {
                return ModelCallObservation::not_started(
                    self.basis,
                    ModelFailure::InvalidResponse,
                );
            };
            *next += 1;
            if expected.reservation.request_sha256 != call.request_sha256()
                || expected.reservation.request_body != call.body()
            {
                return ModelCallObservation::not_started(
                    self.basis,
                    ModelFailure::InvalidResponse,
                );
            }
            expected.observation.clone()
        })
    }
}

struct ReplayJournal<'a> {
    calls: &'a [RecordedModelCall],
    next: usize,
}

impl ReviewJournal for ReplayJournal<'_> {
    fn reserve_call(&mut self, reservation: &CallReservation) -> Result<ReservationOutcome> {
        let expected = self
            .calls
            .get(self.next)
            .ok_or_else(|| Error::Invalid("review replay has an unexpected call".into()))?;
        if expected.reservation != *reservation
            || expected.ticket.reservation_sha256 != reservation.digest()?
            || expected.ticket.run_generation != reservation.run_generation
            || expected.ticket.durable_version == 0
        {
            return Err(Error::Invalid(
                "review replay reservation or ticket differs from the signed trace".into(),
            ));
        }
        Ok(ReservationOutcome::Reserved(expected.ticket.clone()))
    }

    fn record_call(
        &mut self,
        ticket: &CallTicket,
        observation: &ModelCallObservation,
    ) -> Result<()> {
        let expected = self
            .calls
            .get(self.next)
            .ok_or_else(|| Error::Invalid("review replay observation has no reservation".into()))?;
        self.next += 1;
        if expected.ticket != *ticket || expected.observation != *observation {
            return Err(Error::Invalid(
                "review replay observation differs from the signed trace".into(),
            ));
        }
        if expected.journal_recorded {
            Ok(())
        } else {
            Err(Error::Conflict(
                "review replay preserves an unrecorded observation".into(),
            ))
        }
    }
}

impl ReviewController {
    /// Records finite observations, never approval or trusted execution success.
    /// C owns durable authorization, reservations, retention and terminal CAS.
    pub async fn run<S: ReviewSource, J: ReviewJournal, T: ModelTransport>(
        mut request: ReviewRequest,
        source: &mut S,
        journal: &mut J,
        transport: &T,
        cancel: &ReviewCancellation,
    ) -> Result<ReviewRecord> {
        request.validate()?;
        if transport.scope_sha256() != request.recipe.provider_scope_sha256 {
            return Err(Error::Invalid(
                "model endpoint scope does not match review recipe".into(),
            ));
        }
        request.recipe = serde_json::from_slice(&request.recipe.canonical_bytes()?)?;
        let coverage = request
            .recipe
            .source
            .iter()
            .map(|selector| {
                Ok(ReviewCoverage {
                    row_id: selector.row_id()?,
                    selector: selector.clone(),
                    exposure: RowExposure::Unread,
                    read_bytes: 0,
                    source_observation: None,
                    read_failure: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut record = ReviewRecord {
            version: 1,
            request_sha256: request.digest()?,
            request,
            completion: ReviewCompletion::Incomplete,
            stop: ReviewStop::CallsExhausted,
            qualification: ReviewQualification::Unverified,
            coverage,
            calls: Vec::new(),
            unconfirmed_reservation: None,
            findings: Vec::new(),
            unresolved: Vec::new(),
        };
        let started = Instant::now();
        let timeout = Duration::from_millis(u64::from(record.request.recipe.limits.timeout_ms));
        let mut rows = Vec::new();
        let mut source_bytes = 0usize;
        let mut request_bytes = 0usize;
        let mut total_tokens = 0u64;
        for call_index in 0..record.request.recipe.limits.max_calls {
            if stop_for_clock(&mut record, started, timeout, cancel) {
                break;
            }
            let remaining_ms = timeout.saturating_sub(started.elapsed()).as_millis().max(1) as u32;
            let mut call =
                match prepare_call(&record.request, &rows, remaining_ms, transport.basis()) {
                    Ok(call) => call,
                    Err(_) => {
                        record.stop = ReviewStop::RequestLimit;
                        break;
                    }
                };
            request_bytes = request_bytes.saturating_add(call.body().len());
            if request_bytes > record.request.recipe.limits.max_total_request_bytes as usize {
                record.stop = ReviewStop::RequestLimit;
                break;
            }
            let reservation = CallReservation {
                attempt_id: record.request.attempt_id.clone(),
                run_generation: record.request.run_generation,
                call_index,
                subject: record.request.subject.clone(),
                provider_scope_sha256: record.request.recipe.provider_scope_sha256.clone(),
                model: record.request.recipe.model.clone(),
                transport_basis: transport.basis(),
                qualification: ReviewQualification::Unverified,
                review_request_sha256: record.request_sha256.clone(),
                request_sha256: call.request_sha256(),
                request_body: call.body().to_vec(),
                source_row_ids: rows.iter().map(|row| row.row_id.clone()).collect(),
                max_input_tokens: record.request.recipe.limits.max_input_tokens,
                max_output_tokens: record.request.recipe.limits.max_output_tokens,
                max_cost_micros: record.request.recipe.limits.max_cost_micros,
            };
            let ticket = match journal.reserve_call(&reservation) {
                Ok(ReservationOutcome::Reserved(ticket))
                    if ticket.reservation_sha256 == reservation.digest()?
                        && ticket.run_generation == record.request.run_generation
                        && ticket.durable_version > 0 =>
                {
                    ticket
                }
                Ok(ReservationOutcome::Existing) => {
                    record.unconfirmed_reservation = Some(reservation);
                    record.stop = ReviewStop::ExistingReservation;
                    break;
                }
                _ => {
                    record.unconfirmed_reservation = Some(reservation);
                    record.stop = ReviewStop::Journal;
                    break;
                }
            };
            // The journal has already recorded possible exposure. Mark before invoke
            // so an interrupted send cannot become an undisclosed/unbilled claim.
            for coverage in &mut record.coverage {
                if coverage.exposure == RowExposure::Prepared {
                    coverage.exposure = RowExposure::PossiblyDisclosed;
                }
            }
            let mut observation = if cancel.is_cancelled() {
                ModelCallObservation::not_started(transport.basis(), ModelFailure::Cancelled)
            } else if started.elapsed() >= timeout {
                ModelCallObservation::not_started(transport.basis(), ModelFailure::Deadline)
            } else {
                call.timeout_ms =
                    timeout.saturating_sub(started.elapsed()).as_millis().max(1) as u32;
                let cancelled = async {
                    loop {
                        if cancel.is_cancelled() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                };
                // Poll a completed transport result first to retain its partial
                // capture. A pending adapter cannot bypass the original deadline.
                tokio::select! {
                    biased;
                    observation = transport.invoke(&call, cancel) => observation,
                    _ = cancelled => interrupted_call(transport.basis(), ModelFailure::Cancelled),
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(started + timeout)) => {
                        interrupted_call(transport.basis(), ModelFailure::Deadline)
                    }
                }
            };
            if !observation.consistent_with(&call, transport.basis()) {
                observation.reject_inconsistent(call.max_response_bytes);
            }
            let journal_recorded = journal.record_call(&ticket, &observation).is_ok();
            record.calls.push(RecordedModelCall {
                reservation,
                ticket,
                observation,
                journal_recorded,
            });
            if !journal_recorded {
                record.stop = ReviewStop::Journal;
                break;
            }
            let observation = &record
                .calls
                .last()
                .expect("call was just recorded")
                .observation;
            let Some(reply) = observation.reply.as_ref().filter(|_| {
                observation.state == CallState::Acknowledged && observation.failure.is_none()
            }) else {
                match observation.failure {
                    Some(ModelFailure::Cancelled) => {
                        record.completion = ReviewCompletion::Cancelled;
                        record.stop = ReviewStop::Cancelled;
                    }
                    Some(ModelFailure::Deadline) => record.stop = ReviewStop::Deadline,
                    Some(ModelFailure::OutputLimit) => record.stop = ReviewStop::TokenLimit,
                    _ => record.stop = ReviewStop::Transport,
                }
                break;
            };
            // A trusted transport's acknowledgement is a server observation, not
            // proof of model attention, exact weights, semantics or enforceable caps.
            let reply = reply.clone();
            if observation.basis != transport.basis()
                || reply.model != record.request.recipe.model
                || reply
                    .usage
                    .input_tokens
                    .checked_add(reply.usage.output_tokens)
                    != Some(reply.usage.total_tokens)
            {
                record.stop = ReviewStop::Transport;
                break;
            }
            for coverage in &mut record.coverage {
                if matches!(
                    coverage.exposure,
                    RowExposure::Prepared | RowExposure::PossiblyDisclosed
                ) {
                    coverage.exposure = RowExposure::Acknowledged;
                }
            }
            total_tokens = total_tokens.saturating_add(reply.usage.total_tokens);
            if reply.usage.input_tokens > record.request.recipe.limits.max_input_tokens
                || reply.usage.output_tokens
                    > u64::from(record.request.recipe.limits.max_output_tokens)
                || total_tokens > record.request.recipe.limits.max_total_tokens
            {
                record.stop = ReviewStop::TokenLimit;
                break;
            }
            if stop_for_clock(&mut record, started, timeout, cancel) {
                break;
            }
            match reply.decision {
                ReviewDecision::RequestRows { row_ids } => {
                    if call_index + 1 == record.request.recipe.limits.max_calls {
                        break;
                    }
                    if let Err(stop) = load_rows(
                        &mut record,
                        source,
                        &mut rows,
                        &mut source_bytes,
                        &row_ids,
                        started,
                        timeout,
                        cancel,
                    ) {
                        record.stop = stop;
                        if stop == ReviewStop::Cancelled {
                            record.completion = ReviewCompletion::Cancelled;
                        }
                        break;
                    }
                }
                ReviewDecision::Finish {
                    findings,
                    unresolved,
                } => {
                    if findings.len() > MAX_FINDINGS
                        || unresolved.len() > MAX_ROWS
                        || unresolved
                            .iter()
                            .any(|v| v.trim().is_empty() || v.len() > 2048)
                    {
                        record.stop = ReviewStop::InvalidDecision;
                        break;
                    }
                    let ranges = record
                        .coverage
                        .iter()
                        .filter(|row| row.exposure == RowExposure::Acknowledged)
                        .map(|row| {
                            (
                                row.row_id.clone(),
                                row.selector.start_line,
                                row.selector.end_line,
                            )
                        })
                        .collect::<Vec<_>>();
                    let mut accepted = Vec::new();
                    for (ordinal, claim) in findings.into_iter().enumerate() {
                        if claim.validate(&ranges).is_err() {
                            record.stop = ReviewStop::InvalidFinding;
                            return Ok(record);
                        }
                        let identity = assign_new_finding(
                            &record.request.subject,
                            &claim,
                            &record.request.attempt_id,
                            record.request.run_generation,
                            u64::from(call_index),
                            ordinal as u64,
                        )?;
                        accepted.push(ReviewFinding { identity, claim });
                    }
                    record.findings = accepted;
                    record.unresolved = unresolved;
                    if record.coverage.iter().any(|row| {
                        row.selector.required && row.exposure != RowExposure::Acknowledged
                    }) {
                        record.stop = ReviewStop::RequiredScopeUnvisited;
                    } else if !record.unresolved.is_empty() {
                        record.stop = ReviewStop::UnresolvedScope;
                    } else {
                        record.completion = ReviewCompletion::FinishedObservations;
                        record.stop = ReviewStop::ModelFinished;
                    }
                    break;
                }
            }
        }
        Ok(record)
    }
}

fn interrupted_call(basis: TransportBasis, failure: ModelFailure) -> ModelCallObservation {
    // The adapter may already have disclosed the request or charged for work.
    // Dropping its pending future cannot establish a not-started outcome.
    let mut observation = ModelCallObservation::not_started(basis, failure);
    observation.state = CallState::Uncertain;
    observation
}

fn stop_for_clock(
    record: &mut ReviewRecord,
    started: Instant,
    timeout: Duration,
    cancel: &ReviewCancellation,
) -> bool {
    if cancel.is_cancelled() {
        record.completion = ReviewCompletion::Cancelled;
        record.stop = ReviewStop::Cancelled;
        true
    } else if started.elapsed() >= timeout {
        record.stop = ReviewStop::Deadline;
        true
    } else {
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn load_rows<S: ReviewSource>(
    record: &mut ReviewRecord,
    source: &mut S,
    rows: &mut Vec<ReviewSourceRow>,
    source_bytes: &mut usize,
    row_ids: &[String],
    started: Instant,
    timeout: Duration,
    cancel: &ReviewCancellation,
) -> std::result::Result<(), ReviewStop> {
    if row_ids.is_empty() || row_ids.len() > MAX_ROWS {
        return Err(ReviewStop::InvalidDecision);
    }
    let mut seen = HashSet::new();
    for id in row_ids {
        if !seen.insert(id) {
            return Err(ReviewStop::InvalidDecision);
        }
        let Some(coverage) = record.coverage.iter_mut().find(|row| &row.row_id == id) else {
            return Err(ReviewStop::InvalidDecision);
        };
        if coverage.exposure != RowExposure::Unread {
            return Err(ReviewStop::InvalidDecision);
        }
        if cancel.is_cancelled() {
            return Err(ReviewStop::Cancelled);
        }
        if started.elapsed() >= timeout {
            return Err(ReviewStop::Deadline);
        }
        let remaining = (record.request.recipe.limits.max_source_bytes as usize)
            .saturating_sub(*source_bytes)
            .min(MAX_ROW_BYTES);
        if remaining == 0 {
            return Err(ReviewStop::SourceLimit);
        }
        let observation = source
            .read(&record.request, &coverage.selector, remaining)
            .map_err(|failure| {
                coverage.read_failure = Some(failure);
                if failure == SourceReadFailure::Limit {
                    ReviewStop::SourceLimit
                } else {
                    ReviewStop::SourceRead
                }
            })?;
        coverage.exposure = RowExposure::Read;
        coverage.read_bytes = observation.raw.len() as u64;
        if observation.source.validate().is_err() {
            return Err(ReviewStop::SourceMismatch);
        }
        coverage.source_observation = Some(ReviewSourceMetadata {
            source: observation.source.clone(),
            raw_bytes: observation.raw.len() as u64,
            raw_sha256: None,
            rendered_bytes: observation.rendered.len() as u64,
            rendered_sha256: None,
            rendering: observation.rendering,
        });
        if observation.raw.len() > remaining || observation.rendered.len() > remaining {
            return Err(ReviewStop::SourceLimit);
        }
        let raw_digest = hash_bytes(&observation.raw);
        let rendered_digest = hash_bytes(observation.rendered.as_bytes());
        if let Some(metadata) = &mut coverage.source_observation {
            metadata.raw_sha256 = Some(raw_digest.clone());
            metadata.rendered_sha256 = Some(rendered_digest.clone());
        }
        if observation.source != coverage.selector.source {
            return Err(ReviewStop::SourceMismatch);
        }
        if observation.rendering != SourceRendering::ExactUtf8 {
            return Err(ReviewStop::SourceRendering);
        }
        let text = observation.rendered;
        if text.is_empty()
            || std::str::from_utf8(&observation.raw).ok() != Some(text.as_str())
            || text.lines().count() as u32
                != coverage.selector.end_line - coverage.selector.start_line + 1
            || raw_digest != coverage.selector.raw_sha256
            || rendered_digest != coverage.selector.rendered_sha256
        {
            return Err(ReviewStop::SourceMismatch);
        }
        *source_bytes += text.len();
        coverage.exposure = RowExposure::Prepared;
        rows.push(ReviewSourceRow {
            row_id: id.clone(),
            selector: coverage.selector.clone(),
            text,
        });
    }
    Ok(())
}

#[cfg(test)]
mod replay_tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::checks::CheckSubject;
    use crate::context::ChangeId;

    struct Source(ReviewSourceObservation);

    impl ReviewSource for Source {
        fn read(
            &mut self,
            _: &ReviewRequest,
            _: &ReviewSourceSelector,
            _: usize,
        ) -> std::result::Result<ReviewSourceObservation, SourceReadFailure> {
            Ok(self.0.clone())
        }
    }

    struct Transport {
        decisions: Mutex<VecDeque<ReviewDecision>>,
        scope: String,
    }

    impl ModelTransport for Transport {
        fn scope_sha256(&self) -> String {
            self.scope.clone()
        }
        fn basis(&self) -> TransportBasis {
            TransportBasis::Fixture
        }
        fn invoke<'a>(
            &'a self,
            call: &'a PreparedModelCall,
            _: &'a ReviewCancellation,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelCallObservation> + Send + 'a>>
        {
            Box::pin(async move {
                let decision = self.decisions.lock().unwrap().pop_front().unwrap();
                let reply = ModelReply {
                    response_id: format!("reply-{}", call.request_sha256()),
                    model: "fixture-model".into(),
                    usage: ReviewUsage {
                        basis: UsageBasis::Fixture,
                        input_tokens: 2,
                        output_tokens: 1,
                        total_tokens: 3,
                        prompt_details: None,
                        completion_details: None,
                    },
                    content_sha256: hash_bytes(&serde_json::to_vec(&decision).unwrap()),
                    decision,
                    system_fingerprint: None,
                    service_tier: None,
                };
                let captured_response = serde_json::to_vec(&reply).unwrap();
                ModelCallObservation {
                    state: CallState::Acknowledged,
                    basis: TransportBasis::Fixture,
                    reply: Some(reply),
                    failure: None,
                    http_status: None,
                    captured_response_bytes: captured_response.len() as u64,
                    captured_response_sha256: hash_bytes(&captured_response),
                    captured_response,
                    response_capture_complete: true,
                }
            })
        }
    }

    #[derive(Default)]
    struct Calls(Vec<RecordedModelCall>);

    impl ReviewJournal for Calls {
        fn reserve_call(&mut self, reservation: &CallReservation) -> Result<ReservationOutcome> {
            let ticket = CallTicket {
                reservation_sha256: reservation.digest()?,
                run_generation: reservation.run_generation,
                durable_version: u64::from(reservation.call_index) + 1,
            };
            self.0.push(RecordedModelCall {
                reservation: reservation.clone(),
                ticket: ticket.clone(),
                observation: ModelCallObservation::not_started(
                    TransportBasis::Fixture,
                    ModelFailure::InvalidResponse,
                ),
                journal_recorded: false,
            });
            Ok(ReservationOutcome::Reserved(ticket))
        }
        fn record_call(
            &mut self,
            ticket: &CallTicket,
            observation: &ModelCallObservation,
        ) -> Result<()> {
            let call = self.0.last_mut().unwrap();
            if call.ticket != *ticket {
                return Err(Error::Invalid("fixture ticket mismatch".into()));
            }
            call.observation = observation.clone();
            call.journal_recorded = true;
            Ok(())
        }
    }

    fn fixture() -> (ReviewRequest, Source, Transport) {
        let binding = ReviewSourceBinding {
            profile: "fixture/v1".into(),
            permit_sha256: "1".repeat(64),
        };
        let raw = b"line\n".to_vec();
        let selector = ReviewSourceSelector {
            source: binding.clone(),
            path: "src/lib.rs".into(),
            start_line: 1,
            end_line: 1,
            raw_sha256: hash_bytes(&raw),
            rendered_sha256: hash_bytes(&raw),
            required: true,
        };
        let scope = "2".repeat(64);
        let recipe = ReviewRecipe {
            version: 1,
            model: "fixture-model".into(),
            objective: "inspect the selected source".into(),
            provider_scope_sha256: scope.clone(),
            installed_policy_sha256: "3".repeat(64),
            policy_epoch: 1,
            source: vec![selector.clone()],
            limits: ReviewLimits {
                max_source_bytes: 4096,
                max_request_bytes: 32 * 1024,
                max_total_request_bytes: 64 * 1024,
                max_response_bytes: 4096,
                max_calls: 2,
                max_input_tokens: 100,
                max_output_tokens: 100,
                max_total_tokens: 100,
                max_cost_micros: Some(100),
                timeout_ms: 5_000,
            },
        };
        let request = ReviewRequest {
            attempt_id: "attempt-1".into(),
            run_generation: 1,
            subject: CheckSubject {
                repository: "a".repeat(32),
                change: ChangeId {
                    repository: "a".repeat(32),
                    id: "change-1".into(),
                },
                change_version: 1,
                context_digest: "4".repeat(64),
                revision: "5".repeat(64),
                recipe_sha256: recipe.digest().unwrap(),
                toolchain_sha256: "6".repeat(64),
                environment_sha256: scope.clone(),
            },
            recipe,
        };
        let row = selector.row_id().unwrap();
        (
            request,
            Source(ReviewSourceObservation {
                source: binding,
                raw,
                rendered: "line\n".into(),
                rendering: SourceRendering::ExactUtf8,
            }),
            Transport {
                decisions: Mutex::new(VecDeque::from([
                    ReviewDecision::RequestRows { row_ids: vec![row] },
                    ReviewDecision::Finish {
                        findings: vec![],
                        unresolved: vec![],
                    },
                ])),
                scope,
            },
        )
    }

    #[tokio::test]
    async fn deterministic_replay_rebuilds_coverage_and_rejects_tampering() {
        let (request, mut source, transport) = fixture();
        let mut calls = Calls::default();
        let record = ReviewController::run(
            request.clone(),
            &mut source,
            &mut calls,
            &transport,
            &ReviewCancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(record.completion, ReviewCompletion::FinishedObservations);
        let (_, mut replay_source, _) = fixture();
        validate_replayed_record(&request, &mut replay_source, &record)
            .await
            .unwrap();
        let mut altered = record;
        altered.coverage[0].read_bytes += 1;
        let (_, mut replay_source, _) = fixture();
        assert!(
            validate_replayed_record(&request, &mut replay_source, &altered)
                .await
                .is_err()
        );
    }
}
