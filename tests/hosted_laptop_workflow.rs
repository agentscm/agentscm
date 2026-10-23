#![cfg(unix)]
//! Two source-only laptop stores against the real local Hub HTTP/auth/queue path.
//! The workflow uses only built-in source checks; no Docker or provider runs.
#[path = "common/workspace_hub.rs"]
mod workspace_hub;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;

use agentscm::authority::Authority;
use agentscm::hub::store::{mint_agent_token, mint_bound_agent_token};
use agentscm::hub::workspace_execution::source::{
    ContextDeclaration, SOURCE_TRANSFER_VERSION, SourceBootstrap, SourceProfile, SourceTransfer,
    SourceTransferRequest, SourceTransferState,
};
use agentscm::hub::workspace_execution::{
    ExecutionConfig, ExecutionService, ExecutionState, WorkspaceRun,
};
use agentscm::store::Engine;
use agentscm::sync;
use agentscm::team::auth::Role;
use agentscm::workspace::Workspace;
use reqwest::{Method, StatusCode};
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;
use workspace_hub::{Env, NativeSourceProfile, native_source_profile};

const REPOSITORY: &str = "synthetic/project";
const REPOSITORY_PATH: &str = "synthetic%2Fproject";
const PROFILE: &str = "checks";
const SOURCE_PATH: &str = "src/example.rs";
const ORIGINAL_SOURCE: &[u8] = b"fn example() {}\n";
static ACCEPTANCE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn hosted_env(root: &Path, native: &NativeSourceProfile) -> (Env, Arc<ExecutionService>) {
    let profile = native.profile.clone();
    let source = SourceProfile {
        repository_id: REPOSITORY.into(),
        profile_id: PROFILE.into(),
        template_context_digest: native.context.digest().unwrap(),
    };
    let queue = root.join("execution.sqlite");
    let installed = Arc::new(std::sync::OnceLock::new());
    let retain = installed.clone();
    let env = Env::with_state(move |state, workspace| {
        let service = Arc::new(
            ExecutionService::open(
                &queue,
                ExecutionConfig {
                    attached_publication_profiles: vec![],
                    version: 1,
                    profiles: vec![profile],
                    source_profiles: vec![source],
                    agent_task_profiles: vec![],
                },
                workspace,
            )
            .unwrap(),
        );
        retain.set(service.clone()).ok().unwrap();
        state.with_workspace_execution(service)
    })
    .await;
    (env, installed.get().unwrap().clone())
}

async fn agent_token(env: &Env, synthetic_subject: i64, name: &str) -> (String, String) {
    let mut user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == synthetic_subject);
    if user.is_none() {
        let _ = env.signin(synthetic_subject).await;
        user = env
            .store
            .list_users()
            .unwrap()
            .into_iter()
            .find(|user| user.identity.user_id == synthetic_subject);
    }
    let user = user.unwrap();
    let (id, secret) = mint_agent_token(&*env.store, &user.id, name, REPOSITORY, 3600).unwrap();
    (id, secret.expose().to_owned())
}

fn token_file(root: &Path, name: &str, token: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, token).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn args(
    env: &Env,
    token: PathBuf,
    data: PathBuf,
    workspace: PathBuf,
    change: &str,
) -> (PathBuf, Laptop) {
    (
        data,
        Laptop {
            endpoint: env.base.clone(),
            repository: REPOSITORY.into(),
            profile: PROFILE.into(),
            token_file: token,
            workspace,
            change: change.into(),
            poll_attempts: 60,
            poll_interval_ms: 10,
        },
    )
}

struct Laptop {
    endpoint: String,
    repository: String,
    profile: String,
    token_file: PathBuf,
    workspace: PathBuf,
    change: String,
    poll_attempts: u16,
    poll_interval_ms: u64,
}

struct CliChild(Option<Child>);

impl CliChild {
    fn get_mut(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }

    fn wait_with_output(mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }

    fn kill_with_output(mut self) -> Output {
        let mut child = self.0.take().unwrap();
        let _ = child.kill();
        child.wait_with_output().unwrap()
    }
}

impl Drop for CliChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn run_cli(data: &Path, laptop: &Laptop, command: &str) -> Value {
    run_cli_input(data, laptop, command, None)
}

fn cli_command(data: &Path, laptop: &Laptop, command: &str) -> Command {
    let mut process = Command::new(env!("CARGO_BIN_EXE_agentscm"));
    process
        .arg("--data-dir")
        .arg(data)
        .args(["skill-workflow", command])
        .arg("--endpoint")
        .arg(&laptop.endpoint)
        .arg("--repository")
        .arg(&laptop.repository)
        .arg("--profile")
        .arg(&laptop.profile)
        .arg("--token-file")
        .arg(&laptop.token_file)
        .arg("--workspace")
        .arg(&laptop.workspace)
        .arg("--change")
        .arg(&laptop.change)
        .arg("--poll-attempts")
        .arg(laptop.poll_attempts.to_string())
        .arg("--poll-interval-ms")
        .arg(laptop.poll_interval_ms.to_string());
    process
}

fn cli_output(data: &Path, laptop: &Laptop, command: &str, input: Option<&[u8]>) -> Output {
    let mut child = cli_command(data, laptop, command)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn run_cli_input(data: &Path, laptop: &Laptop, command: &str, input: Option<&[u8]>) -> Value {
    let output = cli_output(data, laptop, command, input);
    assert!(
        output.status.success(),
        "{command} for {} failed\nstdout: {}\nstderr: {}",
        laptop.change,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_cli_failure(data: &Path, laptop: &Laptop, command: &str) -> String {
    let output = cli_output(data, laptop, command, None);
    assert!(output.status.code().is_some_and(|code| code != 0));
    String::from_utf8(output.stderr).unwrap()
}

fn run_cli_blocked(data: &Path, laptop: &Laptop, command: &str) -> Value {
    let output = cli_output(data, laptop, command, None);
    assert!(output.status.code().is_some_and(|code| code != 0));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn queue_counts(root: &Path) -> (i64, i64) {
    let connection = Connection::open(root.join("execution.sqlite")).unwrap();
    let transfers = connection
        .query_row(
            "SELECT count(*) FROM workspace_source_transfers",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let runs = connection
        .query_row("SELECT count(*) FROM workspace_runs", [], |row| row.get(0))
        .unwrap();
    (transfers, runs)
}

fn journal_path(data: &Path) -> PathBuf {
    let mut journals = fs::read_dir(data.join("hosted-workflow"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|value| value == "json"))
        .collect::<Vec<_>>();
    assert_eq!(journals.len(), 1, "expected one hosted workflow journal");
    journals.pop().unwrap()
}

fn authority_counts(native: &NativeSourceProfile) -> (i64, i64) {
    let connection = Connection::open(native.profile.data_dir.join("state.sqlite")).unwrap();
    let count = |table: &str| {
        connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    (count("approvals"), count("integration_events"))
}

async fn reviewer(env: &Env) -> (String, String, Value) {
    let (_, owner) = env.signin(101).await;
    let owner_session = env.session(&owner).await;
    let grant = env.member(
        202,
        &[
            Role::Reader,
            Role::Contributor,
            Role::Reviewer,
            Role::Integrator,
        ],
        true,
        env.workspace.settings().unwrap().generation,
        "hosted-reviewer",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            owner_session["csrf_token"].as_str(),
            None,
            Some(grant),
        )
        .await
        .0,
        200
    );
    let (_, reviewer) = env.signin(202).await;
    let session = env.session(&reviewer).await;
    (
        reviewer,
        session["csrf_token"].as_str().unwrap().to_owned(),
        session["actor"]["generation"].clone(),
    )
}

async fn land(
    env: &Env,
    reviewer: &str,
    csrf: &str,
    generation: &Value,
    result: &Value,
    operation: &str,
) {
    let subject = result["context_subject"].clone();
    let approve =
        format!("/v1/hub/workspace/repositories/{REPOSITORY_PATH}/profiles/{PROFILE}/approve");
    let approval = env
        .request(
            Method::POST,
            &approve,
            Some(reviewer),
            Some(csrf),
            None,
            Some(serde_json::json!({
                "request_id":format!("approve-{operation}"),
                "expected_generation":generation,
                "subject":subject,
            })),
        )
        .await;
    assert_eq!(approval.0, 200, "{approval:?}");
    let integrate =
        format!("/v1/hub/workspace/repositories/{REPOSITORY_PATH}/profiles/{PROFILE}/integrate");
    let integrated = env
        .request(
            Method::POST,
            &integrate,
            Some(reviewer),
            Some(csrf),
            None,
            Some(serde_json::json!({
                "request_id":format!("integrate-{operation}"),
                "expected_generation":generation,
                "subject":result["context_subject"],
                "approval_id":approval.1["approval_id"],
            })),
        )
        .await;
    assert_eq!(integrated.0, 200, "{integrated:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_laptops_get_failure_fix_feedback_and_exact_success_without_native_authority() {
    let _serial = ACCEPTANCE_LOCK.lock().await;
    let root = TempDir::new().unwrap();
    let native = native_source_profile(root.path());
    let (env, service) = hosted_env(root.path(), &native).await;
    let worker = service.spawn_worker();
    let (reviewer, csrf, generation) = reviewer(&env).await;
    let (_, secret_a) = agent_token(&env, 101, "hosted-laptop-a").await;
    let (_, secret_b) = agent_token(&env, 202, "hosted-laptop-b").await;
    let token_a = token_file(root.path(), "agent-a.token", &secret_a);
    let token_b = token_file(root.path(), "agent-b.token", &secret_b);
    let (data_a, laptop_a) = args(
        &env,
        token_a,
        root.path().join("laptop-a-data"),
        root.path().join("laptop-a-work"),
        "laptop-a",
    );
    let (data_b, laptop_b) = args(
        &env,
        token_b,
        root.path().join("laptop-b-data"),
        root.path().join("laptop-b-work"),
        "laptop-b",
    );

    let initialized_a = run_cli(&data_a, &laptop_a, "remote-init");
    let initialized_b = run_cli(&data_b, &laptop_b, "remote-init");
    assert_eq!(
        initialized_a["seed_revision"],
        initialized_b["seed_revision"]
    );
    assert!(!data_a.join("state.sqlite").exists());
    assert!(!data_b.join("state.sqlite").exists());

    let first = run_cli(&data_a, &laptop_a, "remote-check");
    let second = run_cli(&data_b, &laptop_b, "remote-check");
    for result in [&first, &second] {
        assert!(!result["receipt"].is_null(), "{result}");
        assert_eq!(result["workspace_changed_during_run"], false);
        assert_eq!(result["failures"], serde_json::json!([]));
        assert!(
            result["next_action"]
                .as_str()
                .unwrap()
                .contains("no approval or landing")
        );
    }
    // A token replacement must fail before touching A's existing source journal.
    let before_swap = queue_counts(root.path());
    fs::write(&laptop_a.token_file, &secret_b).unwrap();
    let swapped = run_cli_failure(&data_a, &laptop_a, "remote-check");
    assert!(
        swapped.contains("actor") || swapped.contains("person"),
        "{swapped}"
    );
    assert_eq!(queue_counts(root.path()), before_swap);
    fs::write(&laptop_a.token_file, &secret_a).unwrap();

    assert_ne!(first["context_subject"]["change_id"], "laptop-a");
    assert_ne!(second["context_subject"]["change_id"], "laptop-b");
    assert_ne!(
        first["context_subject"]["change_id"],
        second["context_subject"]["change_id"]
    );
    let retained = queue_counts(root.path());
    let repeated = run_cli(&data_a, &laptop_a, "remote-check");
    assert_eq!(repeated["receipt"], first["receipt"]);
    assert_eq!(repeated["run"], first["run"]);
    assert_eq!(queue_counts(root.path()), retained);
    let stopped = run_cli_input(
        &data_a,
        &laptop_a,
        "remote-stop",
        Some(br#"{"hook_event_name":"Stop","stop_hook_active":false}"#),
    );
    assert_eq!(stopped, serde_json::json!({}));
    assert_eq!(queue_counts(root.path()), retained);
    let codex_payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "synthetic-codex-session",
        "turn_id": "turn-1",
        "cwd": laptop_a.workspace,
        "stop_hook_active": false,
        "last_assistant_message": "done",
    })
    .to_string();
    let codex_stopped = run_cli_input(
        &data_a,
        &laptop_a,
        "remote-codex-stop",
        Some(codex_payload.as_bytes()),
    );
    assert_eq!(codex_stopped, serde_json::json!({}));
    assert_eq!(queue_counts(root.path()), retained);
    fs::create_dir(root.path().join("unrelated")).unwrap();
    let elsewhere = serde_json::json!({
        "hook_event_name": "Stop",
        "cwd": root.path().join("unrelated"),
        "stop_hook_active": false,
    })
    .to_string();
    assert_eq!(
        run_cli_input(
            &data_a,
            &laptop_a,
            "remote-codex-stop",
            Some(elsewhere.as_bytes())
        ),
        serde_json::json!({})
    );
    assert_eq!(queue_counts(root.path()), retained);

    // Nothing above approved or landed. A separate human actor now advances
    // the target so the two laptop workspaces exercise ordinary refresh/merge.
    assert_eq!(authority_counts(&native), (0, 0));
    land(&env, &reviewer, &csrf, &generation, &first, "seed").await;

    // Locally excluded files stay on laptop A and are not part of the transferred source.
    fs::write(
        laptop_a.workspace.join(".agentscm-workspace/exclude"),
        b".env\ntarget/\n",
    )
    .unwrap();
    fs::write(laptop_a.workspace.join(".env"), b"SECRET=synthetic\n").unwrap();
    fs::create_dir(laptop_a.workspace.join("target")).unwrap();
    fs::write(laptop_a.workspace.join("target/out"), b"build output\n").unwrap();
    fs::write(laptop_a.workspace.join("a.txt"), b"from laptop a\n").unwrap();
    let advanced_a = run_cli(&data_a, &laptop_a, "remote-check");
    assert!(!advanced_a["receipt"].is_null(), "{advanced_a}");
    land(&env, &reviewer, &csrf, &generation, &advanced_a, "laptop-a").await;

    fs::write(laptop_b.workspace.join("b.txt"), b"from laptop b\n").unwrap();
    let merged_b = run_cli(&data_b, &laptop_b, "remote-check");
    assert!(!merged_b["receipt"].is_null(), "{merged_b}");
    assert_eq!(
        fs::read(laptop_b.workspace.join("a.txt")).unwrap(),
        b"from laptop a\n"
    );
    assert_eq!(
        fs::read(laptop_b.workspace.join("b.txt")).unwrap(),
        b"from laptop b\n"
    );
    assert!(!laptop_b.workspace.join(".env").exists());
    assert!(!laptop_b.workspace.join("target").exists());
    assert_eq!(
        fs::read(laptop_a.workspace.join(".env")).unwrap(),
        b"SECRET=synthetic\n"
    );
    let merged_counts = queue_counts(root.path());
    let repeated_merge = run_cli(&data_b, &laptop_b, "remote-check");
    assert_eq!(repeated_merge["receipt"], merged_b["receipt"]);
    assert_eq!(repeated_merge["run"], merged_b["run"]);
    assert_eq!(queue_counts(root.path()), merged_counts);
    assert_eq!(
        run_cli_input(
            &data_b,
            &laptop_b,
            "remote-stop",
            Some(br#"{"hook_event_name":"Stop","stop_hook_active":false}"#),
        ),
        serde_json::json!({})
    );
    assert_eq!(queue_counts(root.path()), merged_counts);
    land(&env, &reviewer, &csrf, &generation, &merged_b, "laptop-b").await;

    fs::write(laptop_a.workspace.join("shared.txt"), b"target bytes\n").unwrap();
    let shared_target = run_cli(&data_a, &laptop_a, "remote-check");
    assert!(!shared_target["receipt"].is_null(), "{shared_target}");
    land(
        &env,
        &reviewer,
        &csrf,
        &generation,
        &shared_target,
        "shared-target",
    )
    .await;
    fs::write(laptop_b.workspace.join("shared.txt"), b"local bytes\n").unwrap();
    let before_conflict = fs::read(laptop_b.workspace.join("shared.txt")).unwrap();
    let conflict = run_cli_failure(&data_b, &laptop_b, "remote-check");
    assert!(conflict.contains("merge") || conflict.contains("conflict"));
    assert_eq!(
        fs::read(laptop_b.workspace.join("shared.txt")).unwrap(),
        before_conflict
    );

    // This produces a valid source bundle larger than the ordinary 64 KiB Hub
    // JSON limit. The source route admits it, then the pinned source check fails.
    fs::write(laptop_a.workspace.join(SOURCE_PATH), vec![b'x'; 96 * 1024]).unwrap();
    let failed = run_cli_blocked(&data_a, &laptop_a, "remote-check");
    assert!(failed["receipt"].is_null(), "{failed}");
    assert!(!failed["failures"].as_array().unwrap().is_empty());
    assert!(failed["next_action"].as_str().unwrap().contains("Fix"));
    let blocked_stop = run_cli_input(
        &data_a,
        &laptop_a,
        "remote-stop",
        Some(br#"{"hook_event_name":"Stop","stop_hook_active":false}"#),
    );
    assert_eq!(blocked_stop["decision"], "block");
    assert!(
        blocked_stop["reason"]
            .as_str()
            .unwrap()
            .contains("failures")
    );
    // The same failing state through Codex with cwd in the workspace proves the check ran
    // rather than being skipped as an unrelated directory.
    let inside = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "synthetic-codex-session",
        "cwd": laptop_a.workspace,
        "stop_hook_active": false,
    })
    .to_string();
    let codex_blocked = run_cli_input(
        &data_a,
        &laptop_a,
        "remote-codex-stop",
        Some(inside.as_bytes()),
    );
    assert_eq!(codex_blocked["decision"], "block", "{codex_blocked}");
    assert!(
        codex_blocked["reason"]
            .as_str()
            .unwrap()
            .contains("failures")
    );
    let retained: i64 = Connection::open(root.path().join("execution.sqlite"))
        .unwrap()
        .query_row(
            "SELECT max(body_bytes) FROM workspace_source_transfers",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(retained > 64 * 1024, "uploaded only {retained} bytes");

    fs::write(laptop_a.workspace.join(SOURCE_PATH), ORIGINAL_SOURCE).unwrap();
    let fixed = run_cli(&data_a, &laptop_a, "remote-check");
    assert!(!fixed["receipt"].is_null(), "{fixed}");
    assert_eq!(fixed["workspace_changed_during_run"], false);
    let (approvals, integrations) = authority_counts(&native);
    assert_eq!((approvals, integrations), (4, 4));
    assert!(
        Authority::open_read_only(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_some()
    );
    worker.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_blocks_when_workspace_changes_while_the_exact_hosted_run_succeeds() {
    let _serial = ACCEPTANCE_LOCK.lock().await;
    let root = TempDir::new().unwrap();
    let native = native_source_profile(root.path());
    let (env, service) = hosted_env(root.path(), &native).await;
    let (_, secret) = agent_token(&env, 101, "hosted-active-edit").await;
    let token = token_file(root.path(), "active-edit.token", &secret);
    let (data, mut laptop) = args(
        &env,
        token,
        root.path().join("active-edit-data"),
        root.path().join("active-edit-work"),
        "active-edit",
    );
    laptop.poll_interval_ms = 50;
    run_cli(&data, &laptop, "remote-init");

    let mut waiting = CliChild(Some(
        cli_command(&data, &laptop, "remote-stop")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    waiting
        .get_mut()
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"Stop","stop_hook_active":false}"#)
        .unwrap();

    let queue = root.path().join("execution.sqlite");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let count: i64 = Connection::open(&queue)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM workspace_runs WHERE state='queued'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if count == 1 {
            break;
        }
        if waiting.get_mut().try_wait().unwrap().is_some() {
            let output = waiting.wait_with_output();
            panic!(
                "Stop exited before durably queueing its hosted run\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        if std::time::Instant::now() >= deadline {
            let output = waiting.kill_with_output();
            panic!(
                "Stop did not durably queue its hosted run before the deadline\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let changed = laptop.workspace.join("during-run.txt");
    fs::write(&changed, b"newer local bytes\n").unwrap();
    let worker = service.spawn_worker();
    let waiting = tokio::task::spawn_blocking(move || waiting.wait_with_output());
    let output = waiting.await.unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stopped: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stopped["decision"], "block");
    assert!(
        stopped["reason"]
            .as_str()
            .unwrap()
            .contains("Workspace bytes changed")
    );
    assert_eq!(fs::read(changed).unwrap(), b"newer local bytes\n");

    let run_bytes: Vec<u8> = Connection::open(&queue)
        .unwrap()
        .query_row(
            "SELECT run_json FROM workspace_runs ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let run: WorkspaceRun = serde_json::from_slice(&run_bytes).unwrap();
    assert_eq!(run.state, ExecutionState::Succeeded);
    assert!(run.receipt.is_some());
    assert_eq!(authority_counts(&native), (0, 0));
    worker.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_that_outlives_polling_is_recovered_before_new_bytes_are_checked() {
    let _serial = ACCEPTANCE_LOCK.lock().await;
    let root = TempDir::new().unwrap();
    let native = native_source_profile(root.path());
    let (env, service) = hosted_env(root.path(), &native).await;
    let (_, secret) = agent_token(&env, 101, "hosted-poll-recovery").await;
    let token = token_file(root.path(), "poll-recovery.token", &secret);
    let (data, mut laptop) = args(
        &env,
        token,
        root.path().join("poll-recovery-data"),
        root.path().join("poll-recovery-work"),
        "poll-recovery",
    );
    run_cli(&data, &laptop, "remote-init");

    laptop.poll_attempts = 1;
    let queued = run_cli_blocked(&data, &laptop, "remote-check");
    assert!(queued["receipt"].is_null(), "{queued}");
    assert_eq!(queued["run"]["state"], "queued", "{queued}");

    let newer = laptop.workspace.join("after-poll.txt");
    fs::write(&newer, b"bytes written after the first poll\n").unwrap();
    laptop.poll_attempts = 60;
    laptop.poll_interval_ms = 50;
    let worker = service.spawn_worker();
    let recovered = run_cli(&data, &laptop, "remote-check");
    assert!(!recovered["receipt"].is_null(), "{recovered}");
    assert_eq!(recovered["workspace_changed_during_run"], false);
    assert_eq!(
        fs::read(newer).unwrap(),
        b"bytes written after the first poll\n"
    );

    let connection = Connection::open(root.path().join("execution.sqlite")).unwrap();
    let total: i64 = connection
        .query_row("SELECT count(*) FROM workspace_runs", [], |row| row.get(0))
        .unwrap();
    let succeeded: i64 = connection
        .query_row(
            "SELECT count(*) FROM workspace_runs WHERE state='succeeded'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((total, succeeded), (2, 2));
    assert_eq!(authority_counts(&native), (0, 0));
    worker.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retained_offer_is_uploaded_before_new_workspace_bytes_are_admitted() {
    let _serial = ACCEPTANCE_LOCK.lock().await;
    let root = TempDir::new().unwrap();
    let native = native_source_profile(root.path());
    let (env, service) = hosted_env(root.path(), &native).await;
    let (_, secret) = agent_token(&env, 101, "hosted-offer-recovery").await;
    let token = token_file(root.path(), "offer-recovery.token", &secret);
    let (data, laptop) = args(
        &env,
        token,
        root.path().join("offer-recovery-data"),
        root.path().join("offer-recovery-work"),
        "offer-recovery",
    );
    run_cli(&data, &laptop, "remote-init");

    let source = format!(
        "{}/v1/hub/workspace/repositories/{REPOSITORY_PATH}/profiles/{PROFILE}/source",
        env.base
    );
    let bootstrap_response = env
        .client
        .get(format!("{source}/bootstrap"))
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap();
    assert_eq!(bootstrap_response.status(), StatusCode::OK);
    let bootstrap: SourceBootstrap =
        serde_json::from_slice(&bootstrap_response.bytes().await.unwrap()).unwrap();
    let engine = Engine::open(&data).unwrap();
    let workspace = Workspace::open(&laptop.workspace).unwrap();
    let revision = workspace.state().unwrap().base_revision;
    let offer = sync::source_offer(
        &engine,
        &bootstrap.native_repository,
        std::slice::from_ref(&revision),
    )
    .unwrap();
    let request = SourceTransferRequest {
        version: SOURCE_TRANSFER_VERSION,
        request_id: "interrupted-source-offer".into(),
        expected_generation: bootstrap.actor_generation,
        source_repository: bootstrap.native_repository.clone(),
        source_change: laptop.change.clone(),
        source_revision: revision.clone(),
        expected_change_version: 0,
        context: ContextDeclaration {
            intent: bootstrap.template_context.intent.clone(),
            acceptance_criteria: bootstrap.template_context.acceptance_criteria.clone(),
            scope: bootstrap.template_context.scope.clone(),
            exclusions: bootstrap.template_context.exclusions.clone(),
            omissions: bootstrap.template_context.omissions.clone(),
        },
        offer,
    };
    let offered_response = env
        .client
        .post(format!("{source}/offers"))
        .bearer_auth(&secret)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&request).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(offered_response.status(), StatusCode::OK);
    let offered: SourceTransfer =
        serde_json::from_slice(&offered_response.bytes().await.unwrap()).unwrap();
    assert_eq!(offered.state, SourceTransferState::AwaitingUpload);
    let original_transfer_id = offered.transfer_id.clone();

    let journal_path = journal_path(&data);
    let mut journal: Value = serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
    journal["pending_source_request"] = serde_json::to_value(&request).unwrap();
    journal["transfer"] = serde_json::to_value(&offered).unwrap();
    fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();

    let newer = laptop.workspace.join("after-offer.txt");
    fs::write(&newer, b"bytes written after the durable offer\n").unwrap();
    let worker = service.spawn_worker();
    let recovered = run_cli(&data, &laptop, "remote-check");
    assert!(!recovered["receipt"].is_null(), "{recovered}");
    assert_eq!(recovered["workspace_changed_during_run"], false);
    assert_eq!(
        fs::read(newer).unwrap(),
        b"bytes written after the durable offer\n"
    );

    let retained_response = env
        .client
        .get(format!("{source}/transfers/{original_transfer_id}"))
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap();
    assert_eq!(retained_response.status(), StatusCode::OK);
    let retained: SourceTransfer =
        serde_json::from_slice(&retained_response.bytes().await.unwrap()).unwrap();
    assert_eq!(retained.state, SourceTransferState::Complete);
    assert_eq!(retained.input_revision, revision);
    assert_ne!(
        recovered["context_subject"]["revision"],
        retained.subject.unwrap().revision
    );
    assert_eq!(queue_counts(root.path()), (2, 1));
    assert_eq!(authority_counts(&native), (0, 0));
    worker.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_routes_reject_scope_before_body_and_preserve_rejected_operations() {
    let _serial = ACCEPTANCE_LOCK.lock().await;
    let root = TempDir::new().unwrap();
    let native = native_source_profile(root.path());
    let (env, _service) = hosted_env(root.path(), &native).await;
    let (token_id, secret) = agent_token(&env, 101, "hosted-recovery").await;
    let token = token_file(root.path(), "recovery.token", &secret);
    let (data, laptop) = args(
        &env,
        token,
        root.path().join("laptop-data"),
        root.path().join("laptop-work"),
        "recovery-laptop",
    );
    run_cli(&data, &laptop, "remote-init");

    let user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == 101)
        .unwrap();
    let (_, read_only, _) = mint_bound_agent_token(
        &*env.store,
        &user.id,
        "hosted-read-only",
        REPOSITORY,
        1,
        3600,
        true,
    )
    .unwrap();
    let (_, wrong_scope) = mint_agent_token(
        &*env.store,
        &user.id,
        "hosted-wrong-scope",
        "synthetic/other",
        3600,
    )
    .unwrap();
    let offers = format!(
        "{}/v1/hub/workspace/repositories/{REPOSITORY_PATH}/profiles/{PROFILE}/source/offers",
        env.base
    );
    for denied in [read_only.expose(), wrong_scope.expose()] {
        let url = reqwest::Url::parse(&offers).unwrap();
        let denied = denied.to_owned();
        let status = tokio::task::spawn_blocking(move || {
            let address = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
            let timeout = std::time::Duration::from_secs(5);
            let mut stream =
                std::net::TcpStream::connect_timeout(&address.parse().unwrap(), timeout).unwrap();
            stream.set_read_timeout(Some(timeout)).unwrap();
            stream.set_write_timeout(Some(timeout)).unwrap();
            // Withhold the oversized body: authorization must reject from headers alone.
            // Uploading it races the early response and can produce a client broken pipe.
            write!(stream, "POST {} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {denied}\r\nContent-Type: application/json\r\nContent-Length: 98304\r\nConnection: close\r\n\r\n", url.path()).unwrap();
            let mut status = [0; 13];
            stream.read_exact(&mut status).unwrap();
            status
        })
        .await
        .unwrap();
        assert_eq!(&status, b"HTTP/1.1 403 ");
    }

    let source = format!(
        "{}/v1/hub/workspace/repositories/{REPOSITORY_PATH}/profiles/{PROFILE}/source",
        env.base
    );
    let bootstrap_url = format!("{source}/bootstrap");
    let response = env
        .client
        .get(&bootstrap_url)
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bootstrap: SourceBootstrap =
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let engine = Engine::open(&data).unwrap();
    let workspace = Workspace::open(&laptop.workspace).unwrap();
    let revision = workspace.state().unwrap().base_revision;
    let offer = sync::source_offer(
        &engine,
        &bootstrap.native_repository,
        std::slice::from_ref(&revision),
    )
    .unwrap();
    let declaration = ContextDeclaration {
        intent: bootstrap.template_context.intent.clone(),
        acceptance_criteria: bootstrap.template_context.acceptance_criteria.clone(),
        scope: bootstrap.template_context.scope.clone(),
        exclusions: bootstrap.template_context.exclusions.clone(),
        omissions: bootstrap.template_context.omissions.clone(),
    };
    let request = SourceTransferRequest {
        version: SOURCE_TRANSFER_VERSION,
        request_id: "unknown-source-operation".into(),
        expected_generation: bootstrap.actor_generation,
        source_repository: bootstrap.native_repository.clone(),
        source_change: "unknown-probe".into(),
        source_revision: revision,
        expected_change_version: 0,
        context: declaration,
        offer,
    };

    let mut stale = request.clone();
    stale.request_id = "stale-source-operation".into();
    stale.expected_generation += 1;
    let stale_response = env
        .client
        .post(&offers)
        .bearer_auth(&secret)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&stale).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(stale_response.status(), StatusCode::CONFLICT);

    let response = env
        .client
        .post(&offers)
        .bearer_auth(&secret)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&request).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let offered: SourceTransfer = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let transfer_url = format!("{source}/transfers/{}", offered.transfer_id);
    let failed_upload = env
        .client
        .put(&transfer_url)
        .bearer_auth(&secret)
        .header("content-type", "application/octet-stream")
        .body("not a native source bundle")
        .send()
        .await
        .unwrap();
    assert_eq!(failed_upload.status(), StatusCode::OK);
    let failed: SourceTransfer =
        serde_json::from_slice(&failed_upload.bytes().await.unwrap()).unwrap();
    assert_eq!(failed.state, SourceTransferState::Rejected);
    let retained_response = env
        .client
        .get(&transfer_url)
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap();
    let retained: SourceTransfer =
        serde_json::from_slice(&retained_response.bytes().await.unwrap()).unwrap();
    assert_eq!(retained.state, SourceTransferState::Rejected);
    assert!(retained.subject.is_none());
    let retained_accounting: (String, i64) = Connection::open(root.path().join("execution.sqlite"))
        .unwrap()
        .query_row(
            "SELECT state,body_bytes FROM workspace_source_transfers WHERE transfer_id=?1",
            [&retained.transfer_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        retained_accounting,
        (
            "rejected".into(),
            b"not a native source bundle".len() as i64
        )
    );
    let rows_before: i64 = Connection::open(root.path().join("execution.sqlite"))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM workspace_source_transfers",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let replay = env
        .client
        .put(&transfer_url)
        .bearer_auth(&secret)
        .header("content-type", "application/octet-stream")
        .body("not a native source bundle")
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    let replayed: SourceTransfer = serde_json::from_slice(&replay.bytes().await.unwrap()).unwrap();
    assert_eq!(replayed, retained);
    let exact_response = env
        .client
        .post(&offers)
        .bearer_auth(&secret)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&request).unwrap())
        .send()
        .await
        .unwrap();
    let exact_retry: SourceTransfer =
        serde_json::from_slice(&exact_response.bytes().await.unwrap()).unwrap();
    assert_eq!(exact_retry, retained);
    let rows_after: i64 = Connection::open(root.path().join("execution.sqlite"))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM workspace_source_transfers",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows_after, rows_before);

    assert!(env.store.revoke_agent_token(&token_id).unwrap());
    let revoked = env
        .client
        .get(&bootstrap_url)
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(authority_counts(&native), (0, 0));
}
