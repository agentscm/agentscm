use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentscm::authority::Authority;
use agentscm::authority::workflows::WorkflowRequirement;
use agentscm::context::{ChangeId, InputIdentity, RequiredEvidence, ReviewContext};
use agentscm::execution::{
    COMMAND_CAPABILITY, COMMAND_EVIDENCE, Cancellation, CommandRecipe, ExecutionLimits, OciBackend,
    SyscallPolicy, environment_digest,
};
use agentscm::model::FileChange;
use agentscm::reviewer::ReviewCancellation;
use agentscm::skill_workflow::runtime::{
    ClaudeExecutor, CommandExecutor, advance_with_backends, finalize,
};
use agentscm::skill_workflow::{
    RunState, SkillReference, StepAction, StepDefinition, StepState, WorkflowDefinition,
    WorkflowSource, advance, cancel, compile, start, status,
};
use agentscm::store::Engine;
use rusqlite::Connection;
use tempfile::TempDir;

use super::fake_docker::FAKE_DOCKER;

const SKILL_PATH: &str = "skills/cancellation/SKILL.md";
const RUNNER_PATH: &str = "skills/cancellation/runner.json";
const SOURCE_PATH: &str = "src/cancellation.rs";
const SOURCE_BYTES: &[u8] = b"pub fn cancellation_is_durable() -> bool { true }\n";
const IMAGE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const TOOLCHAIN: &str = "2222222222222222222222222222222222222222222222222222222222222222";

struct CancellationFixture {
    root: TempDir,
    _socket: UnixListener,
    backend: OciBackend,
    work: PathBuf,
    context: ReviewContext,
    requirement: WorkflowRequirement,
}

impl CancellationFixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        private(root.path());
        let config = root.path().join("docker-config");
        fs::create_dir(&config).unwrap();
        private(&config);
        let work = root.path().join("execution-work");
        fs::create_dir(&work).unwrap();
        private(&work);
        let docker = root.path().join("fake-docker.py");
        fs::write(&docker, FAKE_DOCKER).unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
        let socket_path = root.path().join("docker.sock");
        let socket = UnixListener::bind(&socket_path).unwrap();
        let backend = OciBackend::new(&docker, &socket_path, &config).unwrap();

        let recipe = recipe();
        let mut authority = Authority::open(root.path()).unwrap();
        let repository = authority.repository_id().unwrap();
        let engine = Engine::open(root.path()).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[
                    put(SKILL_PATH, b"# Review cancellation\n"),
                    put(RUNNER_PATH, &serde_json::to_vec(&recipe).unwrap()),
                    put(SOURCE_PATH, SOURCE_BYTES),
                ],
            )
            .unwrap();
        let mut context = context(&repository, &revision, &recipe);
        authority.start_change(&context).unwrap();
        drop(authority);
        drop(engine);

        let definition = definition(&context, &revision, 0);
        let plan = compile(root.path(), &definition).unwrap();
        let requirement = WorkflowRequirement::from_plan("main", &plan).unwrap();
        let mut authority = Authority::open(root.path()).unwrap();
        authority
            .install_workflow_requirement(&requirement)
            .unwrap();
        context.change_version += 1;
        context.policy_epoch = authority.policy_epoch().unwrap();
        authority.update_change_context(1, &context).unwrap();

        Self {
            root,
            _socket: socket,
            backend,
            work,
            context,
            requirement,
        }
    }

    fn data_dir(&self) -> &Path {
        self.root.path()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_controller_stops_active_claude_and_cannot_publish_or_replay_it() {
    let fixture = CancellationFixture::new();
    let definition = fixture.requirement.instantiate(&fixture.context).unwrap();
    let plan = compile(fixture.data_dir(), &definition).unwrap();
    start(fixture.data_dir(), "cancel-review", &plan).unwrap();
    let command_cancellation = Cancellation::default();
    let command = CommandExecutor {
        backend: &fixture.backend,
        work_dir: &fixture.work,
        cancellation: &command_cancellation,
    };
    let after_command =
        advance_with_backends(fixture.data_dir(), "cancel-review", 1, Some(&command), None)
            .unwrap();
    assert_eq!(after_command.steps[0].state, StepState::Succeeded);
    assert_eq!(after_command.steps[1].state, StepState::Pending);

    let started = fixture.root.path().join("claude-started");
    let calls = fixture.root.path().join("claude-calls");
    let executable = sleeping_claude(fixture.root.path(), &started, &calls);
    let cwd = fixture.root.path().join("claude-work");
    fs::create_dir(&cwd).unwrap();
    private(&cwd);
    let data_dir = fixture.data_dir().to_path_buf();
    let task_data_dir = data_dir.clone();
    let task_executable = executable.clone();
    let task_cwd = cwd.clone();
    let mut active = tokio::task::spawn_blocking(move || {
        let cancellation = ReviewCancellation::default();
        let claude = ClaudeExecutor {
            executable: &task_executable,
            work_dir: &task_cwd,
            cancellation: &cancellation,
        };
        advance_with_backends(&task_data_dir, "cancel-review", 1, None, Some(&claude))
    });

    let start_deadline = Instant::now() + Duration::from_secs(5);
    while !started.exists() {
        assert!(!active.is_finished(), "review exited before Claude started");
        assert!(
            Instant::now() < start_deadline,
            "timed out waiting for fake Claude"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let cancelled_at = Instant::now();
    let cancelled = cancel(&data_dir, "cancel-review").unwrap();
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(cancelled.steps[1].state, StepState::Cancelled);
    // Half the fake Claude's 30 s sleep: cancellation, not the sleep ending, stopped it.
    let completed = match tokio::time::timeout(Duration::from_secs(15), &mut active).await {
        Ok(result) => result.unwrap().unwrap(),
        Err(_) => {
            let pid: i32 = fs::read_to_string(&started)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut active).await;
            panic!("active Claude process did not stop promptly after durable cancellation");
        }
    };
    assert!(cancelled_at.elapsed() < Duration::from_secs(15));
    assert_eq!(completed.state, RunState::Cancelled);
    assert_eq!(completed.steps[1].state, StepState::Cancelled);
    let review = completed.steps[1].review.as_ref().unwrap();
    assert_eq!(review.status, "cancelled");
    assert_eq!(review.stop.as_deref(), Some("cancelled"));
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 1);

    assert!(finalize(&data_dir, "cancel-review", &fixture.context).is_err());
    let replayed = advance(&data_dir, "cancel-review", 32).unwrap();
    assert_eq!(replayed.state, RunState::Cancelled);
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 1);
    let retained = status(&data_dir, "cancel-review").unwrap();
    assert_eq!(
        retained.steps[1].review.as_ref().unwrap().status,
        "cancelled"
    );
    let authority = Connection::open(data_dir.join("state.sqlite")).unwrap();
    let receipts: i64 = authority
        .query_row("SELECT count(*) FROM workflow_receipts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(receipts, 0);
}

fn definition(context: &ReviewContext, revision: &str, policy_epoch: u64) -> WorkflowDefinition {
    WorkflowDefinition {
        version: 1,
        name: "durable-claude-cancellation".into(),
        source: WorkflowSource {
            repository: context.change.repository.clone(),
            revision: revision.into(),
            policy_epoch,
        },
        skill: SkillReference {
            entrypoint: SKILL_PATH.into(),
            resources: vec![RUNNER_PATH.into()],
        },
        steps: vec![
            StepDefinition {
                id: "tests".into(),
                needs: vec![],
                required: true,
                action: StepAction::Command {
                    runner: "runner.json".into(),
                    change: context.change.clone(),
                    change_version: context.change_version,
                    context_digest: context.digest().unwrap(),
                },
            },
            StepDefinition {
                id: "review".into(),
                needs: vec!["tests".into()],
                required: true,
                action: StepAction::ClaudeCodeReview {
                    paths: vec![SOURCE_PATH.into()],
                    model: "sonnet".into(),
                },
            },
        ],
    }
}

fn recipe() -> CommandRecipe {
    let environment = BTreeMap::new();
    CommandRecipe {
        version: 1,
        image: format!("sha256:{IMAGE}"),
        argv: vec!["/bin/true".into()],
        environment_sha256: environment_digest(&environment).unwrap(),
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

fn context(repository: &str, revision: &str, recipe: &CommandRecipe) -> ReviewContext {
    let identity = |sha256: &str| InputIdentity::Digest {
        sha256: sha256.into(),
    };
    let recipe_digest = recipe.digest().unwrap();
    ReviewContext {
        version: 1,
        change: ChangeId {
            repository: repository.into(),
            id: "durable-cancellation".into(),
        },
        change_version: 1,
        revision: revision.into(),
        base: None,
        target: "main".into(),
        policy_epoch: 0,
        intent: "stop an active review from another controller".into(),
        acceptance_criteria: vec!["cancelled reviews cannot authorize landing".into()],
        scope: vec![SOURCE_PATH.into()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: identity(&recipe_digest),
        toolchain: identity(&recipe.toolchain_sha256),
        environment: identity(&recipe.environment_sha256),
        required_evidence: vec![RequiredEvidence {
            identity: COMMAND_EVIDENCE.into(),
            digest: identity(&recipe_digest),
        }],
        required_capabilities: vec![
            "agentscm.review-context/v1".into(),
            COMMAND_CAPABILITY.into(),
        ],
    }
}

fn sleeping_claude(root: &Path, started: &Path, calls: &Path) -> PathBuf {
    let executable = root.join("sleeping-claude.py");
    let started = serde_json::to_string(started.to_str().unwrap()).unwrap();
    let calls = serde_json::to_string(calls.to_str().unwrap()).unwrap();
    let script = format!(
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
if sys.argv[1:] == ['--version']:
 print('2.1.300 (Claude Code)'); sys.exit(0)
if sys.argv[1:] == ['auth','status']:
 print(json.dumps({{'loggedIn':True,'authMethod':'claude.ai'}})); sys.exit(0)
pathlib.Path({calls}).open('a').write('started\n')
pathlib.Path({started}).write_text(str(os.getpid()))
time.sleep(30)
"#
    );
    fs::write(&executable, script).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

fn put(path: &str, bytes: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(bytes.to_vec()),
    }
}

fn private(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
