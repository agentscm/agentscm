#![cfg(unix)]

#[path = "common/fake_docker.rs"]
mod fake_docker;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use agentscm::Error;
use agentscm::authority::Authority;
use agentscm::context::{BaseState, ChangeId, InputIdentity, RequiredEvidence, ReviewContext};
use agentscm::execution::{
    COMMAND_CAPABILITY, COMMAND_EVIDENCE, Cancellation, CleanupCustody, CleanupHandle,
    CommandAttempt, CommandCandidate, CommandCheckRequest, CommandRecipe, CommandState,
    ExecutionLimits, ExecutionOutcome, ExecutionRequest, OciBackend, SyscallPolicy,
    environment_digest,
};
use agentscm::model::{Approval, FileChange, IntegrateRequest};
use agentscm::store::Engine;
use fake_docker::FAKE_DOCKER;
use rusqlite::Connection;
use tempfile::TempDir;

const IMAGE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const TOOLCHAIN: &str = "2222222222222222222222222222222222222222222222222222222222222222";

struct Fixture {
    temporary: TempDir,
    _socket: Option<UnixListener>,
    engine: Engine,
    authority: Authority,
    backend: OciBackend,
    config: PathBuf,
    work: PathBuf,
    context: ReviewContext,
    recipe: CommandRecipe,
}

impl Fixture {
    fn fake() -> Self {
        Self::new(recipe(vec!["/bin/true".into()], BTreeMap::new()), None)
    }

    fn new(recipe: CommandRecipe, runtime: Option<(PathBuf, PathBuf)>) -> Self {
        let temporary = TempDir::new().expect("temporary fixture");
        private(temporary.path());
        let config = temporary.path().join("config");
        fs::create_dir(&config).expect("config directory");
        private(&config);
        let work = temporary.path().join("work");
        fs::create_dir(&work).expect("work directory");
        private(&work);
        let (socket, backend) = match runtime {
            Some((docker, socket)) => (
                None,
                OciBackend::new(&docker, &socket, &config).expect("real backend"),
            ),
            None => {
                let executable = temporary.path().join("fake-docker.py");
                fs::write(&executable, FAKE_DOCKER).expect("write fake Docker");
                fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
                    .expect("make fake Docker executable");
                let socket_path = temporary.path().join("docker.sock");
                let listener = UnixListener::bind(&socket_path).expect("private Unix socket");
                let backend = OciBackend::new(&executable, &socket_path, &config).expect("backend");
                (Some(listener), backend)
            }
        };
        let engine = Engine::open(temporary.path().join("source")).expect("engine");
        let revision = engine
            .checkpoint(
                None,
                &[FileChange {
                    path: "README.md".into(),
                    content: Some(b"trusted bytes\n".to_vec()),
                }],
            )
            .expect("revision");
        let mut authority = Authority::open(temporary.path().join("authority")).expect("authority");
        let repository = authority.repository_id().expect("repository");
        let context = make_context(&repository, 1, &revision, &recipe, true);
        authority.start_change(&context).expect("start change");
        Self {
            temporary,
            _socket: socket,
            engine,
            authority,
            backend,
            config,
            work,
            context,
            recipe,
        }
    }

    fn scenario(&self, value: &str) {
        fs::write(self.config.join("scenario"), value).expect("write scenario");
    }

    fn request(&self, attempt_id: &str) -> CommandCheckRequest {
        request(&self.context, self.recipe.clone(), attempt_id)
    }

    fn create(&mut self, attempt_id: &str) -> CommandAttempt {
        let request = self.request(attempt_id);
        self.authority
            .create_command_attempt(&request)
            .expect("create attempt")
    }

    fn run(&mut self, attempt_id: &str, generation: u64) -> Result<CommandAttempt, Error> {
        self.authority.run_command_attempt(
            &self.engine,
            &self.backend,
            &self.work,
            attempt_id,
            generation,
            &Cancellation::default(),
        )
    }

    fn cancel(&mut self, attempt_id: &str, generation: u64) -> Result<CommandAttempt, Error> {
        self.authority.cancel_command_attempt(
            Some(&self.backend),
            &self.work,
            attempt_id,
            generation,
            "operator cancel",
        )
    }

    fn recover(&mut self, attempt_id: &str, generation: u64) -> Result<CommandAttempt, Error> {
        self.authority.recover_command_attempt(
            Some(&self.backend),
            &self.work,
            attempt_id,
            generation,
            false,
            "operator recover",
        )
    }

    /// A second backend that talks to the same fake daemon state as the first.
    fn second_backend(&self) -> OciBackend {
        let config = self.temporary.path().join("config2");
        fs::create_dir(&config).expect("second config directory");
        private(&config);
        let backend = OciBackend::new(
            &self.temporary.path().join("fake-docker.py"),
            &self.temporary.path().join("docker.sock"),
            &config,
        )
        .expect("second backend");
        fs::write(
            config.join("state_dir"),
            self.config.to_str().expect("path"),
        )
        .expect("share daemon state");
        backend
    }

    fn evidence_rows(&self) -> i64 {
        self.raw()
            .query_row("SELECT count(*) FROM command_evidence", [], |row| {
                row.get(0)
            })
            .expect("evidence count")
    }

    /// Claim `attempt_id` at generation 1 with custody of a container whose ID is unknown.
    fn claim_with_unknown_container(&mut self, attempt_id: &str) {
        let attempt = self.create(attempt_id);
        let request = ExecutionRequest {
            attempt_id: attempt_id.to_owned(),
            subject: attempt.subject,
            run_generation: 1,
            recipe: self.recipe.clone(),
        };
        let nonce = "c".repeat(64);
        let custody = CleanupCustody {
            run_generation: 1,
            handle: CleanupHandle {
                container_name: format!("agentscm-{nonce}"),
                owner_label: format!("{}:{nonce}", request.digest().expect("request digest")),
                creation_confirmed: false,
                runtime_identity: self.backend.probe().identity.expect("runtime identity"),
                container_id: None,
            },
        };
        self.raw()
            .execute(
                "UPDATE command_attempts SET state = 'running', run_generation = 1, custody_json = ?1 WHERE attempt_id = ?2",
                rusqlite::params![serde_json::to_vec(&custody).expect("custody"), attempt_id],
            )
            .expect("claim with custody");
    }

    fn create_and_run(&mut self, attempt_id: &str) -> CommandAttempt {
        self.create(attempt_id);
        self.run(attempt_id, 0).expect("run attempt")
    }

    fn raw(&self) -> Connection {
        Connection::open(self.temporary.path().join("authority/state.sqlite")).expect("raw")
    }

    fn approve(&mut self, id: &str) -> Result<(), Error> {
        let approval = approval(&self.context, id);
        self.authority.approve(&approval)
    }

    fn refused(&mut self, id: &str) {
        assert!(matches!(
            self.approve(id),
            Err(Error::Invalid(message))
                if message.contains("no exact successful controller-collected attempt")
        ));
    }
}

#[test]
fn successful_run_is_bound_to_context_and_admits_approval_and_integration() {
    let mut fixture = Fixture::fake();
    fixture.refused("before-run");
    let pending = fixture.create("attempt-1");
    assert_eq!(pending.state, CommandState::Pending);
    assert_eq!(pending.run_generation, 0);
    fixture.refused("pending");

    let finished = fixture.run("attempt-1", 0).expect("run");
    assert_eq!(finished.state, CommandState::Finished);
    assert_eq!(finished.outcome, Some(ExecutionOutcome::Success));
    assert_eq!(finished.run_generation, 1);
    assert_eq!(
        fixture.run("attempt-1", 0).expect("terminal retry"),
        finished
    );
    let digest = finished.record_digest.as_deref().expect("record digest");
    let record = fixture.authority.command_record(digest).expect("record");
    assert_eq!(record.outcome, ExecutionOutcome::Success);
    assert_eq!(record.request.attempt_id, "attempt-1");
    assert_eq!(record.request.run_generation, 1);
    assert_eq!(record.request.subject, finished.subject);
    assert_eq!(record.request.subject.revision, fixture.context.revision);
    assert_eq!(
        record.request.subject.context_digest,
        fixture.context.digest().expect("context digest")
    );
    assert_eq!(record.stdout, b"fake candidate output\n");

    fixture.approve("approval").expect("approve");
    let context = &fixture.context;
    let event = fixture
        .authority
        .integrate(
            &fixture.engine,
            &IntegrateRequest {
                operation_id: "integrate".into(),
                target: context.target.clone(),
                expected_target: None,
                revision: context.revision.clone(),
                context_digest: context.digest().expect("context digest"),
                approval_id: "approval".into(),
                change: Some(context.change.clone()),
                change_version: Some(context.change_version),
                policy_epoch: context.policy_epoch,
            },
        )
        .expect("integrate");
    assert_eq!(event.after.revision, context.revision);
}

#[test]
fn nonzero_exit_is_recorded_but_never_admits() {
    let mut fixture = Fixture::fake();
    fixture.scenario("nonzero");
    let finished = fixture.create_and_run("attempt-1");
    assert_eq!(finished.state, CommandState::Finished);
    assert_eq!(finished.outcome, Some(ExecutionOutcome::Failed));
    let record = fixture
        .authority
        .command_record(finished.record_digest.as_deref().expect("digest"))
        .expect("record");
    assert_eq!(record.exit_code, Some(7));
    fixture.refused("failed");
}

#[test]
fn unavailable_snapshot_parent_records_incomplete_and_never_admits() {
    let mut fixture = Fixture::fake();
    fs::set_permissions(&fixture.work, fs::Permissions::from_mode(0o755)).expect("open parent");
    let finished = fixture.create_and_run("attempt-1");
    assert_eq!(finished.outcome, Some(ExecutionOutcome::Incomplete));
    let record = fixture
        .authority
        .command_record(finished.record_digest.as_deref().expect("digest"))
        .expect("record");
    assert!(!record.isolation_observed);
    assert!(record.detail.contains("owner-private"), "{}", record.detail);
    fixture.refused("incomplete");
}

#[test]
fn stale_context_cannot_run_and_old_success_does_not_admit_the_new_context() {
    let mut fixture = Fixture::fake();
    fixture.create_and_run("old");
    fixture.create("stale");
    let mut second = make_context(
        &fixture.context.change.repository,
        2,
        &fixture.context.revision,
        &fixture.recipe,
        true,
    );
    second.acceptance_criteria = vec!["same source under changed criteria".into()];
    fixture
        .authority
        .update_change_context(1, &second)
        .expect("update context");

    assert!(matches!(fixture.run("stale", 0), Err(Error::Conflict(_))));
    assert_eq!(
        fixture
            .authority
            .command_attempt("stale")
            .expect("attempt")
            .state,
        CommandState::Pending
    );
    assert!(matches!(
        fixture
            .authority
            .create_command_attempt(&fixture.request("late")),
        Err(Error::Conflict(_))
    ));
    fixture.context = second;
    fixture.refused("new-context");
}

#[test]
fn registration_is_idempotent_and_conflicting_input_is_refused() {
    let mut fixture = Fixture::fake();
    let first = fixture.create("attempt-1");
    assert_eq!(fixture.create("attempt-1"), first);

    let mut different = fixture.request("attempt-1");
    different.recipe.argv.push("--different".into());
    assert!(matches!(
        fixture.authority.create_command_attempt(&different),
        Err(Error::Conflict(_))
    ));
    let mut moved = fixture.request("attempt-1");
    moved.revision = "a".repeat(64);
    assert!(matches!(
        fixture.authority.create_command_attempt(&moved),
        Err(Error::Conflict(_))
    ));
    // A recipe the context does not name is refused for a new attempt too.
    let mut unnamed = fixture.request("attempt-2");
    unnamed.recipe.argv.push("--unnamed".into());
    assert!(matches!(
        fixture.authority.create_command_attempt(&unnamed),
        Err(Error::Conflict(_))
    ));
    // The collector accepts only simple attempt IDs, so such an attempt could never run.
    assert!(
        fixture
            .authority
            .create_command_attempt(&fixture.request("attempt/3"))
            .is_err()
    );
}

#[test]
fn context_that_does_not_request_the_capability_refuses_attempts_and_needs_no_record() {
    let mut fixture = Fixture::fake();
    let mut plain = make_context(
        &fixture.context.change.repository,
        1,
        &fixture.context.revision,
        &fixture.recipe,
        false,
    );
    plain.change.id = "change-2".into();
    fixture
        .authority
        .start_change(&plain)
        .expect("plain change");
    assert!(matches!(
        fixture
            .authority
            .create_command_attempt(&request(&plain, fixture.recipe.clone(), "plain")),
        Err(Error::Invalid(message)) if message.contains("does not request capability")
    ));
    fixture.context = plain;
    fixture.approve("plain-approval").expect("no record needed");
}

#[test]
fn tampered_record_bytes_are_refused() {
    let mut fixture = Fixture::fake();
    let finished = fixture.create_and_run("attempt-1");
    fixture
        .raw()
        .execute(
            "UPDATE command_evidence SET canonical_json = CAST(replace(CAST(canonical_json AS TEXT), '\"outcome\":\"success\"', '\"outcome\":\"failed\"') AS BLOB)",
            [],
        )
        .expect("tamper");
    assert!(
        fixture
            .authority
            .command_record(finished.record_digest.as_deref().expect("digest"))
            .is_err()
    );
    fixture.refused("tampered");
}

#[test]
fn outcome_column_cannot_override_the_record() {
    let mut fixture = Fixture::fake();
    fixture.scenario("nonzero");
    fixture.create_and_run("attempt-1");
    fixture
        .raw()
        .execute("UPDATE command_attempts SET outcome = 'success'", [])
        .expect("tamper outcome");
    fixture.refused("forged-outcome");
}

#[test]
fn record_collected_for_another_attempt_is_refused() {
    let mut fixture = Fixture::fake();
    let finished = fixture.create_and_run("attempt-1");
    fixture.create("copy");
    let raw = fixture.raw();
    let digest = finished.record_digest.expect("digest");
    // The record digest is unique, so it cannot be attached to a second attempt in place.
    assert!(
        raw.execute(
            "UPDATE command_attempts SET state = 'finished', outcome = 'success', run_generation = 1, record_digest = ?1 WHERE attempt_id = 'copy'",
            [&digest],
        )
        .is_err()
    );
    // Moving it instead leaves a record that names a different attempt.
    raw.execute_batch(
        "UPDATE command_attempts SET state = 'pending', outcome = NULL, record_digest = NULL, run_generation = 0 WHERE attempt_id = 'attempt-1'",
    )
    .expect("detach");
    raw.execute(
        "UPDATE command_attempts SET state = 'finished', outcome = 'success', run_generation = 1, record_digest = ?1 WHERE attempt_id = 'copy'",
        [&digest],
    )
    .expect("attach");
    fixture.refused("copied");
}

#[test]
fn schema_rejects_finished_attempts_without_a_record() {
    let mut fixture = Fixture::fake();
    fixture.create("attempt-1");
    let raw = fixture.raw();
    for update in [
        "UPDATE command_attempts SET state = 'finished'",
        "UPDATE command_attempts SET state = 'finished', outcome = 'success'",
        "UPDATE command_attempts SET state = 'running', record_digest = 'ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff'",
    ] {
        assert!(raw.execute(update, []).is_err(), "{update}");
    }
}

#[test]
fn cancelling_a_pending_attempt_is_terminal_and_never_admits() {
    let mut fixture = Fixture::fake();
    fixture.create("attempt-1");
    let cancelled = fixture.cancel("attempt-1", 0).expect("cancel");
    assert_eq!(cancelled.state, CommandState::Finished);
    assert_eq!(cancelled.outcome, Some(ExecutionOutcome::Cancelled));
    assert_eq!(cancelled.run_generation, 1);
    let record = fixture
        .authority
        .command_record(cancelled.record_digest.as_deref().expect("digest"))
        .expect("record");
    assert_eq!(record.outcome, ExecutionOutcome::Cancelled);
    assert!(!record.isolation_observed && record.cleanup_confirmed);
    assert_eq!(record.request.run_generation, 1);
    assert_eq!(record.detail, "operator cancel");
    fixture.refused("cancelled");
    assert_eq!(fixture.run("attempt-1", 0).expect("run"), cancelled);
    assert_eq!(fixture.cancel("attempt-1", 0).expect("again"), cancelled);
    assert!(matches!(
        fixture.recover("attempt-1", 0),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn pending_attempts_cannot_be_recovered_and_cancel_checks_the_generation() {
    let mut fixture = Fixture::fake();
    fixture.create("attempt-1");
    assert!(matches!(
        fixture.recover("attempt-1", 0),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        fixture.cancel("attempt-1", 5),
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        fixture
            .authority
            .command_attempt("attempt-1")
            .expect("attempt")
            .state,
        CommandState::Pending
    );
}

#[test]
fn recovery_wins_over_a_late_finisher() {
    let mut fixture = Fixture::fake();
    fixture.scenario("timeout");
    fixture.create("attempt-1");
    let backend = fixture.second_backend();
    let runner_authority =
        Authority::open(fixture.temporary.path().join("authority")).expect("runner authority");
    let calls = fixture.config.join("calls.jsonl");
    let (engine, work, runtime) = (&fixture.engine, &fixture.work, &fixture.backend);
    let (recovered, runner) = thread::scope(|scope| {
        let runner = scope.spawn(move || {
            let mut authority = runner_authority;
            authority.run_command_attempt(
                engine,
                runtime,
                work,
                "attempt-1",
                0,
                &Cancellation::default(),
            )
        });
        wait_for_log(&calls, "\"start\"");
        let recovered = fixture.authority.recover_command_attempt(
            Some(&backend),
            work,
            "attempt-1",
            1,
            false,
            "operator recover",
        );
        (recovered, runner.join().expect("runner thread"))
    });
    let recovered = recovered.expect("recovery");
    assert_eq!(recovered.state, CommandState::Finished);
    assert_eq!(recovered.outcome, Some(ExecutionOutcome::Incomplete));
    let record = fixture
        .authority
        .command_record(recovered.record_digest.as_deref().expect("digest"))
        .expect("record");
    assert!(record.cleanup_confirmed && record.cleanup_handle.is_none());
    match runner {
        Ok(attempt) => assert_eq!(attempt, recovered),
        Err(error) => assert!(matches!(error, Error::Conflict(_)), "{error}"),
    }
    assert_eq!(fixture.evidence_rows(), 1);
    fixture.refused("late-finisher");
}

#[test]
fn lost_remove_reply_stays_running_until_recovery_confirms_removal() {
    let mut fixture = Fixture::fake();
    fixture.scenario("lost_remove_reply");
    fixture.create("attempt-1");
    assert!(matches!(
        fixture.run("attempt-1", 0),
        Err(Error::Conflict(message)) if message.contains("check-recover-command attempt-1 1")
    ));
    let running = fixture
        .authority
        .command_attempt("attempt-1")
        .expect("attempt");
    assert_eq!(running.state, CommandState::Running);
    assert_eq!(running.run_generation, 1);
    let custody = running.custody.expect("custody");
    assert_eq!(custody.run_generation, 1);
    assert!(custody.handle.container_id.is_some());
    assert_eq!(fixture.evidence_rows(), 0);

    fixture.scenario("success");
    let recovered = fixture.recover("attempt-1", 1).expect("recover");
    assert_eq!(recovered.outcome, Some(ExecutionOutcome::Incomplete));
    assert_eq!(recovered.run_generation, 2);
    assert!(recovered.custody.is_none());
    fixture.refused("recovered");
}

#[test]
fn unknown_container_identity_is_not_closed_by_name_absence() {
    let mut fixture = Fixture::fake();
    fixture.claim_with_unknown_container("attempt-1");
    for generation in [1, 2] {
        assert!(matches!(
            fixture.recover("attempt-1", generation),
            Err(Error::Conflict(message)) if message.contains("unconfirmed")
        ));
        let attempt = fixture
            .authority
            .command_attempt("attempt-1")
            .expect("attempt");
        assert_eq!(attempt.state, CommandState::Running);
        assert_eq!(attempt.run_generation, generation + 1);
        assert!(attempt.custody.is_some());
    }
    assert_eq!(fixture.evidence_rows(), 0);
    fixture.refused("unconfirmed");

    // Only an explicit operator acknowledgement closes it, as indeterminate with its handle.
    let abandon = |fixture: &mut Fixture| {
        fixture.authority.recover_command_attempt(
            Some(&fixture.backend),
            &fixture.work,
            "attempt-1",
            3,
            true,
            "operator checked the daemon",
        )
    };
    let abandoned = abandon(&mut fixture).expect("abandon");
    assert_eq!(abandoned.state, CommandState::Finished);
    assert_eq!(abandoned.outcome, Some(ExecutionOutcome::Indeterminate));
    let record = fixture
        .authority
        .command_record(abandoned.record_digest.as_deref().expect("digest"))
        .expect("record");
    assert!(!record.cleanup_confirmed && record.cleanup_handle.is_some());
    assert_eq!(abandon(&mut fixture).expect("idempotent"), abandoned);
    fixture.refused("abandoned");
}

#[test]
fn recovery_with_custody_requires_a_runtime_and_does_not_fence_without_one() {
    let mut fixture = Fixture::fake();
    fixture.claim_with_unknown_container("attempt-1");
    assert!(matches!(
        fixture
            .authority
            .recover_command_attempt(None, &fixture.work, "attempt-1", 1, false, "no runtime"),
        Err(Error::Invalid(message)) if message.contains("Docker runtime")
    ));
    let attempt = fixture
        .authority
        .command_attempt("attempt-1")
        .expect("attempt");
    assert_eq!(attempt.run_generation, 1);
    assert!(attempt.custody.is_some());
}

#[test]
fn recovery_without_custody_needs_no_runtime_and_removes_the_stale_snapshot() {
    let mut fixture = Fixture::fake();
    fixture.create("attempt-1");
    fixture
        .raw()
        .execute(
            "UPDATE command_attempts SET state = 'running', run_generation = 1",
            [],
        )
        .expect("claim");
    let stale = fixture.work.join("snapshot-attempt-1-1");
    fs::create_dir(&stale).expect("stale snapshot");
    private(&stale);
    fs::write(stale.join("file"), b"stale").expect("stale file");
    let recovered = fixture
        .authority
        .recover_command_attempt(None, &fixture.work, "attempt-1", 1, false, "crashed")
        .expect("recover");
    assert_eq!(recovered.outcome, Some(ExecutionOutcome::Incomplete));
    assert!(!stale.exists());
    fixture.refused("crashed");
}

#[test]
fn cli_runs_command_check_from_default_relative_data_dir_and_admits() {
    let temporary = TempDir::new().expect("temporary fixture");
    let root = temporary.path();
    let docker = root.join("fake-docker.py");
    fs::write(&docker, FAKE_DOCKER).expect("write fake Docker");
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).expect("executable");
    let socket = root.join("docker.sock");
    let _listener = UnixListener::bind(&socket).expect("private Unix socket");
    let cli = |args: &[&str], input: Option<serde_json::Value>| {
        let file = root.join("input.json");
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command.current_dir(root).args(args);
        if let Some(value) = input {
            fs::write(&file, serde_json::to_vec(&value).expect("json")).expect("input");
            command.arg(&file);
        }
        let output = command.output().expect("run agentscm");
        let value: serde_json::Value = serde_json::from_slice(if output.status.success() {
            &output.stdout
        } else {
            &output.stderr
        })
        .expect("json output");
        (output.status.success(), value)
    };

    let repository = cli(&["init"], None).1["repository"]
        .as_str()
        .expect("repository")
        .to_owned();
    let revision = cli(&["checkpoint"], Some(serde_json::json!({"changes":[{"path":"README.md","content":b"trusted bytes\n".to_vec()}]})))
        .1["revision"].as_str().expect("revision").to_owned();
    let mut draft =
        serde_json::to_value(recipe(vec!["/bin/true".into()], BTreeMap::new())).expect("recipe");
    draft["environment_sha256"] = serde_json::json!("0".repeat(64));
    let (ok, digests) = cli(&["check-command-recipe"], Some(draft));
    assert!(ok, "{digests}");
    let recipe: CommandRecipe =
        serde_json::from_value(digests["recipe"].clone()).expect("filled recipe");
    assert_eq!(digests["recipe_sha256"], recipe.digest().expect("digest"));
    let context = make_context(&repository, 1, &revision, &recipe, true);
    assert!(
        cli(
            &["start"],
            Some(serde_json::to_value(&context).expect("context"))
        )
        .0
    );
    let approve = || {
        cli(
            &["approve"],
            Some(serde_json::to_value(approval(&context, "cli")).expect("approval")),
        )
    };
    assert!(!approve().0);

    let (ok, created) = cli(
        &["check-command"],
        Some(serde_json::to_value(request(&context, recipe.clone(), "cli-1")).expect("request")),
    );
    assert!(ok, "{created}");
    let (ok, ran) = cli(
        &[
            "check-run-command",
            "cli-1",
            "0",
            "--docker",
            docker.to_str().expect("path"),
            "--docker-socket",
            socket.to_str().expect("path"),
        ],
        None,
    );
    assert!(ok, "{ran}");
    assert_eq!(ran["attempt"]["outcome"], "success");
    assert_eq!(
        ran["record"]["request"]["subject"]["revision"],
        revision.as_str()
    );
    assert_eq!(
        cli(&["check-command-show", "cli-1"], None).1["attempt"],
        ran["attempt"]
    );
    assert!(approve().0);
    assert!(
        cli(
            &["check-command"],
            Some(
                serde_json::to_value(request(&context, recipe.clone(), "cli-2")).expect("request")
            ),
        )
        .0
    );
    let (ok, cancelled) = cli(
        &["check-cancel-command", "cli-2", "0", "operator stop"],
        None,
    );
    assert!(ok, "{cancelled}");
    assert_eq!(cancelled["attempt"]["outcome"], "cancelled");
    assert_eq!(cancelled["record"]["detail"], "operator stop");
    // A run whose stdout is not the candidate protocol yields no candidate.
    let (ok, error) = cli(&["check-command-candidate", "cli-1"], None);
    assert!(
        !ok && error.to_string().contains("not a valid candidate"),
        "{error}"
    );
    // The same fake daemon, pinned to the candidate scenario with the agent's stdout.
    fs::write(root.join("agent-stdout"), edits()).expect("agent stdout");
    let agent = root.join("fake-agent-docker.py");
    let script = FAKE_DOCKER
        .replace(
            "scenario = scenario_path.read_text().strip() if scenario_path.exists() else \"success\"",
            "scenario = \"candidate\"",
        )
        .replace(
            "home / \"candidate.json\"",
            &format!("pathlib.Path({:?})", root.join("agent-stdout")),
        );
    fs::write(&agent, script).expect("write fake agent Docker");
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).expect("executable");
    let (ok, created) = cli(
        &["check-command"],
        Some(serde_json::to_value(request(&context, recipe, "cli-3")).expect("request")),
    );
    assert!(ok, "{created}");
    let (ok, ran) = cli(
        &[
            "check-run-command",
            "cli-3",
            "0",
            "--docker",
            agent.to_str().expect("path"),
            "--docker-socket",
            socket.to_str().expect("path"),
        ],
        None,
    );
    assert!(ok && ran["attempt"]["outcome"] == "success", "{ran}");
    let (ok, derived) = cli(&["check-command-candidate", "cli-3"], None);
    assert!(ok, "{derived}");
    assert_eq!(derived["candidate"]["base_revision"], revision.as_str());
    assert_eq!(derived["candidate"]["changes"], 2);
    assert_eq!(cli(&["check-command-candidate", "cli-3"], None).1, derived);
    // Both docker flags are needed together.
    let (ok, error) = cli(
        &[
            "check-recover-command",
            "cli-2",
            "1",
            "x",
            "--docker",
            "/bin/true",
        ],
        None,
    );
    assert!(
        !ok && error.to_string().contains("--docker-socket"),
        "{error}"
    );
    // Snapshots and the Docker configuration are transient.
    assert_eq!(
        fs::read_dir(root.join(".agentscm/execution"))
            .expect("work dir")
            .count(),
        0
    );
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn real_runtime_checks_source_isolation_and_admits() {
    let docker = required_absolute_env("AGENTSCM_DOCKER");
    let socket = required_absolute_env("AGENTSCM_DOCKER_SOCKET");
    let image = std::env::var("AGENTSCM_OCI_IMAGE")
        .expect("set AGENTSCM_OCI_IMAGE to sha256:<64 lowercase hex>");
    let script = "set -eu; test \"$(/bin/busybox id -u)\" = 65532; test \"$(/bin/busybox cat /source/README.md)\" = 'trusted bytes'; ! /bin/busybox touch /source/mutation; ! /bin/busybox wget -T 2 -q -O- http://1.1.1.1; test -z \"$(/bin/busybox ip route 2>/dev/null)\"";
    let mut recipe = recipe(
        vec!["/bin/sh".into(), "-c".into(), script.into()],
        BTreeMap::from([(
            "PATH".into(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
        )]),
    );
    recipe.image = image;
    let mut fixture = Fixture::new(recipe, Some((docker, socket)));
    let finished = fixture.create_and_run("live-1");
    let record = fixture
        .authority
        .command_record(finished.record_digest.as_deref().expect("digest"))
        .expect("record");
    assert_eq!(
        finished.outcome,
        Some(ExecutionOutcome::Success),
        "{record:?}"
    );
    assert!(record.isolation_observed && record.cleanup_confirmed);
    fixture.approve("live-approval").expect("admitted");
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn real_runtime_recovery_removes_a_running_candidate() {
    let docker = required_absolute_env("AGENTSCM_DOCKER");
    let socket = required_absolute_env("AGENTSCM_DOCKER_SOCKET");
    let image = std::env::var("AGENTSCM_OCI_IMAGE")
        .expect("set AGENTSCM_OCI_IMAGE to sha256:<64 lowercase hex>");
    let mut recipe = recipe(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "/bin/busybox sleep 30".into(),
        ],
        BTreeMap::from([(
            "PATH".into(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
        )]),
    );
    recipe.image = image;
    recipe.limits.timeout_ms = 60_000;
    let mut fixture = Fixture::new(recipe, Some((docker.clone(), socket.clone())));
    fixture.create("live-recover");
    let runner_authority =
        Authority::open(fixture.temporary.path().join("authority")).expect("runner authority");
    let (engine, work, runtime) = (&fixture.engine, &fixture.work, &fixture.backend);
    let (recovered, container, runner) = thread::scope(|scope| {
        let runner = scope.spawn(move || {
            let mut authority = runner_authority;
            authority.run_command_attempt(
                engine,
                runtime,
                work,
                "live-recover",
                0,
                &Cancellation::default(),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let container = loop {
            let attempt = fixture
                .authority
                .command_attempt("live-recover")
                .expect("attempt");
            if let Some(id) = attempt
                .custody
                .clone()
                .and_then(|custody| custody.handle.container_id)
            {
                break id;
            }
            assert!(
                Instant::now() < deadline,
                "no container custody was persisted: {attempt:?}"
            );
            thread::sleep(Duration::from_millis(100));
        };
        thread::sleep(Duration::from_secs(1));
        let recovered = fixture.authority.recover_command_attempt(
            Some(runtime),
            work,
            "live-recover",
            1,
            false,
            "live recovery",
        );
        (recovered, container, runner.join().expect("runner thread"))
    });
    let recovered = recovered.expect("recovery");
    assert_eq!(recovered.outcome, Some(ExecutionOutcome::Incomplete));
    let inspect = std::process::Command::new(&docker)
        .arg("--host")
        .arg(format!("unix://{}", socket.display()))
        .args(["container", "inspect", &container])
        .output()
        .expect("inspect container");
    assert!(
        !inspect.status.success(),
        "container {container} still exists"
    );
    if let Ok(attempt) = runner {
        assert_ne!(attempt.outcome, Some(ExecutionOutcome::Success));
    }
    fixture.refused("live-recovered");
}

impl Fixture {
    /// Run a successful attempt whose stdout is `stdout`, as the agent would print it.
    fn run_agent(&mut self, attempt_id: &str, stdout: &str) -> CommandAttempt {
        fs::write(self.config.join("candidate.json"), stdout).expect("write agent stdout");
        self.scenario("candidate");
        self.create_and_run(attempt_id)
    }

    fn candidate(&self, attempt_id: &str) -> Result<CommandCandidate, Error> {
        self.authority.command_candidate(&self.engine, attempt_id)
    }

    fn integrate(
        &mut self,
        context: &ReviewContext,
        revision: &str,
        approval_id: &str,
    ) -> Result<String, Error> {
        self.authority
            .integrate(
                &self.engine,
                &IntegrateRequest {
                    operation_id: format!("integrate-{approval_id}"),
                    target: context.target.clone(),
                    expected_target: self.authority.target(&context.target).expect("target"),
                    revision: revision.to_owned(),
                    context_digest: context.digest().expect("context digest"),
                    approval_id: approval_id.to_owned(),
                    change: Some(context.change.clone()),
                    change_version: Some(context.change_version),
                    policy_epoch: context.policy_epoch,
                },
            )
            .map(|event| event.after.revision)
    }

    /// A fresh review context (version 2) on the candidate revision, replacing the run's context.
    fn context_for(&mut self, revision: &str, requires_command: bool) -> ReviewContext {
        let mut next = make_context(
            &self.context.change.repository,
            2,
            revision,
            &self.recipe,
            requires_command,
        );
        next.base = self
            .authority
            .target(&next.target)
            .expect("target")
            .map(|target| BaseState {
                revision: target.revision,
                target_version: target.version,
            });
        self.authority
            .update_change_context(1, &next)
            .expect("update context");
        next
    }
}

fn agent_stdout(changes: &[(&str, Option<&[u8]>)]) -> String {
    let changes: Vec<_> = changes
        .iter()
        .map(|(path, bytes)| {
            let hex = bytes.map(|bytes| {
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            });
            serde_json::json!({"path": path, "hex": hex})
        })
        .collect();
    serde_json::json!({"version": 1, "changes": changes}).to_string()
}

fn edits() -> String {
    agent_stdout(&[
        ("agent.txt", Some(b"agent\n")),
        ("README.md", Some(b"agent edited\n")),
    ])
}

#[test]
fn candidate_is_derived_deterministically_and_lands_only_through_a_fresh_context() {
    let mut fixture = Fixture::fake();
    fixture.run_agent("attempt-1", &edits());
    let candidate = fixture.candidate("attempt-1").expect("candidate");
    let base = fixture.context.revision.clone();
    assert_eq!(candidate.base_revision, base);
    assert_eq!(candidate.changes, 2);
    assert_eq!(
        fixture
            .engine
            .load_revision(&candidate.revision)
            .expect("revision")
            .parents,
        std::slice::from_ref(&base)
    );
    let read = |path| {
        fixture
            .engine
            .read_file(&candidate.revision, path)
            .expect("file")
    };
    assert_eq!(read("agent.txt"), b"agent\n");
    assert_eq!(read("README.md"), b"agent edited\n");
    assert_eq!(fixture.candidate("attempt-1").expect("again"), candidate);

    // The run's own context admits only the exact revision it ran against: it can neither approve
    // the candidate nor integrate it with a fresh, unconsumed approval of its own revision.
    let old = fixture.context.clone();
    let mut stretched = approval(&old, "old-on-candidate");
    stretched.revision = candidate.revision.clone();
    let refused = fixture.authority.approve(&stretched);
    assert!(matches!(refused, Err(Error::Conflict(_))), "{refused:?}");
    fixture.approve("old-unused").expect("approve old context");
    let refused = fixture.integrate(&old, &candidate.revision, "old-unused");
    assert!(matches!(refused, Err(Error::Conflict(_))), "{refused:?}");
    fixture.approve("old").expect("approve old context");
    fixture
        .integrate(&old, &old.revision, "old")
        .expect("land the base");

    let fresh = fixture.context_for(&candidate.revision, false);
    let approve = approval(&fresh, "fresh");
    fixture
        .authority
        .approve(&approve)
        .expect("approve fresh context");
    let landed = fixture
        .integrate(&fresh, &candidate.revision, "fresh")
        .expect("integrate");
    assert_eq!(landed, candidate.revision);
}

#[test]
fn candidate_context_requiring_the_command_needs_a_fresh_run_on_the_candidate() {
    let mut fixture = Fixture::fake();
    fixture.run_agent("attempt-1", &edits());
    let candidate = fixture.candidate("attempt-1").expect("candidate");
    let fresh = fixture.context_for(&candidate.revision, true);
    let approve = approval(&fresh, "fresh");
    assert!(matches!(
        fixture.authority.approve(&approve),
        Err(Error::Invalid(message)) if message.contains("no exact successful controller-collected attempt")
    ));
    fixture.scenario("success");
    let request = request(&fresh, fixture.recipe.clone(), "attempt-2");
    fixture
        .authority
        .create_command_attempt(&request)
        .expect("create on candidate");
    fixture.run("attempt-2", 0).expect("run on candidate");
    fixture
        .authority
        .approve(&approve)
        .expect("approve after fresh run");
}

#[test]
fn candidate_is_refused_unless_the_run_succeeded_with_a_valid_stdout() {
    let mut fixture = Fixture::fake();
    fixture.create("pending");
    assert!(
        matches!(fixture.candidate("pending"), Err(Error::Invalid(message)) if message.contains("not finished"))
    );
    assert!(matches!(
        fixture.candidate("missing"),
        Err(Error::NotFound(_))
    ));

    fixture.scenario("nonzero");
    fixture.create_and_run("failed");
    assert!(fixture.candidate("failed").is_err());

    // A valid candidate followed by padding past the run's bound is truncated to output_limit; the
    // retained prefix would still parse, so only the outcome check stops a smaller candidate.
    fs::write(fixture.config.join("candidate.json"), edits()).expect("agent stdout");
    fixture.scenario("candidate_flood");
    let flooded = fixture.create_and_run("flood");
    assert_eq!(flooded.outcome, Some(ExecutionOutcome::OutputLimit));
    assert!(
        matches!(fixture.candidate("flood"), Err(Error::Invalid(message)) if message.contains("has not finished with success"))
    );

    fixture.scenario("success");
    fixture.create_and_run("plain");
    assert!(
        matches!(fixture.candidate("plain"), Err(Error::Invalid(message)) if message.contains("not a valid candidate"))
    );

    for (id, stdout) in [
        (
            "uppercase",
            agent_stdout(&[("a", Some(b"\xab"))]).replace("ab", "AB"),
        ),
        ("escape", agent_stdout(&[("../a", Some(b"x"))])),
        ("git", agent_stdout(&[("sub/.GIT/config", Some(b"x"))])),
        ("empty", agent_stdout(&[])),
        ("trailing", format!("{} x", edits())),
    ] {
        fixture.run_agent(id, &stdout);
        assert!(
            matches!(fixture.candidate(id), Err(Error::Invalid(_))),
            "{id}"
        );
    }

    fixture.run_agent(
        "noop",
        &agent_stdout(&[("README.md", Some(b"trusted bytes\n"))]),
    );
    assert!(
        matches!(fixture.candidate("noop"), Err(Error::Invalid(message)) if message.contains("does not change"))
    );
}

#[test]
fn candidate_is_refused_when_the_stored_record_was_tampered_with() {
    let mut fixture = Fixture::fake();
    let finished = fixture.run_agent("attempt-1", &edits());
    fixture
        .candidate("attempt-1")
        .expect("candidate before tampering");
    let digest = finished.record_digest.expect("digest");
    let raw = fixture.raw();
    let bytes: Vec<u8> = raw
        .query_row(
            "SELECT canonical_json FROM command_evidence WHERE digest = ?1",
            [&digest],
            |row| row.get(0),
        )
        .expect("record bytes");
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("record json");
    value["stdout"][0] = serde_json::json!(32);
    raw.execute(
        "UPDATE command_evidence SET canonical_json = ?1 WHERE digest = ?2",
        rusqlite::params![serde_json::to_vec(&value).expect("json"), digest],
    )
    .expect("tamper");
    assert!(
        matches!(fixture.candidate("attempt-1"), Err(Error::Invalid(message)) if message.contains("integrity"))
    );
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn real_runtime_agent_writes_scratch_and_lands_as_candidate() {
    let docker = required_absolute_env("AGENTSCM_DOCKER");
    let socket = required_absolute_env("AGENTSCM_DOCKER_SOCKET");
    let image = std::env::var("AGENTSCM_OCI_IMAGE")
        .expect("set AGENTSCM_OCI_IMAGE to sha256:<64 lowercase hex>");
    // Only /scratch is writable and Docker cannot copy a tmpfs out, so the agent prints its result.
    let script = r#"set -eu
b=/bin/busybox
$b mkdir -p /scratch/out/dir/nested
printf 'agent\n' > /scratch/out/agent.txt
printf 'nested\n' > /scratch/out/dir/nested/file.txt
{ $b cat /source/README.md; printf 'edited by agent\n'; } > /scratch/out/README.md
cd /scratch/out
printf '{"version":1,"changes":['
sep=
for f in $($b find . -type f | $b sed 's|^\./||' | $b sort); do
  printf '%s{"path":"%s","hex":"%s"}' "$sep" "$f" "$($b od -An -tx1 -v "$f" | $b tr -d ' \n')"
  sep=,
done
printf ']}\n'"#;
    let mut recipe = recipe(
        vec!["/bin/sh".into(), "-c".into(), script.into()],
        BTreeMap::from([(
            "PATH".into(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
        )]),
    );
    recipe.image = image;
    let mut fixture = Fixture::new(recipe, Some((docker, socket)));
    let finished = fixture.create_and_run("live-agent");
    let record = fixture
        .authority
        .command_record(finished.record_digest.as_deref().expect("digest"))
        .expect("record");
    assert_eq!(
        finished.outcome,
        Some(ExecutionOutcome::Success),
        "{record:?}"
    );
    let candidate = fixture.candidate("live-agent").expect("candidate");
    let read = |path| {
        fixture
            .engine
            .read_file(&candidate.revision, path)
            .expect("file")
    };
    assert_eq!(candidate.changes, 3);
    assert_eq!(read("agent.txt"), b"agent\n");
    assert_eq!(read("dir/nested/file.txt"), b"nested\n");
    assert_eq!(read("README.md"), b"trusted bytes\nedited by agent\n");

    fixture
        .approve("live-old")
        .expect("approve the run's context");
    let old = fixture.context.clone();
    fixture
        .integrate(&old, &old.revision, "live-old")
        .expect("land the base");
    let fresh = fixture.context_for(&candidate.revision, false);
    fixture
        .authority
        .approve(&approval(&fresh, "live-fresh"))
        .expect("approve the candidate");
    let landed = fixture
        .integrate(&fresh, &candidate.revision, "live-fresh")
        .expect("land the candidate");
    assert_eq!(landed, candidate.revision);
}

fn wait_for_log(path: &Path, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(path)
        .unwrap_or_default()
        .contains(needle)
    {
        assert!(
            Instant::now() < deadline,
            "fake Docker phase {needle} was not observed"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn private(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private directory");
}

fn recipe(argv: Vec<String>, environment: BTreeMap<String, String>) -> CommandRecipe {
    CommandRecipe {
        version: 1,
        image: format!("sha256:{IMAGE}"),
        argv,
        environment_sha256: environment_digest(&environment).expect("environment digest"),
        environment,
        toolchain_sha256: TOOLCHAIN.into(),
        limits: ExecutionLimits {
            timeout_ms: 5_000,
            max_output_bytes: 16 * 1024,
            memory_bytes: 64 * 1024 * 1024,
            scratch_bytes: 4 * 1024 * 1024,
            pids: 16,
            cpu_millis: 1_000,
        },
        syscall_policy: SyscallPolicy::DockerBuiltin,
        no_new_privileges: true,
    }
}

fn make_context(
    repository: &str,
    version: u64,
    revision: &str,
    recipe: &CommandRecipe,
    requires_command: bool,
) -> ReviewContext {
    let digest = |sha256: &str| InputIdentity::Digest {
        sha256: sha256.to_owned(),
    };
    let recipe_digest = recipe.digest().expect("recipe digest");
    let mut capabilities = vec!["agentscm.review-context/v1".to_owned()];
    let mut required_evidence = Vec::new();
    if requires_command {
        capabilities.push(COMMAND_CAPABILITY.to_owned());
        required_evidence.push(RequiredEvidence {
            identity: COMMAND_EVIDENCE.to_owned(),
            digest: digest(&recipe_digest),
        });
    }
    ReviewContext {
        version: 1,
        change: ChangeId {
            repository: repository.to_owned(),
            id: "change-1".to_owned(),
        },
        change_version: version,
        revision: revision.to_owned(),
        base: None,
        target: "main".to_owned(),
        policy_epoch: 0,
        intent: "run a declared command against the exact revision".to_owned(),
        acceptance_criteria: vec!["command exits zero in isolation".to_owned()],
        scope: vec!["README.md".to_owned()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: digest(&recipe_digest),
        toolchain: digest(&recipe.toolchain_sha256),
        environment: digest(&recipe.environment_sha256),
        required_evidence,
        required_capabilities: capabilities,
    }
}

fn request(
    context: &ReviewContext,
    recipe: CommandRecipe,
    attempt_id: &str,
) -> CommandCheckRequest {
    CommandCheckRequest {
        attempt_id: attempt_id.to_owned(),
        change: context.change.clone(),
        change_version: context.change_version,
        context_digest: context.digest().expect("context digest"),
        revision: context.revision.clone(),
        recipe,
    }
}

fn approval(context: &ReviewContext, id: &str) -> Approval {
    Approval {
        approval_id: id.to_owned(),
        target: context.target.clone(),
        revision: context.revision.clone(),
        context_digest: context.digest().expect("context digest"),
        change: Some(context.change.clone()),
        change_version: Some(context.change_version),
        policy_epoch: context.policy_epoch,
    }
}

fn required_absolute_env(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")));
    assert!(path.is_absolute(), "{name} must be absolute");
    path
}
