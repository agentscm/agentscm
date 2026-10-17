use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::contracts::*;
use super::findings::assign_new_finding;
use super::wire::prepare_call;
use crate::{Error, Result};

pub struct ReviewController;

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
            let mut call = match prepare_call(&record.request, &rows, remaining_ms) {
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
