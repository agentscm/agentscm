use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentscm::checks::CheckSubject;
use agentscm::context::ChangeId;
use agentscm::reviewer::{
    CallReservation, CallState, CallTicket, FindingClaim, FindingSeverity, ModelCallObservation,
    ModelFailure, ModelReply, ModelTransport, PreparedModelCall, ReservationOutcome,
    ReviewCancellation, ReviewCompletion, ReviewController, ReviewDecision, ReviewJournal,
    ReviewLimits, ReviewRecipe, ReviewRequest, ReviewSource, ReviewSourceBinding,
    ReviewSourceObservation, ReviewSourceSelector, ReviewStop, ReviewUsage, RowExposure,
    SourceCitation, SourceReadFailure, SourceRendering, TransportBasis, UsageBasis,
};
use agentscm::{Error, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PROVIDER_SCOPE: &str = "9999999999999999999999999999999999999999999999999999999999999999";
const HOST_ONLY_SENTINEL: &str = "HOST_ONLY_CONTENT_MUST_NOT_BE_DISCLOSED";

struct Harness {
    request: ReviewRequest,
    source: FixtureSource,
    approved_rows: HashMap<String, String>,
    row_ids: Vec<String>,
}

impl Harness {
    fn new(required: &[bool]) -> Self {
        let mut selectors = Vec::new();
        let mut source_text = HashMap::new();
        for (index, required) in required.iter().copied().enumerate() {
            let text = format!("fixture source row {}\n", index + 1);
            let line = 10 + index as u32;
            let selector = ReviewSourceSelector {
                source: ReviewSourceBinding {
                    profile: "fixture-source-v1".into(),
                    permit_sha256: "7".repeat(64),
                },
                path: format!("src/fixture-{}.rs", index + 1),
                start_line: line,
                end_line: line,
                raw_sha256: digest(text.as_bytes()),
                rendered_sha256: digest(text.as_bytes()),
                required,
            };
            source_text.insert(selector.path.clone(), text);
            selectors.push(selector);
        }
        let limits = ReviewLimits {
            max_source_bytes: 64 * 1024,
            max_request_bytes: 64 * 1024,
            max_total_request_bytes: 256 * 1024,
            max_response_bytes: 32 * 1024,
            max_calls: 4,
            max_input_tokens: 1_000,
            max_output_tokens: 100,
            max_total_tokens: 2_000,
            max_cost_micros: Some(10_000),
            timeout_ms: 5_000,
        };
        let recipe = ReviewRecipe {
            version: 1,
            model: "fixture-model".into(),
            objective: "Find the seeded authorization defect.".into(),
            provider_scope_sha256: PROVIDER_SCOPE.into(),
            installed_policy_sha256: "8".repeat(64),
            policy_epoch: 3,
            source: selectors,
            limits,
        };
        let mut subject = CheckSubject {
            repository: "repository-1".into(),
            change: ChangeId {
                repository: "repository-1".into(),
                id: "change-1".into(),
            },
            change_version: 2,
            context_digest: "1".repeat(64),
            revision: "2".repeat(64),
            recipe_sha256: String::new(),
            toolchain_sha256: "3".repeat(64),
            environment_sha256: "4".repeat(64),
        };
        subject.recipe_sha256 = recipe.digest().expect("valid fixture recipe");
        let request = ReviewRequest {
            attempt_id: "fixture-attempt-1".into(),
            run_generation: 7,
            subject,
            recipe,
        };
        let row_ids = request
            .recipe
            .source
            .iter()
            .map(|selector| selector.row_id().expect("valid fixture selector"))
            .collect::<Vec<_>>();
        let approved_rows = row_ids
            .iter()
            .zip(&request.recipe.source)
            .map(|(row_id, selector)| {
                (
                    row_id.clone(),
                    source_text
                        .get(&selector.path)
                        .expect("fixture text")
                        .clone(),
                )
            })
            .collect();
        Self {
            request,
            source: FixtureSource {
                text: source_text,
                reads: Vec::new(),
            },
            approved_rows,
            row_ids,
        }
    }
}

/// Synthetic bounded source. It never reads the filesystem or another backend.
struct FixtureSource {
    text: HashMap<String, String>,
    reads: Vec<String>,
}

impl ReviewSource for FixtureSource {
    fn read(
        &mut self,
        request: &ReviewRequest,
        selector: &ReviewSourceSelector,
        max_bytes: usize,
    ) -> std::result::Result<ReviewSourceObservation, SourceReadFailure> {
        assert_eq!(request.attempt_id, "fixture-attempt-1");
        let text = self
            .text
            .get(&selector.path)
            .ok_or(SourceReadFailure::Missing)?;
        if text.len() > max_bytes {
            return Err(SourceReadFailure::Limit);
        }
        self.reads.push(selector.path.clone());
        Ok(ReviewSourceObservation {
            source: selector.source.clone(),
            raw: text.as_bytes().to_vec(),
            rendered: text.clone(),
            rendering: SourceRendering::ExactUtf8,
        })
    }
}

/// Typed source override for failure-path fixtures; still entirely in memory.
#[derive(Clone)]
struct ObservationSource(std::result::Result<ReviewSourceObservation, SourceReadFailure>);

impl ReviewSource for ObservationSource {
    fn read(
        &mut self,
        _: &ReviewRequest,
        _: &ReviewSourceSelector,
        _: usize,
    ) -> std::result::Result<ReviewSourceObservation, SourceReadFailure> {
        self.0.clone()
    }
}

struct ScriptStep {
    expected_rows: Vec<String>,
    observation: ModelCallObservation,
    cancel_before_return: bool,
}

/// In-memory fixture transport. It performs no inference and no network I/O.
struct ScriptedFixtureTransport {
    steps: Mutex<VecDeque<ScriptStep>>,
    approved_rows: HashMap<String, String>,
    invocations: AtomicUsize,
}

impl ScriptedFixtureTransport {
    fn new(approved_rows: HashMap<String, String>, steps: Vec<ScriptStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into_iter().collect()),
            approved_rows,
            invocations: AtomicUsize::new(0),
        }
    }

    fn invocation_count(&self) -> usize {
        self.invocations.load(Ordering::SeqCst)
    }

    fn validate_call(&self, call: &PreparedModelCall, expected_rows: &[String]) {
        let body: Value = serde_json::from_slice(call.body()).expect("fixture request JSON");
        assert_eq!(body["model"], "fixture-model");
        assert_eq!(body["stream"], false);
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["tool_choice"], "none");
        assert_eq!(body["n"], 1);
        assert!(
            body["max_completion_tokens"]
                .as_u64()
                .is_some_and(|value| (1..=16_384).contains(&value))
        );
        assert!(call.timeout_ms() > 0);

        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        let input: Value = serde_json::from_str(
            messages[1]["content"]
                .as_str()
                .expect("serialized review input"),
        )
        .expect("review input JSON");
        assert_eq!(input["protocol"], "agentscm.model-review/v1");
        assert!(input.get("subject").is_none());
        assert!(!input.to_string().contains("change-1"));
        assert!(!input.to_string().contains(HOST_ONLY_SENTINEL));
        for row in input["catalog"].as_array().expect("catalog") {
            let selector = row["selector"].as_object().expect("wire location");
            assert_eq!(selector.len(), 4);
            assert!(selector.contains_key("path"));
            assert!(selector.contains_key("start_line"));
            assert!(selector.contains_key("end_line"));
            assert!(selector.contains_key("required"));
        }
        assert!(!input.to_string().contains("permit_sha256"));
        assert!(!input.to_string().contains("raw_sha256"));
        assert!(!input.to_string().contains("rendered_sha256"));

        let supplied = input["supplied_rows"]
            .as_array()
            .expect("supplied rows array");
        let actual_ids = supplied
            .iter()
            .map(|row| row["row_id"].as_str().expect("row ID").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(actual_ids, expected_rows);
        for row in supplied {
            let row_id = row["row_id"].as_str().expect("row ID");
            let text = row["text"].as_str().expect("row text");
            assert_eq!(
                Some(text),
                self.approved_rows.get(row_id).map(String::as_str),
                "transport received text outside the approved fixture row"
            );
        }
    }
}

impl ModelTransport for ScriptedFixtureTransport {
    fn scope_sha256(&self) -> String {
        PROVIDER_SCOPE.into()
    }

    fn basis(&self) -> TransportBasis {
        TransportBasis::Fixture
    }

    fn invoke<'a>(
        &'a self,
        call: &'a PreparedModelCall,
        cancel: &'a ReviewCancellation,
    ) -> Pin<Box<dyn Future<Output = ModelCallObservation> + Send + 'a>> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        let step = self
            .steps
            .lock()
            .expect("fixture script lock")
            .pop_front()
            .expect("unexpected fixture invocation");
        self.validate_call(call, &step.expected_rows);
        if step.cancel_before_return {
            cancel.cancel();
        }
        Box::pin(async move { step.observation })
    }
}

/// In-memory fixture journal. It is not durable authority or authorization.
#[derive(Default)]
struct FixtureJournal {
    reservations: Vec<CallReservation>,
    record_attempts: Vec<(CallTicket, ModelCallObservation)>,
    fail_reservation: bool,
    wrong_generation: bool,
    fail_record: bool,
}

impl ReviewJournal for FixtureJournal {
    fn reserve_call(&mut self, reservation: &CallReservation) -> Result<ReservationOutcome> {
        if self.reservations.iter().any(|old| {
            old.attempt_id == reservation.attempt_id
                && old.run_generation == reservation.run_generation
                && old.call_index == reservation.call_index
        }) {
            return Ok(ReservationOutcome::Existing);
        }
        self.reservations.push(reservation.clone());
        if self.fail_reservation {
            return Err(Error::Conflict("fixture reservation rejected".into()));
        }
        Ok(ReservationOutcome::Reserved(CallTicket {
            reservation_sha256: reservation.digest()?,
            run_generation: if self.wrong_generation {
                reservation.run_generation + 1
            } else {
                reservation.run_generation
            },
            durable_version: 1,
        }))
    }

    fn record_call(
        &mut self,
        ticket: &CallTicket,
        observation: &ModelCallObservation,
    ) -> Result<()> {
        self.record_attempts
            .push((ticket.clone(), observation.clone()));
        if self.fail_record {
            Err(Error::Conflict("fixture terminal record rejected".into()))
        } else {
            Ok(())
        }
    }
}

fn step(expected_rows: Vec<String>, observation: ModelCallObservation) -> ScriptStep {
    ScriptStep {
        expected_rows,
        observation,
        cancel_before_return: false,
    }
}

fn acknowledged(
    decision: ReviewDecision,
    input_tokens: u64,
    output_tokens: u64,
) -> ModelCallObservation {
    let content_sha256 = digest(&serde_json::to_vec(&decision).expect("fixture decision JSON"));
    let reply = ModelReply {
        response_id: "fixture-response".into(),
        model: "fixture-model".into(),
        usage: ReviewUsage {
            basis: UsageBasis::Fixture,
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
            prompt_details: None,
            completion_details: None,
        },
        decision,
        content_sha256,
        system_fingerprint: None,
        service_tier: None,
    };
    let captured_response = serde_json::to_vec(&reply).expect("fixture reply envelope");
    ModelCallObservation {
        state: CallState::Acknowledged,
        basis: TransportBasis::Fixture,
        reply: Some(reply),
        failure: None,
        http_status: Some(200),
        captured_response_bytes: captured_response.len() as u64,
        captured_response_sha256: digest(&captured_response),
        captured_response,
        response_capture_complete: true,
    }
}

fn uncertain_observation() -> ModelCallObservation {
    let captured_response = b"partial fixture response".to_vec();
    ModelCallObservation {
        state: CallState::Uncertain,
        basis: TransportBasis::Fixture,
        reply: None,
        failure: Some(ModelFailure::Connection),
        http_status: None,
        captured_response_bytes: captured_response.len() as u64,
        captured_response_sha256: digest(&captured_response),
        captured_response,
        response_capture_complete: false,
    }
}

fn request_rows(row_ids: Vec<String>) -> ReviewDecision {
    ReviewDecision::RequestRows { row_ids }
}

fn finish(findings: Vec<FindingClaim>) -> ReviewDecision {
    ReviewDecision::Finish {
        findings,
        unresolved: Vec::new(),
    }
}

fn cited_finding(row_id: &str, selector: &ReviewSourceSelector) -> FindingClaim {
    FindingClaim {
        title: "Seeded authorization bypass".into(),
        detail: "The fixture source allows the protected action without its guard.".into(),
        severity: FindingSeverity::Error,
        citations: vec![SourceCitation {
            row_id: row_id.into(),
            start_line: selector.start_line,
            end_line: selector.end_line,
        }],
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn source_observation(
    selector: &ReviewSourceSelector,
    raw: Vec<u8>,
    rendered: String,
    rendering: SourceRendering,
) -> ReviewSourceObservation {
    ReviewSourceObservation {
        source: selector.source.clone(),
        raw,
        rendered,
        rendering,
    }
}

async fn run_single_source_observation(
    harness: Harness,
    observation: ReviewSourceObservation,
) -> (agentscm::reviewer::ReviewRecord, FixtureJournal, usize) {
    let row_id = harness.request.recipe.source[0]
        .row_id()
        .expect("fixture row ID");
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows,
        vec![step(
            Vec::new(),
            acknowledged(request_rows(vec![row_id]), 7, 2),
        )],
    );
    let mut journal = FixtureJournal::default();
    let mut source = ObservationSource(Ok(observation));
    let record = ReviewController::run(
        harness.request,
        &mut source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic source observation");
    (record, journal, transport.invocation_count())
}

#[tokio::test]
async fn seeded_defect_is_recorded_after_the_cited_row_is_delivered() {
    let mut harness = Harness::new(&[true]);
    let row_id = harness.row_ids[0].clone();
    let finding = cited_finding(&row_id, &harness.request.recipe.source[0]);
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![
            step(
                Vec::new(),
                acknowledged(request_rows(vec![row_id.clone()]), 10, 3),
            ),
            step(vec![row_id], acknowledged(finish(vec![finding]), 12, 6)),
        ],
    );
    let mut journal = FixtureJournal::default();

    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic review");

    assert_eq!(record.completion, ReviewCompletion::FinishedObservations);
    assert_eq!(record.stop, ReviewStop::ModelFinished);
    assert_eq!(record.findings.len(), 1);
    assert_eq!(record.calls.len(), 2);
    assert_eq!(harness.source.reads.len(), 1);
    assert_eq!(transport.invocation_count(), 2);
}

#[tokio::test]
async fn provider_request_omits_local_subject_and_change_identity() {
    let mut harness = Harness::new(&[false]);
    let private_change_id = harness.request.subject.change.id.clone();
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![step(Vec::new(), acknowledged(finish(Vec::new()), 8, 2))],
    );
    let mut journal = FixtureJournal::default();

    ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic review");

    let request_body = &journal.reservations[0].request_body;
    assert!(!String::from_utf8_lossy(request_body).contains(&private_change_id));
    let body: Value = serde_json::from_slice(request_body).expect("prepared request JSON");
    let input: Value = serde_json::from_str(
        body["messages"][1]["content"]
            .as_str()
            .expect("serialized review input"),
    )
    .expect("review input JSON");
    assert!(input.get("subject").is_none());
}

#[tokio::test]
async fn effective_guard_finishes_without_a_finding_after_required_scope() {
    let mut harness = Harness::new(&[true]);
    let row_id = harness.row_ids[0].clone();
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![
            step(
                Vec::new(),
                acknowledged(request_rows(vec![row_id.clone()]), 8, 2),
            ),
            step(vec![row_id], acknowledged(finish(Vec::new()), 9, 2)),
        ],
    );
    let mut journal = FixtureJournal::default();

    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic review");

    assert_eq!(record.completion, ReviewCompletion::FinishedObservations);
    assert_eq!(record.stop, ReviewStop::ModelFinished);
    assert!(record.findings.is_empty());
    assert_eq!(harness.source.reads.len(), 1);
}

#[tokio::test]
async fn unvisited_required_scope_stays_incomplete() {
    let mut harness = Harness::new(&[true]);
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![step(Vec::new(), acknowledged(finish(Vec::new()), 7, 2))],
    );
    let mut journal = FixtureJournal::default();

    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic review");

    assert_eq!(record.completion, ReviewCompletion::Incomplete);
    assert_eq!(record.stop, ReviewStop::RequiredScopeUnvisited);
    assert!(harness.source.reads.is_empty());
}

#[tokio::test]
async fn a_finding_cannot_cite_an_unread_catalog_row() {
    let mut harness = Harness::new(&[true, true]);
    let delivered = harness.row_ids[0].clone();
    let unread = harness.row_ids[1].clone();
    let finding = cited_finding(&unread, &harness.request.recipe.source[1]);
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![
            step(
                Vec::new(),
                acknowledged(request_rows(vec![delivered.clone()]), 8, 2),
            ),
            step(vec![delivered], acknowledged(finish(vec![finding]), 9, 4)),
        ],
    );
    let mut journal = FixtureJournal::default();

    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic review");

    assert_eq!(record.completion, ReviewCompletion::Incomplete);
    assert_eq!(record.stop, ReviewStop::InvalidFinding);
    assert!(record.findings.is_empty());
}

#[tokio::test]
async fn reservation_failure_prevents_transport_invocation() {
    let mut harness = Harness::new(&[false]);
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![step(Vec::new(), acknowledged(finish(Vec::new()), 7, 2))],
    );
    let mut journal = FixtureJournal {
        fail_reservation: true,
        ..FixtureJournal::default()
    };

    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic review");

    assert_eq!(record.stop, ReviewStop::Journal);
    assert!(record.calls.is_empty());
    assert!(record.unconfirmed_reservation.is_some());
    assert_eq!(journal.reservations.len(), 1);
    assert_eq!(transport.invocation_count(), 0);
}

#[tokio::test]
async fn uncertain_transport_result_is_recorded_once_without_retry() {
    let mut harness = Harness::new(&[false]);
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![step(Vec::new(), uncertain_observation())],
    );
    let mut journal = FixtureJournal::default();

    let record = ReviewController::run(
        harness.request.clone(),
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic review");

    assert_eq!(record.stop, ReviewStop::Transport);
    assert_eq!(record.calls.len(), 1);
    assert_eq!(record.calls[0].observation.state, CallState::Uncertain);
    assert!(record.calls[0].journal_recorded);
    assert_eq!(journal.record_attempts.len(), 1);
    assert_eq!(transport.invocation_count(), 1);
    let restarted = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic restart");
    assert_eq!(restarted.stop, ReviewStop::ExistingReservation);
    assert_eq!(restarted.completion, ReviewCompletion::Incomplete);
    assert!(restarted.unconfirmed_reservation.is_some());
    assert_eq!(transport.invocation_count(), 1);
    assert_eq!(journal.reservations.len(), 1);
    assert_eq!(journal.record_attempts.len(), 1);
}
#[tokio::test]
async fn terminal_journal_failure_retains_the_raw_call_record() {
    let mut harness = Harness::new(&[false]);
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![step(Vec::new(), acknowledged(finish(Vec::new()), 7, 2))],
    );
    let mut journal = FixtureJournal {
        fail_record: true,
        ..FixtureJournal::default()
    };

    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic review");

    assert_eq!(record.stop, ReviewStop::Journal);
    assert_eq!(record.calls.len(), 1);
    assert!(!record.calls[0].journal_recorded);
    assert!(!record.calls[0].reservation.request_body.is_empty());
    assert!(!record.calls[0].observation.captured_response.is_empty());
    assert_eq!(journal.record_attempts.len(), 1);
}

#[tokio::test]
async fn wrong_generation_ticket_prevents_invocation() {
    let mut harness = Harness::new(&[false]);
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![step(Vec::new(), acknowledged(finish(Vec::new()), 7, 2))],
    );
    let mut journal = FixtureJournal {
        wrong_generation: true,
        ..FixtureJournal::default()
    };

    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic review");

    assert_eq!(record.stop, ReviewStop::Journal);
    assert!(record.calls.is_empty());
    assert!(record.unconfirmed_reservation.is_some());
    assert_eq!(transport.invocation_count(), 0);
}

#[tokio::test]
async fn requesting_an_already_delivered_row_is_rejected() {
    let mut harness = Harness::new(&[true]);
    let row_id = harness.row_ids[0].clone();
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![
            step(
                Vec::new(),
                acknowledged(request_rows(vec![row_id.clone()]), 7, 2),
            ),
            step(
                vec![row_id.clone()],
                acknowledged(request_rows(vec![row_id]), 8, 2),
            ),
        ],
    );
    let mut journal = FixtureJournal::default();

    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("synthetic review");

    assert_eq!(record.stop, ReviewStop::InvalidDecision);
    assert_eq!(record.calls.len(), 2);
    assert_eq!(harness.source.reads.len(), 1);
}

#[tokio::test]
async fn cancellation_stops_before_invoke_and_before_a_requested_read() {
    let mut before_invoke = Harness::new(&[true]);
    let transport = ScriptedFixtureTransport::new(before_invoke.approved_rows.clone(), Vec::new());
    let mut journal = FixtureJournal::default();
    let cancelled = ReviewCancellation::default();
    cancelled.cancel();

    let record = ReviewController::run(
        before_invoke.request,
        &mut before_invoke.source,
        &mut journal,
        &transport,
        &cancelled,
    )
    .await
    .expect("pre-invoke cancellation");
    assert_eq!(record.completion, ReviewCompletion::Cancelled);
    assert_eq!(record.stop, ReviewStop::Cancelled);
    assert_eq!(transport.invocation_count(), 0);
    assert!(journal.reservations.is_empty());
    assert!(before_invoke.source.reads.is_empty());

    let mut before_read = Harness::new(&[true]);
    let row_id = before_read.row_ids[0].clone();
    let transport = ScriptedFixtureTransport::new(
        before_read.approved_rows.clone(),
        vec![ScriptStep {
            expected_rows: Vec::new(),
            observation: acknowledged(request_rows(vec![row_id]), 7, 2),
            cancel_before_return: true,
        }],
    );
    let mut journal = FixtureJournal::default();
    let record = ReviewController::run(
        before_read.request,
        &mut before_read.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("pre-read cancellation");
    assert_eq!(record.completion, ReviewCompletion::Cancelled);
    assert_eq!(record.stop, ReviewStop::Cancelled);
    assert!(before_read.source.reads.is_empty());
    assert_eq!(transport.invocation_count(), 1);
}

#[tokio::test]
async fn output_and_accumulated_token_limits_stop_incomplete() {
    let mut output_case = Harness::new(&[false]);
    output_case.request.recipe.limits.max_output_tokens = 10;
    output_case.request.subject.recipe_sha256 = output_case.request.recipe.digest().unwrap();
    let transport = ScriptedFixtureTransport::new(
        output_case.approved_rows.clone(),
        vec![step(Vec::new(), acknowledged(finish(Vec::new()), 1, 11))],
    );
    let mut journal = FixtureJournal::default();
    let record = ReviewController::run(
        output_case.request,
        &mut output_case.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("output token limit");
    assert_eq!(record.completion, ReviewCompletion::Incomplete);
    assert_eq!(record.stop, ReviewStop::TokenLimit);

    let mut total_case = Harness::new(&[true]);
    total_case.request.recipe.limits.max_total_tokens = 100;
    total_case.request.subject.recipe_sha256 = total_case.request.recipe.digest().unwrap();
    let row_id = total_case.row_ids[0].clone();
    let transport = ScriptedFixtureTransport::new(
        total_case.approved_rows.clone(),
        vec![
            step(
                Vec::new(),
                acknowledged(request_rows(vec![row_id.clone()]), 40, 20),
            ),
            step(vec![row_id], acknowledged(finish(Vec::new()), 30, 20)),
        ],
    );
    let mut journal = FixtureJournal::default();
    let record = ReviewController::run(
        total_case.request,
        &mut total_case.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("total token limit");
    assert_eq!(record.completion, ReviewCompletion::Incomplete);
    assert_eq!(record.stop, ReviewStop::TokenLimit);
    assert_eq!(record.calls.len(), 2);
}

#[tokio::test]
async fn cumulative_request_bytes_are_bounded_before_a_second_reservation() {
    let mut harness = Harness::new(&[true]);
    harness.request.recipe.objective = "x".repeat(4_096);
    harness.request.recipe.limits.max_request_bytes = 8 * 1024;
    harness.request.recipe.limits.max_total_request_bytes = 8 * 1024;
    harness.request.subject.recipe_sha256 = harness.request.recipe.digest().unwrap();
    let row_id = harness.row_ids[0].clone();
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![step(
            Vec::new(),
            acknowledged(request_rows(vec![row_id]), 7, 2),
        )],
    );
    let mut journal = FixtureJournal::default();

    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("cumulative request limit");

    assert_eq!(record.completion, ReviewCompletion::Incomplete);
    assert_eq!(record.stop, ReviewStop::RequestLimit);
    assert_eq!(record.calls.len(), 1);
    assert_eq!(journal.reservations.len(), 1);
    assert_eq!(transport.invocation_count(), 1);
}

#[tokio::test]
async fn exact_source_metadata_is_retained_with_acknowledged_coverage() {
    let mut harness = Harness::new(&[true]);
    let row_id = harness.row_ids[0].clone();
    let selector = harness.request.recipe.source[0].clone();
    let text = harness
        .approved_rows
        .get(&row_id)
        .expect("approved fixture row")
        .clone();
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows.clone(),
        vec![
            step(
                Vec::new(),
                acknowledged(request_rows(vec![row_id.clone()]), 7, 2),
            ),
            step(vec![row_id], acknowledged(finish(Vec::new()), 8, 2)),
        ],
    );
    let mut journal = FixtureJournal::default();
    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .expect("exact source fixture");

    assert_eq!(record.completion, ReviewCompletion::FinishedObservations);
    assert_eq!(record.coverage[0].exposure, RowExposure::Acknowledged);
    assert_eq!(record.coverage[0].read_bytes, text.len() as u64);
    let metadata = record.coverage[0]
        .source_observation
        .as_ref()
        .expect("exact source metadata");
    assert_eq!(metadata.source, selector.source);
    assert_eq!(metadata.raw_bytes, text.len() as u64);
    assert_eq!(metadata.rendered_bytes, text.len() as u64);
    assert_eq!(
        metadata.raw_sha256.as_deref(),
        Some(selector.raw_sha256.as_str())
    );
    assert_eq!(
        metadata.rendered_sha256.as_deref(),
        Some(selector.rendered_sha256.as_str())
    );
    assert_eq!(metadata.rendering, SourceRendering::ExactUtf8);
}

#[tokio::test]
async fn transformed_source_renderings_are_retained_but_never_prepared() {
    for rendering in [
        SourceRendering::Redacted,
        SourceRendering::Truncated,
        SourceRendering::Unsupported,
    ] {
        let harness = Harness::new(&[true]);
        let selector = harness.request.recipe.source[0].clone();
        let raw = harness
            .source
            .text
            .get(&selector.path)
            .expect("fixture source")
            .as_bytes()
            .to_vec();
        let rendered = match rendering {
            SourceRendering::Redacted => "[redacted]\n".to_owned(),
            SourceRendering::Truncated => "fixture\n".to_owned(),
            SourceRendering::Unsupported => String::new(),
            SourceRendering::ExactUtf8 => unreachable!(),
        };
        let observation = source_observation(&selector, raw.clone(), rendered.clone(), rendering);
        let (record, journal, invocations) =
            run_single_source_observation(harness, observation).await;

        assert_eq!(record.stop, ReviewStop::SourceRendering);
        assert_eq!(record.completion, ReviewCompletion::Incomplete);
        assert_eq!(record.coverage[0].exposure, RowExposure::Read);
        let metadata = record.coverage[0]
            .source_observation
            .as_ref()
            .expect("transformed source metadata");
        assert_eq!(metadata.rendering, rendering);
        assert_eq!(metadata.raw_bytes, raw.len() as u64);
        assert_eq!(metadata.rendered_bytes, rendered.len() as u64);
        let raw_digest = digest(&raw);
        let rendered_digest = digest(rendered.as_bytes());
        assert_eq!(metadata.raw_sha256.as_deref(), Some(raw_digest.as_str()));
        assert_eq!(
            metadata.rendered_sha256.as_deref(),
            Some(rendered_digest.as_str())
        );
        assert_eq!(journal.record_attempts.len(), 1);
        assert_eq!(invocations, 1);
    }
}

#[tokio::test]
async fn stale_source_profile_or_permit_is_a_source_mismatch() {
    for stale_profile in [true, false] {
        let harness = Harness::new(&[true]);
        let selector = harness.request.recipe.source[0].clone();
        let text = harness
            .source
            .text
            .get(&selector.path)
            .expect("fixture source")
            .clone();
        let mut observation = source_observation(
            &selector,
            text.as_bytes().to_vec(),
            text,
            SourceRendering::ExactUtf8,
        );
        if stale_profile {
            observation.source.profile = "stale-profile-v1".into();
        } else {
            observation.source.permit_sha256 = "6".repeat(64);
        }
        let returned_source = observation.source.clone();
        let (record, journal, invocations) =
            run_single_source_observation(harness, observation).await;

        assert_eq!(record.stop, ReviewStop::SourceMismatch);
        assert_eq!(record.coverage[0].exposure, RowExposure::Read);
        let metadata = record.coverage[0]
            .source_observation
            .as_ref()
            .expect("stale binding metadata");
        assert_eq!(metadata.source, returned_source);
        assert!(metadata.raw_sha256.is_some());
        assert!(metadata.rendered_sha256.is_some());
        assert_eq!(journal.record_attempts.len(), 1);
        assert_eq!(invocations, 1);
    }
}

#[tokio::test]
async fn raw_and_rendered_digest_mismatches_are_retained_separately() {
    for raw_mismatch in [true, false] {
        let mut harness = Harness::new(&[true]);
        let text = harness
            .source
            .text
            .get(&harness.request.recipe.source[0].path)
            .expect("fixture source")
            .clone();
        if raw_mismatch {
            harness.request.recipe.source[0].raw_sha256 = "6".repeat(64);
        } else {
            harness.request.recipe.source[0].rendered_sha256 = "6".repeat(64);
        }
        harness.request.subject.recipe_sha256 = harness.request.recipe.digest().unwrap();
        let selector = harness.request.recipe.source[0].clone();
        let observation = source_observation(
            &selector,
            text.as_bytes().to_vec(),
            text.clone(),
            SourceRendering::ExactUtf8,
        );
        let (record, journal, invocations) =
            run_single_source_observation(harness, observation).await;

        assert_eq!(record.stop, ReviewStop::SourceMismatch);
        assert_eq!(record.coverage[0].exposure, RowExposure::Read);
        let metadata = record.coverage[0]
            .source_observation
            .as_ref()
            .expect("digest mismatch metadata");
        let actual = digest(text.as_bytes());
        assert_eq!(metadata.raw_sha256.as_deref(), Some(actual.as_str()));
        assert_eq!(metadata.rendered_sha256.as_deref(), Some(actual.as_str()));
        if raw_mismatch {
            assert_ne!(
                metadata.raw_sha256.as_deref(),
                Some(selector.raw_sha256.as_str())
            );
            assert_eq!(
                metadata.rendered_sha256.as_deref(),
                Some(selector.rendered_sha256.as_str())
            );
        } else {
            assert_eq!(
                metadata.raw_sha256.as_deref(),
                Some(selector.raw_sha256.as_str())
            );
            assert_ne!(
                metadata.rendered_sha256.as_deref(),
                Some(selector.rendered_sha256.as_str())
            );
        }
        assert_eq!(journal.record_attempts.len(), 1);
        assert_eq!(invocations, 1);
    }
}

#[tokio::test]
async fn malformed_acknowledged_observations_stop_without_retry() {
    for case in 0..18 {
        let mut harness = Harness::new(&[true]);
        let row_id = harness.row_ids[0].clone();
        let mut malformed = acknowledged(finish(Vec::new()), 8, 2);
        match case {
            0 => malformed.response_capture_complete = false,
            1 => malformed.captured_response_bytes += 1,
            2 => malformed.captured_response_sha256 = "6".repeat(64),
            3 => {
                malformed.captured_response =
                    vec![b'x'; harness.request.recipe.limits.max_response_bytes as usize + 1];
                malformed.captured_response_bytes = malformed.captured_response.len() as u64;
                malformed.captured_response_sha256 = digest(&malformed.captured_response);
            }
            4 => malformed.captured_response_sha256 = "invalid-digest".into(),
            5 => {
                malformed.reply.as_mut().expect("fixture reply").response_id =
                    "invalid response id".into();
            }
            6 => {
                malformed
                    .reply
                    .as_mut()
                    .expect("fixture reply")
                    .content_sha256 = "invalid-digest".into();
            }
            7 => malformed.basis = TransportBasis::LocalHttpUnqualified,
            8 => {
                malformed.reply.as_mut().expect("fixture reply").model = "other-model".into();
            }
            9 => {
                malformed.reply.as_mut().expect("fixture reply").usage.basis =
                    UsageBasis::ServerReportedChatCompletionsV1;
            }
            10 => {
                malformed.state = CallState::Uncertain;
            }
            11 => malformed.failure = Some(ModelFailure::Connection),
            12 => malformed.reply.as_mut().unwrap().content_sha256 = "6".repeat(64),
            13 => {
                let reply = malformed.reply.as_mut().unwrap();
                reply.decision = ReviewDecision::Finish {
                    findings: Vec::new(),
                    unresolved: vec!["Different captured decision".into()],
                };
                reply.content_sha256 = digest(&serde_json::to_vec(&reply.decision).unwrap());
            }
            14 => {
                let usage = &mut malformed.reply.as_mut().unwrap().usage;
                usage.input_tokens += 1;
                usage.total_tokens += 1;
            }
            15 => malformed.reply.as_mut().unwrap().service_tier = Some("other-route".into()),
            16 => {
                malformed = ModelCallObservation {
                    state: CallState::NotStarted,
                    basis: TransportBasis::Fixture,
                    reply: None,
                    failure: Some(ModelFailure::OutputLimit),
                    http_status: None,
                    captured_response_bytes: 0,
                    captured_response_sha256: digest(&[]),
                    captured_response: Vec::new(),
                    response_capture_complete: false,
                };
            }
            17 => {
                // Even matching envelopes cannot supply a false content digest.
                malformed.reply.as_mut().unwrap().content_sha256 = "6".repeat(64);
                malformed.captured_response =
                    serde_json::to_vec(malformed.reply.as_ref().unwrap()).unwrap();
                malformed.captured_response_bytes = malformed.captured_response.len() as u64;
                malformed.captured_response_sha256 = digest(&malformed.captured_response);
            }
            _ => unreachable!(),
        }
        let transport = ScriptedFixtureTransport::new(
            harness.approved_rows.clone(),
            vec![
                step(
                    Vec::new(),
                    acknowledged(request_rows(vec![row_id.clone()]), 7, 2),
                ),
                step(vec![row_id], malformed),
            ],
        );
        let mut journal = FixtureJournal::default();
        let record = ReviewController::run(
            harness.request,
            &mut harness.source,
            &mut journal,
            &transport,
            &ReviewCancellation::default(),
        )
        .await
        .expect("malformed fixture observation");

        assert_eq!(
            record.completion,
            ReviewCompletion::Incomplete,
            "case {case}"
        );
        assert_eq!(record.stop, ReviewStop::Transport, "case {case}");
        assert_eq!(record.coverage[0].exposure, RowExposure::PossiblyDisclosed);
        assert_eq!(record.calls.len(), 2);
        assert_eq!(journal.record_attempts.len(), 2);
        assert_eq!(transport.invocation_count(), 2);
        let normalized = &record.calls[1].observation;
        assert_eq!(normalized.state, CallState::Uncertain);
        assert_eq!(normalized.failure, Some(ModelFailure::InvalidResponse));
        assert!(normalized.reply.is_none());
        assert!(!normalized.response_capture_complete);
        assert!(
            normalized.captured_response.len()
                <= record.request.recipe.limits.max_response_bytes as usize
        );
        assert_eq!(
            normalized.captured_response_bytes,
            normalized.captured_response.len() as u64
        );
        assert_eq!(
            normalized.captured_response_sha256,
            digest(&normalized.captured_response)
        );
        assert_eq!(&journal.record_attempts[1].1, normalized);
    }
}

#[tokio::test]
async fn source_denial_and_mismatched_returned_bytes_remain_distinct() {
    for case in 0..3 {
        let harness = Harness::new(&[true]);
        let row_id = harness.row_ids[0].clone();
        let mut source = ObservationSource(if case == 0 {
            Err(SourceReadFailure::Denied)
        } else if case == 1 {
            Err(SourceReadFailure::Limit)
        } else {
            Ok(ReviewSourceObservation {
                source: harness.request.recipe.source[0].source.clone(),
                raw: b"wrong bytes\n".to_vec(),
                rendered: "wrong bytes\n".into(),
                rendering: SourceRendering::ExactUtf8,
            })
        });
        let transport = ScriptedFixtureTransport::new(
            harness.approved_rows,
            vec![step(
                Vec::new(),
                acknowledged(request_rows(vec![row_id]), 7, 2),
            )],
        );
        let record = ReviewController::run(
            harness.request,
            &mut source,
            &mut FixtureJournal::default(),
            &transport,
            &ReviewCancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(transport.invocation_count(), 1);
        if case < 2 {
            assert_eq!(
                record.stop,
                if case == 0 {
                    ReviewStop::SourceRead
                } else {
                    ReviewStop::SourceLimit
                }
            );
            assert_eq!(
                record.coverage[0].read_failure,
                Some(if case == 0 {
                    SourceReadFailure::Denied
                } else {
                    SourceReadFailure::Limit
                })
            );
            assert_eq!(record.coverage[0].exposure, RowExposure::Unread);
        } else {
            assert_eq!(record.stop, ReviewStop::SourceMismatch);
            assert_eq!(record.coverage[0].exposure, RowExposure::Read);
            assert_eq!(record.coverage[0].read_bytes, 12);
            let metadata = record.coverage[0]
                .source_observation
                .as_ref()
                .expect("mismatched source metadata retained");
            let wrong_digest = digest(b"wrong bytes\n");
            assert_eq!(metadata.raw_sha256.as_deref(), Some(wrong_digest.as_str()));
            assert_eq!(metadata.rendered_sha256, metadata.raw_sha256);
        }
        assert!(record.findings.is_empty());
    }
}

#[tokio::test]
async fn deadline_after_reservation_is_recorded_without_invoking_transport() {
    struct SlowJournal(FixtureJournal);
    impl ReviewJournal for SlowJournal {
        fn reserve_call(&mut self, reservation: &CallReservation) -> Result<ReservationOutcome> {
            let ticket = self.0.reserve_call(reservation)?;
            std::thread::sleep(std::time::Duration::from_millis(20));
            Ok(ticket)
        }
        fn record_call(
            &mut self,
            ticket: &CallTicket,
            observation: &ModelCallObservation,
        ) -> Result<()> {
            self.0.record_call(ticket, observation)
        }
    }
    let mut harness = Harness::new(&[true]);
    harness.request.recipe.limits.timeout_ms = 5;
    harness.request.subject.recipe_sha256 = harness.request.recipe.digest().unwrap();
    let transport = ScriptedFixtureTransport::new(harness.approved_rows, Vec::new());
    let mut journal = SlowJournal(FixtureJournal::default());
    let record = ReviewController::run(
        harness.request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(transport.invocation_count(), 0);
    assert_eq!(record.calls.len(), 1);
    assert!(record.calls[0].journal_recorded);
    assert_eq!(record.calls[0].observation.state, CallState::NotStarted);
    assert_eq!(
        record.calls[0].observation.failure,
        Some(ModelFailure::Deadline)
    );
    assert_eq!(record.completion, ReviewCompletion::Incomplete);
    assert_eq!(record.stop, ReviewStop::Deadline);
}

#[tokio::test]
async fn restarting_controller_does_not_redispatch_an_existing_reservation() {
    let mut harness = Harness::new(&[false]);
    let request = harness.request.clone();
    let transport = ScriptedFixtureTransport::new(
        harness.approved_rows,
        vec![step(Vec::new(), acknowledged(finish(Vec::new()), 7, 2))],
    );
    let mut journal = FixtureJournal::default();
    let first = ReviewController::run(
        request.clone(),
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(first.completion, ReviewCompletion::FinishedObservations);
    let second = ReviewController::run(
        request,
        &mut harness.source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(second.stop, ReviewStop::ExistingReservation);
    assert_eq!(second.completion, ReviewCompletion::Incomplete);
    assert!(second.unconfirmed_reservation.is_some());
    assert_eq!(transport.invocation_count(), 1);
    assert_eq!(journal.reservations.len(), 1);
}

/// Shares only fixture reservation state to exercise overlapping controllers.
#[derive(Clone, Default)]
struct SharedFixtureJournal(Arc<Mutex<FixtureJournal>>);

impl ReviewJournal for SharedFixtureJournal {
    fn reserve_call(&mut self, reservation: &CallReservation) -> Result<ReservationOutcome> {
        self.0
            .lock()
            .expect("shared fixture lock")
            .reserve_call(reservation)
    }

    fn record_call(
        &mut self,
        ticket: &CallTicket,
        observation: &ModelCallObservation,
    ) -> Result<()> {
        self.0
            .lock()
            .expect("shared fixture lock")
            .record_call(ticket, observation)
    }
}

struct YieldingFixtureTransport(ScriptedFixtureTransport);

impl ModelTransport for YieldingFixtureTransport {
    fn scope_sha256(&self) -> String {
        self.0.scope_sha256()
    }

    fn basis(&self) -> TransportBasis {
        self.0.basis()
    }

    fn invoke<'a>(
        &'a self,
        call: &'a PreparedModelCall,
        cancel: &'a ReviewCancellation,
    ) -> Pin<Box<dyn Future<Output = ModelCallObservation> + Send + 'a>> {
        Box::pin(async move {
            // Leave the first reservation pending while the other runner starts.
            tokio::task::yield_now().await;
            self.0.invoke(call, cancel).await
        })
    }
}

#[tokio::test]
async fn overlapping_controllers_do_not_dispatch_the_same_pending_reservation() {
    let mut first = Harness::new(&[false]);
    let mut second = Harness::new(&[false]);
    let transport = YieldingFixtureTransport(ScriptedFixtureTransport::new(
        first.approved_rows,
        vec![step(Vec::new(), acknowledged(finish(Vec::new()), 7, 2))],
    ));
    let mut first_journal = SharedFixtureJournal::default();
    let mut second_journal = first_journal.clone();
    let cancellation = ReviewCancellation::default();
    let (first_record, second_record) = tokio::join!(
        ReviewController::run(
            first.request,
            &mut first.source,
            &mut first_journal,
            &transport,
            &cancellation
        ),
        ReviewController::run(
            second.request,
            &mut second.source,
            &mut second_journal,
            &transport,
            &cancellation
        )
    );
    let first_record = first_record.expect("first synthetic runner");
    let second_record = second_record.expect("second synthetic runner");
    assert_eq!(
        first_record.completion,
        ReviewCompletion::FinishedObservations
    );
    assert_eq!(second_record.stop, ReviewStop::ExistingReservation);
    assert_eq!(second_record.completion, ReviewCompletion::Incomplete);
    assert!(second_record.unconfirmed_reservation.is_some());
    assert_eq!(transport.0.invocation_count(), 1);
    let journal = first_journal.0.lock().expect("shared fixture lock");
    assert_eq!(journal.reservations.len(), 1);
    assert_eq!(journal.record_attempts.len(), 1);
}

struct PendingFixtureTransport {
    first_reply: Option<ModelCallObservation>,
    pending_rows: Vec<String>,
    validator: ScriptedFixtureTransport,
    invocations: AtomicUsize,
    pending_entered: Arc<AtomicBool>,
}

impl ModelTransport for PendingFixtureTransport {
    fn scope_sha256(&self) -> String {
        PROVIDER_SCOPE.into()
    }
    fn basis(&self) -> TransportBasis {
        TransportBasis::Fixture
    }
    fn invoke<'a>(
        &'a self,
        call: &'a PreparedModelCall,
        _: &'a ReviewCancellation,
    ) -> Pin<Box<dyn Future<Output = ModelCallObservation> + Send + 'a>> {
        let ordinal = self.invocations.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if ordinal == 0
                && let Some(reply) = &self.first_reply
            {
                self.validator.validate_call(call, &[]);
                return reply.clone();
            }
            self.validator.validate_call(call, &self.pending_rows);
            self.pending_entered.store(true, Ordering::SeqCst);
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn controller_deadline_bounds_a_permanently_pending_adapter() {
    let mut harness = Harness::new(&[false]);
    harness.request.recipe.limits.timeout_ms = 30;
    harness.request.subject.recipe_sha256 = harness.request.recipe.digest().unwrap();
    let transport = PendingFixtureTransport {
        first_reply: None,
        pending_rows: Vec::new(),
        validator: ScriptedFixtureTransport::new(harness.approved_rows, Vec::new()),
        invocations: AtomicUsize::new(0),
        pending_entered: Arc::new(AtomicBool::new(false)),
    };
    let mut journal = FixtureJournal::default();
    let cancel = ReviewCancellation::default();
    let record = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        ReviewController::run(
            harness.request,
            &mut harness.source,
            &mut journal,
            &transport,
            &cancel,
        ),
    )
    .await
    .expect("controller must not hang on a pending adapter")
    .unwrap();
    assert!(transport.pending_entered.load(Ordering::SeqCst));
    assert_eq!(transport.invocations.load(Ordering::SeqCst), 1);
    assert_eq!(record.completion, ReviewCompletion::Incomplete);
    assert_eq!(record.stop, ReviewStop::Deadline);
    assert_eq!(journal.reservations.len(), 1);
    assert_eq!(journal.record_attempts.len(), 1);
    let observed = &record.calls[0].observation;
    assert_eq!(observed.state, CallState::Uncertain);
    assert_eq!(observed.failure, Some(ModelFailure::Deadline));
    assert!(observed.reply.is_none());
    assert!(observed.captured_response.is_empty());
    assert_eq!(observed.captured_response_bytes, 0);
    assert_eq!(observed.captured_response_sha256, digest(&[]));
    assert!(!observed.response_capture_complete);
    assert_eq!(&journal.record_attempts[0].1, observed);
}

#[tokio::test]
async fn cancellation_bounds_a_pending_adapter_after_source_disclosure() {
    let mut harness = Harness::new(&[true]);
    let row_id = harness.row_ids[0].clone();
    let entered = Arc::new(AtomicBool::new(false));
    let transport = PendingFixtureTransport {
        first_reply: Some(acknowledged(request_rows(vec![row_id.clone()]), 7, 2)),
        pending_rows: vec![row_id],
        validator: ScriptedFixtureTransport::new(harness.approved_rows, Vec::new()),
        invocations: AtomicUsize::new(0),
        pending_entered: entered.clone(),
    };
    let cancel = ReviewCancellation::default();
    let trigger = cancel.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !entered.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("adapter must reach pending disclosed call");
        trigger.cancel();
    });
    let mut journal = FixtureJournal::default();
    let record = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        ReviewController::run(
            harness.request,
            &mut harness.source,
            &mut journal,
            &transport,
            &cancel,
        ),
    )
    .await
    .expect("controller must cancel pending adapter")
    .unwrap();
    canceller.await.unwrap();
    assert_eq!(record.completion, ReviewCompletion::Cancelled);
    assert_eq!(record.stop, ReviewStop::Cancelled);
    assert_eq!(record.coverage[0].exposure, RowExposure::PossiblyDisclosed);
    assert_eq!(harness.source.reads.len(), 1);
    assert_eq!(transport.invocations.load(Ordering::SeqCst), 2);
    assert_eq!(journal.reservations.len(), 2);
    assert_eq!(journal.record_attempts.len(), 2);
    let observed = &record.calls[1].observation;
    assert_eq!(observed.state, CallState::Uncertain);
    assert_eq!(observed.failure, Some(ModelFailure::Cancelled));
    assert!(observed.reply.is_none());
    assert!(observed.captured_response.is_empty());
    assert_eq!(observed.captured_response_bytes, 0);
    assert_eq!(observed.captured_response_sha256, digest(&[]));
    assert!(!observed.response_capture_complete);
    assert_eq!(&journal.record_attempts[1].1, observed);
}
