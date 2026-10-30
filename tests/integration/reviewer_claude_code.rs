#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscm::checks::CheckSubject;
use agentscm::context::ChangeId;
use agentscm::reviewer::{
    CallReservation, CallTicket, ClaudeCodeTransport, ModelCallObservation, ModelFailure,
    ModelTransport, ReservationOutcome, ReviewCancellation, ReviewCompletion, ReviewController,
    ReviewJournal, ReviewLimits, ReviewRecipe, ReviewRequest, ReviewSource, ReviewSourceBinding,
    ReviewSourceObservation, ReviewSourceSelector, ReviewStop, SourceReadFailure, SourceRendering,
    TransportBasis, observed_models,
};
use agentscm::{Error, Result};
use serde_json::json;
use sha2::{Digest, Sha256};

const ROW_TEXT: &str = "bounded fixture source\n";

#[derive(Default)]
struct Journal {
    reservations: Vec<CallReservation>,
    observations: Vec<ModelCallObservation>,
}

impl ReviewJournal for Journal {
    fn reserve_call(&mut self, reservation: &CallReservation) -> Result<ReservationOutcome> {
        self.reservations.push(reservation.clone());
        Ok(ReservationOutcome::Reserved(CallTicket {
            reservation_sha256: reservation.digest()?,
            run_generation: reservation.run_generation,
            durable_version: self.reservations.len() as u64,
        }))
    }

    fn record_call(&mut self, _: &CallTicket, observation: &ModelCallObservation) -> Result<()> {
        self.observations.push(observation.clone());
        Ok(())
    }
}

struct Source(HashMap<String, String>);

impl ReviewSource for Source {
    fn read(
        &mut self,
        _: &ReviewRequest,
        selector: &ReviewSourceSelector,
        max_bytes: usize,
    ) -> std::result::Result<ReviewSourceObservation, SourceReadFailure> {
        let text = self
            .0
            .get(&selector.path)
            .ok_or(SourceReadFailure::Missing)?;
        if text.len() > max_bytes {
            return Err(SourceReadFailure::Limit);
        }
        Ok(ReviewSourceObservation {
            source: selector.source.clone(),
            raw: text.as_bytes().to_vec(),
            rendered: text.clone(),
            rendering: SourceRendering::ExactUtf8,
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    executable: PathBuf,
    work_dir: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new(mode: &str, auth_method: &str, row_id: &str) -> Self {
        Self::new_with_model(mode, auth_method, row_id, "claude-sonnet-fixture")
    }

    fn new_with_model(mode: &str, auth_method: &str, row_id: &str, observed_model: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("claude-fixture");
        let work_dir = root.path().join("empty-controller-cwd");
        let log = root.path().join("argv.log");
        fs::create_dir(&work_dir).unwrap();
        fs::set_permissions(&work_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let request_rows = result(
            json!({"action":"request_rows","row_ids":[row_id],"findings":[],"unresolved":[]}),
            observed_model,
        );
        let finish = result(
            json!({"action":"finish","row_ids":[],"findings":[],"unresolved":[]}),
            observed_model,
        );
        let script = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf '%s\n' '2.1.300 (Claude Code)'
  exit 0
fi
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '__AUTH__'
  exit 0
fi
printf '<%s>\n' "$@" >> '__LOG__'
printf '<MAX_OUTPUT=%s>\n' "$CLAUDE_CODE_MAX_OUTPUT_TOKENS" >> '__LOG__'
input=$(cat)
case '__MODE__' in
  success)
    if printf '%s' "$input" | grep -q '"supplied_rows":\[\]'; then
      printf '%s\n' '__REQUEST_ROWS__'
    else
      printf '%s\n' '__FINISH__'
    fi
    ;;
  invalid)
    printf '%s\n' '{"unexpected":true}'
    ;;
  sleep)
    sleep 30
    printf '%s\n' '__FINISH__'
    ;;
esac
"#
        .replace(
            "__AUTH__",
            &json!({"loggedIn":true,"authMethod":auth_method,"extra":"allowed"}).to_string(),
        )
        .replace("__LOG__", log.to_str().unwrap())
        .replace("__MODE__", mode)
        .replace("__REQUEST_ROWS__", &request_rows)
        .replace("__FINISH__", &finish);
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _root: root,
            executable,
            work_dir,
            log,
        }
    }
}

fn result(decision: serde_json::Value, observed_model: &str) -> String {
    let mut model_usage = serde_json::Map::new();
    model_usage.insert(
        observed_model.into(),
        json!({
            "inputTokens":10,
            "outputTokens":5,
            "cacheCreationInputTokens":2,
            "cacheReadInputTokens":3,
            "costUSD":0.0
        }),
    );
    json!({
        "type":"result",
        "subtype":"success",
        "is_error":false,
        "session_id":"claude-fixture-session",
        "num_turns":2,
        "structured_output":decision,
        "usage":{
            "input_tokens":10,
            "output_tokens":5,
            "cache_creation_input_tokens":2,
            "cache_read_input_tokens":3
        },
        "modelUsage":model_usage,
        "additive_metadata":{"accepted":true}
    })
    .to_string()
}

fn request(transport: &ClaudeCodeTransport, timeout_ms: u32) -> (ReviewRequest, Source, String) {
    let selector = ReviewSourceSelector {
        source: ReviewSourceBinding {
            profile: "native-revision-fixture-v1".into(),
            permit_sha256: "7".repeat(64),
        },
        path: "src/reviewed.rs".into(),
        start_line: 1,
        end_line: 1,
        raw_sha256: digest(ROW_TEXT.as_bytes()),
        rendered_sha256: digest(ROW_TEXT.as_bytes()),
        required: true,
    };
    let row_id = selector.row_id().unwrap();
    let recipe = ReviewRecipe {
        version: 1,
        model: "sonnet".into(),
        objective: "Apply the pinned review skill to the supplied source rows.".into(),
        provider_scope_sha256: transport.scope_sha256(),
        installed_policy_sha256: "8".repeat(64),
        policy_epoch: 2,
        source: vec![selector],
        limits: ReviewLimits {
            max_source_bytes: 16 * 1024,
            max_request_bytes: 64 * 1024,
            max_total_request_bytes: 128 * 1024,
            max_response_bytes: 32 * 1024,
            max_calls: 2,
            max_input_tokens: 1_000,
            max_output_tokens: 128,
            max_total_tokens: 2_000,
            max_cost_micros: None,
            timeout_ms,
        },
    };
    let mut subject = CheckSubject {
        repository: "repository-1".into(),
        change: ChangeId {
            repository: "repository-1".into(),
            id: "change-1".into(),
        },
        change_version: 1,
        context_digest: "1".repeat(64),
        revision: "2".repeat(64),
        recipe_sha256: String::new(),
        toolchain_sha256: "3".repeat(64),
        environment_sha256: "4".repeat(64),
    };
    subject.recipe_sha256 = recipe.digest().unwrap();
    (
        ReviewRequest {
            attempt_id: "claude-fixture-attempt".into(),
            run_generation: 1,
            subject,
            recipe,
        },
        Source(HashMap::from([("src/reviewed.rs".into(), ROW_TEXT.into())])),
        row_id,
    )
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[tokio::test]
async fn subscription_transport_runs_the_finite_controller_without_tools() {
    let seed_selector = ReviewSourceSelector {
        source: ReviewSourceBinding {
            profile: "native-revision-fixture-v1".into(),
            permit_sha256: "7".repeat(64),
        },
        path: "src/reviewed.rs".into(),
        start_line: 1,
        end_line: 1,
        raw_sha256: digest(ROW_TEXT.as_bytes()),
        rendered_sha256: digest(ROW_TEXT.as_bytes()),
        required: true,
    };
    let fixture = Fixture::new("success", "claude.ai", &seed_selector.row_id().unwrap());
    let transport = ClaudeCodeTransport::new(&fixture.executable, &fixture.work_dir).unwrap();
    let (request, mut source, _) = request(&transport, 2_000);
    let mut journal = Journal::default();
    let record = ReviewController::run(
        request,
        &mut source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .unwrap();

    assert_eq!(record.completion, ReviewCompletion::FinishedObservations);
    assert_eq!(record.stop, ReviewStop::ModelFinished);
    assert_eq!(record.calls.len(), 2);
    assert!(record.findings.is_empty());
    assert_eq!(
        observed_models(&record.calls[1].observation).unwrap(),
        vec!["claude-sonnet-fixture"]
    );
    let retained_request: serde_json::Value =
        serde_json::from_slice(&journal.reservations[0].request_body).unwrap();
    let retained_system = retained_request["messages"][0]["content"].as_str().unwrap();
    assert!(retained_system.contains(
        "Claude Code structured output requires all four fields: action, row_ids, findings, unresolved."
    ));
    let args = fs::read_to_string(&fixture.log).unwrap();
    for required in [
        "<--restricted>",
        "<--safe-mode>",
        "<--strict-mcp-config>",
        "<--no-session-persistence>",
        "<--json-schema>",
    ] {
        assert!(
            args.contains(required),
            "missing argument profile: {required}"
        );
    }
    assert!(args.contains("<--tools>\n<>\n<--disallowedTools>\n<mcp__*>"));
    assert!(args.contains("<--permission-prompts>\n<none>"));
    assert!(args.contains("<--max-turns>\n<1>"));
    assert!(args.contains("<MAX_OUTPUT=128>"));
    assert!(args.contains(r#""type":"object""#));
    assert!(args.contains(r#""action":{"enum":["request_rows","finish"]}"#));
    assert!(args.contains(&format!("<--system-prompt>\n<{retained_system}>\n")));
    assert!(!args.contains("oneOf"));
    assert!(!args.contains("anyOf"));
    assert!(!args.contains("allOf"));
    assert!(!args.contains(ROW_TEXT));
    assert!(fs::read_dir(&fixture.work_dir).unwrap().next().is_none());
}

#[test]
fn rejects_api_key_authentication_before_a_review_can_start() {
    let fixture = Fixture::new("success", "api_key", &"a".repeat(64));
    let error = ClaudeCodeTransport::new(&fixture.executable, &fixture.work_dir)
        .err()
        .expect("API-key auth must be rejected");
    assert!(matches!(error, Error::Invalid(message) if message.contains("subscription")));
    assert!(!fixture.log.exists());
}

#[tokio::test]
async fn malformed_cli_output_is_an_incomplete_transport_observation() {
    let fixture = Fixture::new("invalid", "oauth_token", &"a".repeat(64));
    let transport = ClaudeCodeTransport::new(&fixture.executable, &fixture.work_dir).unwrap();
    let (request, mut source, _) = request(&transport, 1_000);
    let mut journal = Journal::default();
    let record = ReviewController::run(
        request,
        &mut source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(record.completion, ReviewCompletion::Incomplete);
    assert_eq!(record.stop, ReviewStop::Transport);
    assert_eq!(record.calls.len(), 1);
    assert_eq!(
        record.calls[0].observation.basis,
        TransportBasis::ClaudeCode
    );
}

#[tokio::test]
async fn requested_model_must_match_the_single_observed_claude_identity() {
    for (requested, observed) in [
        ("sonnet", "claude-opus-fixture"),
        ("claude-sonnet-exact", "claude-sonnet-other"),
        ("custom", "claude-sonnet-fixture"),
    ] {
        let fixture = Fixture::new_with_model("success", "claude.ai", &"a".repeat(64), observed);
        let transport = ClaudeCodeTransport::new(&fixture.executable, &fixture.work_dir).unwrap();
        let (mut request, mut source, _) = request(&transport, 1_000);
        request.recipe.model = requested.into();
        request.subject.recipe_sha256 = request.recipe.digest().unwrap();
        let mut journal = Journal::default();
        let record = ReviewController::run(
            request,
            &mut source,
            &mut journal,
            &transport,
            &ReviewCancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(record.completion, ReviewCompletion::Incomplete);
        assert_eq!(record.stop, ReviewStop::Transport);
        assert_eq!(record.calls.len(), 1);
        assert_eq!(
            record.calls[0].observation.failure,
            Some(ModelFailure::Identity),
            "requested={requested}, observed={observed}"
        );
    }
}

#[tokio::test]
async fn controller_deadline_bounds_a_sleeping_cli() {
    let fixture = Fixture::new("sleep", "claude.ai", &"a".repeat(64));
    let transport = ClaudeCodeTransport::new(&fixture.executable, &fixture.work_dir).unwrap();
    let (request, mut source, _) = request(&transport, 50);
    let mut journal = Journal::default();
    let started = std::time::Instant::now();
    let record = ReviewController::run(
        request,
        &mut source,
        &mut journal,
        &transport,
        &ReviewCancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(record.stop, ReviewStop::Deadline);
    // The fixture sleeps 30 s; a third of that separates the 50 ms deadline from no deadline
    // even when a loaded host is slow to start the CLI.
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn cancellation_stops_a_sleeping_cli_review() {
    let fixture = Fixture::new("sleep", "claude.ai", &"a".repeat(64));
    let transport = ClaudeCodeTransport::new(&fixture.executable, &fixture.work_dir).unwrap();
    let (request, mut source, _) = request(&transport, 2_000);
    let journal = Arc::new(Mutex::new(Journal::default()));
    struct SharedJournal(Arc<Mutex<Journal>>);
    impl ReviewJournal for SharedJournal {
        fn reserve_call(&mut self, reservation: &CallReservation) -> Result<ReservationOutcome> {
            self.0.lock().unwrap().reserve_call(reservation)
        }
        fn record_call(
            &mut self,
            ticket: &CallTicket,
            observation: &ModelCallObservation,
        ) -> Result<()> {
            self.0.lock().unwrap().record_call(ticket, observation)
        }
    }
    let mut shared = SharedJournal(Arc::clone(&journal));
    let cancellation = ReviewCancellation::default();
    let trigger = cancellation.clone();
    let review =
        ReviewController::run(request, &mut source, &mut shared, &transport, &cancellation);
    let cancel = async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        trigger.cancel();
    };
    let (record, ()) = tokio::join!(review, cancel);
    let record = record.unwrap();
    assert_eq!(record.completion, ReviewCompletion::Cancelled);
    assert_eq!(record.stop, ReviewStop::Cancelled);
    assert_eq!(journal.lock().unwrap().observations.len(), 1);
}

#[test]
fn work_directory_must_be_private_empty_and_outside_git() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("claude-fixture");
    fs::write(
        &executable,
        "#!/bin/sh\nprintf '%s\\n' '2.1.300 (Claude Code)'\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let work = root.path().join("work");
    fs::create_dir(&work).unwrap();
    fs::set_permissions(&work, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ClaudeCodeTransport::new(&executable, &work).is_err());
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(work.join("candidate.rs"), "untrusted").unwrap();
    assert!(ClaudeCodeTransport::new(&executable, &work).is_err());
}
