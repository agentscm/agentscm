#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::fake_docker::FAKE_DOCKER;
use agentscm::Error;
use agentscm::authority::Authority;
use agentscm::checks::SourcePathExpectation;
use agentscm::context::{ChangeId, InputIdentity, RequiredEvidence, ReviewContext};
use agentscm::execution::{
    COMMAND_CAPABILITY, COMMAND_EVIDENCE, Cancellation, CleanupCustody, CleanupHandle,
    CommandCheckRequest, CommandRecipe, CommandState, ExecutionLimits, ExecutionRequest,
    OciBackend, SyscallPolicy, environment_digest,
};
use agentscm::model::FileChange;
use agentscm::skill_workflow::runtime::{
    CommandExecutor, advance_with_executor, cancel_with_executor, command_attempt_id,
    recover_command,
};
use agentscm::skill_workflow::{
    RunState, SkillReference, StepAction, StepDefinition, StepState, WorkflowDefinition,
    WorkflowSource, advance, compile, start, status,
};
use agentscm::store::{Engine, digest};
use rusqlite::{Connection, params};
use tempfile::TempDir;

const SKILL_PATH: &str = "skills/command-workflow/SKILL.md";
const RUNNER_PATH: &str = "skills/command-workflow/runner.json";
const RUNNER_RESOURCE: &str = "runner.json";
const SOURCE_PATH: &str = "src/example.rs";
const SOURCE_BYTES: &[u8] = b"pub fn answer() -> u8 { 42 }\n";
const IMAGE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const TOOLCHAIN: &str = "2222222222222222222222222222222222222222222222222222222222222222";

struct Fixture {
    root: TempDir,
    _socket: Option<UnixListener>,
    backend: OciBackend,
    config: PathBuf,
    work: PathBuf,
    repository: String,
    revision: String,
    context: ReviewContext,
    recipe: CommandRecipe,
    cancellation: Cancellation,
}

impl Fixture {
    fn fake() -> Self {
        Self::new(recipe(), None)
    }

    fn new(recipe: CommandRecipe, runtime: Option<(PathBuf, PathBuf)>) -> Self {
        let root = TempDir::new().expect("temporary native root");
        private(root.path());
        let config = root.path().join("docker-config");
        fs::create_dir(&config).expect("Docker configuration directory");
        private(&config);
        let work = root.path().join("execution-work");
        fs::create_dir(&work).expect("execution work directory");
        private(&work);
        let (socket, backend) = match runtime {
            Some((docker, socket)) => (
                None,
                OciBackend::new(&docker, &socket, &config).expect("real OCI backend"),
            ),
            None => {
                let docker = root.path().join("fake-docker.py");
                fs::write(&docker, FAKE_DOCKER).expect("write fake Docker");
                fs::set_permissions(&docker, fs::Permissions::from_mode(0o700))
                    .expect("make fake Docker executable");
                let socket_path = root.path().join("docker.sock");
                let listener = UnixListener::bind(&socket_path).expect("private Unix socket");
                let backend =
                    OciBackend::new(&docker, &socket_path, &config).expect("fake OCI backend");
                (Some(listener), backend)
            }
        };

        let mut authority = Authority::open(root.path()).expect("authority");
        let repository = authority.repository_id().expect("repository ID");
        let engine = Engine::open(root.path()).expect("native engine");
        let revision = engine
            .checkpoint(
                None,
                &[
                    put(SKILL_PATH, b"# Command workflow\n"),
                    put(
                        RUNNER_PATH,
                        &serde_json::to_vec(&recipe).expect("command recipe JSON"),
                    ),
                    put(SOURCE_PATH, SOURCE_BYTES),
                ],
            )
            .expect("native checkpoint");
        let context = make_context(&repository, 1, &revision, &recipe);
        authority.start_change(&context).expect("start real change");
        drop(authority);
        drop(engine);

        Self {
            root,
            _socket: socket,
            backend,
            config,
            work,
            repository,
            revision,
            context,
            recipe,
            cancellation: Cancellation::default(),
        }
    }

    fn data_dir(&self) -> &Path {
        self.root.path()
    }

    fn executor(&self) -> CommandExecutor<'_> {
        CommandExecutor {
            backend: &self.backend,
            work_dir: &self.work,
            cancellation: &self.cancellation,
        }
    }

    fn executor_with<'a>(&'a self, cancellation: &'a Cancellation) -> CommandExecutor<'a> {
        CommandExecutor {
            backend: &self.backend,
            work_dir: &self.work,
            cancellation,
        }
    }

    fn definition(&self, steps: Vec<StepDefinition>) -> WorkflowDefinition {
        WorkflowDefinition {
            version: 1,
            name: "native-command-acceptance".into(),
            source: WorkflowSource {
                repository: self.repository.clone(),
                revision: self.revision.clone(),
                policy_epoch: 0,
            },
            skill: SkillReference {
                entrypoint: SKILL_PATH.into(),
                resources: vec![RUNNER_PATH.into()],
            },
            steps,
        }
    }

    fn command_step(&self, id: &str, needs: &[&str], required: bool) -> StepDefinition {
        StepDefinition {
            id: id.into(),
            needs: needs.iter().map(|value| (*value).into()).collect(),
            required,
            action: StepAction::Command {
                runner: RUNNER_RESOURCE.into(),
                change: self.context.change.clone(),
                change_version: self.context.change_version,
                context_digest: self.context.digest().expect("context digest"),
            },
        }
    }

    fn scenario(&self, value: &str) {
        fs::write(self.config.join("scenario"), value).expect("write fake Docker scenario");
    }

    fn docker_calls(&self, fragment: &str) -> usize {
        fs::read_to_string(self.config.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(fragment))
            .count()
    }

    fn cli_backend(&self) -> (PathBuf, PathBuf) {
        let configured_home = serde_json::to_string(
            self.config
                .to_str()
                .expect("UTF-8 fake Docker configuration path"),
        )
        .expect("Python-compatible configuration string");
        let script = FAKE_DOCKER
            .replace(
                "home = pathlib.Path(os.environ[\"HOME\"])",
                &format!("home = pathlib.Path({configured_home})"),
            )
            .replace(
                "if scenario in (\"timeout\", \"cancel\"):",
                "if scenario in (\"timeout\", \"cancel\", \"hanging_attach\"):",
            );
        assert_ne!(script, FAKE_DOCKER, "fake Docker wrapper must be rewritten");
        let docker = self.root.path().join("cli-fake-docker.py");
        fs::write(&docker, script).expect("write CLI fake Docker wrapper");
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o700))
            .expect("make CLI fake Docker executable");
        (docker, self.root.path().join("docker.sock"))
    }

    fn definition_file(&self, definition: &WorkflowDefinition, name: &str) -> PathBuf {
        let path = self.root.path().join(name);
        fs::write(
            &path,
            serde_json::to_vec(definition).expect("workflow definition JSON"),
        )
        .expect("write workflow definition");
        path
    }

    fn authority(&self) -> Authority {
        Authority::open(self.data_dir()).expect("authority")
    }

    fn attempt_id(&self, run_id: &str, plan_digest: &str, step_id: &str) -> String {
        command_attempt_id(run_id, plan_digest, step_id)
    }

    fn create_attempt(&self, run_id: &str, plan_digest: &str, step_id: &str) -> String {
        let attempt_id = self.attempt_id(run_id, plan_digest, step_id);
        self.authority()
            .create_command_attempt(&CommandCheckRequest {
                attempt_id: attempt_id.clone(),
                change: self.context.change.clone(),
                change_version: self.context.change_version,
                context_digest: self.context.digest().expect("context digest"),
                revision: self.revision.clone(),
                recipe: self.recipe.clone(),
            })
            .expect("create durable command attempt");
        attempt_id
    }

    fn raw_authority(&self) -> Connection {
        Connection::open(self.data_dir().join("state.sqlite")).expect("authority database")
    }

    fn mark_workflow_running(&self, run_id: &str, step_id: &str) {
        let journal = Connection::open(self.data_dir().join("skill-workflows/state.sqlite"))
            .expect("workflow journal");
        journal
            .execute(
                "UPDATE workflow_steps SET state='running',generation=1 \
                 WHERE run_id=?1 AND step_id=?2",
                params![run_id, step_id],
            )
            .expect("simulate recorded workflow intent");
        journal
            .execute(
                "UPDATE workflow_runs SET state='running' WHERE run_id=?1",
                [run_id],
            )
            .expect("simulate running workflow");
    }
}

fn cli_command(data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
    command.arg("--data-dir").arg(data_dir);
    command
}

fn wait_for_call(fixture: &Fixture, fragment: &str, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            child.try_wait().expect("inspect CLI process").is_none(),
            "CLI exited before fake Docker observed {fragment}"
        );
        if fixture.docker_calls(fragment) > 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {fragment}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_output(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("inspect CLI process") {
            break status;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGKILL);
            }
            let _ = child.wait();
            panic!("timed out waiting for CLI process");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("captured stdout")
        .read_to_end(&mut stdout)
        .expect("read CLI stdout");
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .expect("captured stderr")
        .read_to_end(&mut stderr)
        .expect("read CLI stderr");
    Output {
        status,
        stdout,
        stderr,
    }
}

fn cli_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "CLI stdout is not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn another_controller_can_inspect_and_cancel_a_running_workflow() {
    let fixture = Fixture::fake();
    fixture.scenario("hanging_attach");
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let definition_path = fixture.definition_file(&definition, "concurrent.json");
    let (docker, socket) = fixture.cli_backend();
    let mut child = cli_command(fixture.data_dir())
        .args(["skill-workflow", "run", "concurrent-cancel"])
        .arg(&definition_path)
        .arg("--docker")
        .arg(&docker)
        .arg("--docker-socket")
        .arg(&socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn running workflow");
    wait_for_call(&fixture, "\"container\", \"start\"", &mut child);
    let running = status(fixture.data_dir(), "concurrent-cancel").expect("journal lease released");
    assert_eq!(running.steps[0].state, StepState::Running);
    assert!(agentscm::skill_workflow::cancel(fixture.data_dir(), "concurrent-cancel").is_err());
    let stopped = cli_command(fixture.data_dir())
        .args(["skill-workflow", "cancel", "concurrent-cancel"])
        .arg("--docker")
        .arg(&docker)
        .arg("--docker-socket")
        .arg(&socket)
        .output()
        .expect("second controller cancellation");
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_eq!(cli_json(&stopped)["state"], "cancelled");
    let late = wait_for_output(child);
    assert!(!late.status.success());
    let retained = status(fixture.data_dir(), "concurrent-cancel").expect("retained cancellation");
    assert_eq!(retained.state, RunState::Cancelled);
    assert_eq!(
        retained.steps[0]
            .execution
            .as_ref()
            .unwrap()
            .outcome
            .as_deref(),
        Some("cancelled")
    );
    assert_eq!(fixture.docker_calls("\"container\", \"start\""), 1);
}

#[test]
fn cli_command_run_requires_a_paired_backend_and_records_terminal_success() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let definition_path = fixture.definition_file(&definition, "cli-command.json");
    let (docker, socket) = fixture.cli_backend();

    let rejected = cli_command(fixture.data_dir())
        .args([
            "skill-workflow",
            "run",
            "unpaired-run",
            definition_path.to_str().expect("UTF-8 definition path"),
            "--docker",
            docker.to_str().expect("UTF-8 Docker path"),
        ])
        .output()
        .expect("invoke CLI with an unpaired backend");
    assert!(!rejected.status.success());
    assert!(
        !fixture.data_dir().join("skill-workflows").exists(),
        "argument rejection must precede workflow journal creation"
    );

    let completed = cli_command(fixture.data_dir())
        .args([
            "skill-workflow",
            "run",
            "cli-command",
            definition_path.to_str().expect("UTF-8 definition path"),
            "--docker",
            docker.to_str().expect("UTF-8 Docker path"),
            "--docker-socket",
            socket.to_str().expect("UTF-8 socket path"),
        ])
        .output()
        .expect("run command workflow through CLI");
    assert!(
        completed.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&completed.stderr)
    );
    let view = cli_json(&completed);
    assert_eq!(view["state"], "succeeded");
    assert_eq!(view["steps"][0]["state"], "succeeded");
    assert_eq!(view["steps"][0]["execution"]["state"], "finished");
    assert_eq!(view["steps"][0]["execution"]["outcome"], "success");
    assert_eq!(view["steps"][0]["execution"]["generation"], 1);
    let record_digest = view["steps"][0]["execution"]["record_digest"]
        .as_str()
        .expect("terminal record digest");
    let record = fixture
        .authority()
        .command_record(record_digest)
        .expect("terminal execution record");
    assert_eq!(record.request.recipe, fixture.recipe);
    assert!(record.cleanup_confirmed);
    assert!(record.cleanup_handle.is_none());
    assert!(record.isolation_observed);
    assert_eq!(fixture.docker_calls("\"container\", \"start\""), 1);
}

#[test]
fn cli_sigterm_cancels_attached_command_and_confirms_cleanup() {
    let fixture = Fixture::fake();
    fixture.scenario("hanging_attach");
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let definition_path = fixture.definition_file(&definition, "cli-signal.json");
    let (docker, socket) = fixture.cli_backend();
    let mut child = cli_command(fixture.data_dir())
        .args([
            "skill-workflow",
            "run",
            "cli-signal",
            definition_path.to_str().expect("UTF-8 definition path"),
            "--docker",
            docker.to_str().expect("UTF-8 Docker path"),
            "--docker-socket",
            socket.to_str().expect("UTF-8 socket path"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn command workflow CLI");
    wait_for_call(
        &fixture,
        "\"container\", \"start\", \"--attach\"",
        &mut child,
    );

    let pid = child.id() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let cancelled = wait_for_output(child);
    assert_eq!(
        cancelled.status.signal(),
        None,
        "CLI must translate SIGTERM into durable cancellation"
    );
    assert!(!cancelled.status.success());
    let view = cli_json(&cancelled);
    assert_eq!(view["state"], "cancelled");
    assert_eq!(view["steps"][0]["state"], "cancelled");
    assert_eq!(view["steps"][0]["execution"]["state"], "finished");
    assert_eq!(view["steps"][0]["execution"]["outcome"], "cancelled");
    let record_digest = view["steps"][0]["execution"]["record_digest"]
        .as_str()
        .expect("cancelled record digest");
    let record = fixture
        .authority()
        .command_record(record_digest)
        .expect("cancelled execution record");
    assert!(record.cleanup_confirmed);
    assert!(record.cleanup_handle.is_none());
    assert_eq!(fixture.docker_calls("\"container\", \"start\""), 1);
    assert_eq!(fixture.docker_calls("\"container\", \"rm\""), 1);
    assert!(!fixture.config.join("state.json").exists());
}

#[test]
fn successful_command_records_real_execution_then_releases_its_dependent() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![
        fixture.command_step("command", &[], true),
        source_step("source", &["command"]),
    ]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "success-run", &plan).expect("start workflow");

    let completed =
        advance_with_executor(fixture.data_dir(), "success-run", 2, &fixture.executor())
            .expect("execute workflow");
    assert_eq!(completed.state, RunState::Succeeded);
    assert_eq!(completed.steps[0].state, StepState::Succeeded);
    assert_eq!(completed.steps[1].state, StepState::Succeeded);
    let execution = completed.steps[0]
        .execution
        .as_ref()
        .expect("execution view");
    assert_eq!(execution.state, "finished");
    assert_eq!(execution.outcome.as_deref(), Some("success"));
    assert_eq!(execution.generation, 1);
    assert!(execution.record_digest.is_some());
    assert_eq!(
        execution.attempt_id,
        fixture.attempt_id("success-run", plan.digest(), "command")
    );
    assert_eq!(
        execution.attempt_id,
        command_attempt_id("success-run", plan.digest(), "command")
    );
    assert_ne!(
        execution.attempt_id,
        command_attempt_id("success-run", plan.digest(), "source")
    );
    assert_eq!(fixture.docker_calls("\"container\", \"start\""), 1);
}

#[test]
fn nonzero_command_fails_and_blocks_its_dependent() {
    let fixture = Fixture::fake();
    fixture.scenario("nonzero");
    let definition = fixture.definition(vec![
        fixture.command_step("command", &[], true),
        source_step("source", &["command"]),
    ]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "failed-run", &plan).expect("start workflow");

    let failed = advance_with_executor(fixture.data_dir(), "failed-run", 2, &fixture.executor())
        .expect("record failed command");
    assert_eq!(failed.state, RunState::Failed);
    assert_eq!(failed.steps[0].state, StepState::Failed);
    assert_eq!(failed.steps[1].state, StepState::Blocked);
    assert_eq!(
        failed.steps[0]
            .execution
            .as_ref()
            .and_then(|execution| execution.outcome.as_deref()),
        Some("failed")
    );
}

#[test]
fn lost_result_survives_reopen_without_replay_and_recovers_incomplete() {
    let fixture = Fixture::fake();
    fixture.scenario("lost_remove_reply");
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "lost-run", &plan).expect("start workflow");
    assert!(advance_with_executor(fixture.data_dir(), "lost-run", 1, &fixture.executor()).is_err());
    assert_eq!(fixture.docker_calls("\"container\", \"start\""), 1);

    let reopened = status(fixture.data_dir(), "lost-run").expect("reopen workflow");
    assert_eq!(reopened.steps[0].state, StepState::Running);
    assert_eq!(
        reopened.steps[0]
            .execution
            .as_ref()
            .map(|execution| execution.state.as_str()),
        Some("running")
    );
    fixture.scenario("success");
    assert!(advance_with_executor(fixture.data_dir(), "lost-run", 1, &fixture.executor()).is_err());
    assert_eq!(fixture.docker_calls("\"container\", \"start\""), 1);

    let recovered = recover_command(
        fixture.data_dir(),
        "lost-run",
        "command",
        &fixture.executor(),
    )
    .expect("recover lost result");
    assert_eq!(recovered.state, RunState::Blocked);
    assert_eq!(recovered.steps[0].state, StepState::Blocked);
    assert_eq!(
        recovered.steps[0]
            .execution
            .as_ref()
            .and_then(|execution| execution.outcome.as_deref()),
        Some("incomplete")
    );
}

#[test]
fn a_preexisting_running_attempt_is_never_automatically_rerun() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "running-run", &plan).expect("start workflow");
    let attempt_id = fixture.create_attempt("running-run", plan.digest(), "command");
    fixture
        .raw_authority()
        .execute(
            "UPDATE command_attempts SET state='running',run_generation=1 WHERE attempt_id=?1",
            [&attempt_id],
        )
        .expect("simulate controller crash after claim");
    fixture.mark_workflow_running("running-run", "command");

    assert!(
        advance_with_executor(fixture.data_dir(), "running-run", 1, &fixture.executor()).is_err()
    );
    assert_eq!(fixture.docker_calls("\"container\", \"create\""), 0);
    let attempt = fixture
        .authority()
        .command_attempt(&attempt_id)
        .expect("retained running attempt");
    assert_eq!(attempt.state, CommandState::Running);
    assert_eq!(attempt.run_generation, 1);
}

#[test]
fn cancelling_pending_work_cancels_its_durable_attempt() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "pending-cancel", &plan).expect("start workflow");
    let attempt_id = fixture.create_attempt("pending-cancel", plan.digest(), "command");

    let cancelled = cancel_with_executor(fixture.data_dir(), "pending-cancel", &fixture.executor())
        .expect("cancel workflow");
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(cancelled.steps[0].state, StepState::Cancelled);
    let execution = cancelled.steps[0]
        .execution
        .as_ref()
        .expect("cancel record");
    assert_eq!(execution.attempt_id, attempt_id);
    assert_eq!(execution.outcome.as_deref(), Some("cancelled"));
    assert_eq!(fixture.docker_calls("\"container\", \"create\""), 0);
}

#[test]
fn executor_cancellation_before_claim_starts_no_container() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "token-cancel", &plan).expect("start workflow");
    let cancellation = Cancellation::default();
    cancellation.cancel();

    let cancelled = advance_with_executor(
        fixture.data_dir(),
        "token-cancel",
        1,
        &fixture.executor_with(&cancellation),
    )
    .expect("record cancellation");
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(cancelled.steps[0].state, StepState::Cancelled);
    assert_eq!(
        cancelled.steps[0]
            .execution
            .as_ref()
            .and_then(|execution| execution.outcome.as_deref()),
        Some("cancelled")
    );
    assert_eq!(fixture.docker_calls("\"container\", \"create\""), 0);
}

#[test]
fn recovery_preserves_unknown_cleanup_custody_and_never_completes_the_step() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "unknown-cleanup", &plan).expect("start workflow");
    let attempt_id = fixture.create_attempt("unknown-cleanup", plan.digest(), "command");
    let attempt = fixture
        .authority()
        .command_attempt(&attempt_id)
        .expect("pending attempt");
    let request = ExecutionRequest {
        attempt_id: attempt_id.clone(),
        subject: attempt.subject,
        run_generation: 1,
        recipe: fixture.recipe.clone(),
    };
    let nonce = "c".repeat(64);
    let custody = CleanupCustody {
        run_generation: 1,
        handle: CleanupHandle {
            container_name: format!("agentscm-{nonce}"),
            owner_label: format!("{}:{nonce}", request.digest().expect("request digest")),
            creation_confirmed: false,
            runtime_identity: fixture.backend.probe().identity.expect("runtime identity"),
            container_id: None,
        },
    };
    fixture
        .raw_authority()
        .execute(
            "UPDATE command_attempts SET state='running',run_generation=1,custody_json=?1 \
             WHERE attempt_id=?2",
            params![
                serde_json::to_vec(&custody).expect("custody JSON"),
                attempt_id
            ],
        )
        .expect("simulate uncertain create");
    fixture.mark_workflow_running("unknown-cleanup", "command");

    assert!(
        advance_with_executor(
            fixture.data_dir(),
            "unknown-cleanup",
            1,
            &fixture.executor()
        )
        .is_err()
    );
    assert!(
        recover_command(
            fixture.data_dir(),
            "unknown-cleanup",
            "command",
            &fixture.executor()
        )
        .is_err()
    );
    let retained = fixture
        .authority()
        .command_attempt(&attempt_id)
        .expect("retained custody");
    assert_eq!(retained.state, CommandState::Running);
    assert_eq!(retained.run_generation, 2);
    assert!(retained.custody.is_some());
    assert_eq!(
        status(fixture.data_dir(), "unknown-cleanup")
            .expect("workflow status")
            .steps[0]
            .state,
        StepState::Running
    );
}

#[test]
fn source_or_context_mismatch_is_rejected_before_a_workflow_can_start() {
    let fixture = Fixture::fake();
    let mut bad_context = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let StepAction::Command { context_digest, .. } = &mut bad_context.steps[0].action else {
        unreachable!()
    };
    *context_digest = "f".repeat(64);
    assert!(compile(fixture.data_dir(), &bad_context).is_err());

    let engine = Engine::open(fixture.data_dir()).expect("native engine");
    let next_revision = engine
        .checkpoint(
            Some(&fixture.revision),
            &[put(SOURCE_PATH, b"pub fn answer() -> u8 { 7 }\n")],
        )
        .expect("changed exact source");
    drop(engine);
    let mut wrong_source = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    wrong_source.source.revision = next_revision;
    assert!(compile(fixture.data_dir(), &wrong_source).is_err());
    assert_eq!(fixture.docker_calls("\"container\", \"create\""), 0);
}

#[test]
fn current_context_drift_after_start_prevents_execution() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "context-drift", &plan).expect("start workflow");
    let mut next = make_context(&fixture.repository, 2, &fixture.revision, &fixture.recipe);
    next.change = fixture.context.change.clone();
    fixture
        .authority()
        .update_change_context(1, &next)
        .expect("advance current context");

    assert!(
        advance_with_executor(fixture.data_dir(), "context-drift", 1, &fixture.executor()).is_err()
    );
    let attempt_id = fixture.attempt_id("context-drift", plan.digest(), "command");
    assert!(matches!(
        fixture.authority().command_attempt(&attempt_id),
        Err(Error::NotFound(_))
    ));
    assert_eq!(fixture.docker_calls("\"container\", \"create\""), 0);
}

#[test]
fn policy_drift_prevents_attempt_creation_and_execution() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "policy-drift", &plan).expect("start workflow");
    fixture
        .authority()
        .advance_policy(0)
        .expect("advance policy epoch");

    assert!(
        advance_with_executor(fixture.data_dir(), "policy-drift", 1, &fixture.executor()).is_err()
    );
    let attempt_id = fixture.attempt_id("policy-drift", plan.digest(), "command");
    assert!(matches!(
        fixture.authority().command_attempt(&attempt_id),
        Err(Error::NotFound(_))
    ));
    assert_eq!(fixture.docker_calls("\"container\", \"create\""), 0);
}

#[test]
fn policy_revoked_between_workflow_compile_and_command_claim_starts_nothing() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).unwrap();
    start(fixture.data_dir(), "claim-race", &plan).unwrap();
    let id = fixture.create_attempt("claim-race", plan.digest(), "command");
    fixture.authority().advance_policy(0).unwrap();
    // Exercise the claim directly: another workflow compilation must not be
    // the fence protecting the gap after a successful compilation.
    let result = fixture.authority().run_command_attempt(
        &Engine::open(fixture.data_dir()).unwrap(),
        &fixture.backend,
        &fixture.work,
        &id,
        0,
        &fixture.cancellation,
    );
    assert!(
        result
            .expect_err("revoked policy must prevent the claim")
            .to_string()
            .contains("policy epoch")
    );
    assert_eq!(
        fixture.authority().command_attempt(&id).unwrap().state,
        CommandState::Pending
    );
    assert_eq!(fixture.docker_calls("\"container\", \"create\""), 0);
}

#[test]
fn crash_before_custody_requires_the_executor_to_clean_its_snapshot() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).unwrap();
    start(fixture.data_dir(), "snapshot-crash", &plan).unwrap();
    let id = fixture.create_attempt("snapshot-crash", plan.digest(), "command");
    fixture
        .raw_authority()
        .execute(
            "UPDATE command_attempts SET state='running',run_generation=1 WHERE attempt_id=?1",
            [&id],
        )
        .unwrap();
    fixture.mark_workflow_running("snapshot-crash", "command");
    let snapshot = fixture.work.join(format!("snapshot-{id}-1"));
    fs::create_dir(&snapshot).unwrap();
    fs::write(snapshot.join("source.txt"), b"retained source").unwrap();
    assert!(agentscm::skill_workflow::cancel(fixture.data_dir(), "snapshot-crash").is_err());
    assert!(snapshot.exists());
    let cancelled =
        cancel_with_executor(fixture.data_dir(), "snapshot-crash", &fixture.executor()).unwrap();
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert!(!snapshot.exists());
}

#[test]
fn historical_success_rejects_a_record_from_another_execution_generation() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).unwrap();
    start(fixture.data_dir(), "generation-binding", &plan).unwrap();
    let completed = advance_with_executor(
        fixture.data_dir(),
        "generation-binding",
        1,
        &fixture.executor(),
    )
    .unwrap();
    assert_eq!(completed.state, RunState::Succeeded);
    let id = command_attempt_id("generation-binding", plan.digest(), "command");
    fixture
        .raw_authority()
        .execute(
            "UPDATE command_attempts SET run_generation=2 WHERE attempt_id=?1",
            [&id],
        )
        .unwrap();
    assert!(status(fixture.data_dir(), "generation-binding").is_err());
}

#[test]
fn legacy_advance_without_a_runtime_does_not_claim_or_start_command_work() {
    let fixture = Fixture::fake();
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "no-runtime", &plan).expect("start workflow");

    assert!(advance(fixture.data_dir(), "no-runtime", 1).is_err());
    let retained = status(fixture.data_dir(), "no-runtime").expect("workflow status");
    assert_eq!(retained.state, RunState::Running);
    assert_eq!(retained.steps[0].state, StepState::Pending);
    let attempt_id = fixture.attempt_id("no-runtime", plan.digest(), "command");
    assert!(matches!(
        fixture.authority().command_attempt(&attempt_id),
        Err(Error::NotFound(_))
    ));
    assert_eq!(fixture.docker_calls("\"container\", \"create\""), 0);
}

#[test]
#[ignore = "requires explicitly selected local Docker executable, socket and immutable image"]
fn live_runtime_executes_the_exact_native_revision() {
    let docker = required_absolute_env("AGENTSCM_DOCKER");
    let socket = required_absolute_env("AGENTSCM_DOCKER_SOCKET");
    let image = std::env::var("AGENTSCM_OCI_IMAGE")
        .expect("set AGENTSCM_OCI_IMAGE to sha256:<64 lowercase hex>");
    let mut selected_recipe = recipe();
    selected_recipe.image = image;
    selected_recipe.argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "set -eu; test \"$(/bin/busybox cat /source/src/example.rs)\" = 'pub fn answer() -> u8 { 42 }'; ! /bin/busybox touch /source/mutation".into(),
    ];
    selected_recipe.environment = BTreeMap::from([(
        "PATH".into(),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
    )]);
    selected_recipe.environment_sha256 =
        environment_digest(&selected_recipe.environment).expect("environment digest");
    let fixture = Fixture::new(selected_recipe, Some((docker, socket)));
    let definition = fixture.definition(vec![fixture.command_step("command", &[], true)]);
    let plan = compile(fixture.data_dir(), &definition).expect("compiled plan");
    start(fixture.data_dir(), "live-run", &plan).expect("start workflow");

    let completed = advance_with_executor(fixture.data_dir(), "live-run", 1, &fixture.executor())
        .expect("execute live workflow");
    assert_eq!(completed.state, RunState::Succeeded);
    assert_eq!(
        completed.steps[0]
            .execution
            .as_ref()
            .and_then(|execution| execution.outcome.as_deref()),
        Some("success")
    );
}

fn put(path: &str, bytes: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(bytes.to_vec()),
    }
}

fn source_step(id: &str, needs: &[&str]) -> StepDefinition {
    StepDefinition {
        id: id.into(),
        needs: needs.iter().map(|value| (*value).into()).collect(),
        required: true,
        action: StepAction::SourcePaths {
            expectations: vec![SourcePathExpectation {
                path: SOURCE_PATH.into(),
                sha256: digest(SOURCE_BYTES),
            }],
        },
    }
}

fn recipe() -> CommandRecipe {
    let environment = BTreeMap::new();
    CommandRecipe {
        version: 1,
        image: format!("sha256:{IMAGE}"),
        argv: vec!["/bin/true".into()],
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
) -> ReviewContext {
    let digest_identity = |sha256: &str| InputIdentity::Digest {
        sha256: sha256.into(),
    };
    let recipe_digest = recipe.digest().expect("recipe digest");
    ReviewContext {
        version: 1,
        change: ChangeId {
            repository: repository.into(),
            id: "workflow-command".into(),
        },
        change_version: version,
        revision: revision.into(),
        base: None,
        target: "main".into(),
        policy_epoch: 0,
        intent: "run the packaged command against the exact native revision".into(),
        acceptance_criteria: vec!["the isolated command exits zero".into()],
        scope: vec![SOURCE_PATH.into()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: digest_identity(&recipe_digest),
        toolchain: digest_identity(&recipe.toolchain_sha256),
        environment: digest_identity(&recipe.environment_sha256),
        required_evidence: vec![RequiredEvidence {
            identity: COMMAND_EVIDENCE.into(),
            digest: digest_identity(&recipe_digest),
        }],
        required_capabilities: vec![
            "agentscm.review-context/v1".into(),
            COMMAND_CAPABILITY.into(),
        ],
    }
}

fn private(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private directory");
}

fn required_absolute_env(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")));
    assert!(path.is_absolute(), "{name} must be absolute");
    path
}
