#![cfg(unix)]
//! Real Hub HTTP controls with a local synthetic forge and source-only native state.
//! Source-only service execution uses an inert socket; no Docker or provider process runs.
#[path = "common/cold_restore.rs"]
mod cold_restore;
#[path = "support/fake_github.rs"]
mod fake_github;
#[path = "common/workspace_hub.rs"]
mod workspace_hub;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, process::Command};

use agentscm::attached_worktree::{AttachRequest, AttachmentJournal, SourceMode};
use agentscm::authority::Authority;
use agentscm::checks::SourcePathExpectation;
use agentscm::context::{
    ChangeId, InputIdentity, REVIEW_CONTEXT_VERSION, RequiredEvidence, ReviewContext,
};
use agentscm::execution::{
    COMMAND_CAPABILITY, COMMAND_EVIDENCE, CommandRecipe, ExecutionLimits, OciBackend,
    SyscallPolicy, environment_digest,
};
use agentscm::git_export::{CommitMetadata, Mirror};
use agentscm::hub::store::{mint_agent_token, mint_bound_agent_token};
use agentscm::hub::workspace_execution::{
    AgentTaskProfile, AttachedPublicationProfile, ExecutionConfig, ExecutionService,
};
use agentscm::model::FileChange;
use agentscm::skill_workflow::{
    self, CollectorPayload, CollectorProfile, CollectorProfileAction, CriterionOutcome,
    CriterionResult, EvidenceTrustClass, ExecutorEnrollment, ObservationSignatureAlgorithm,
    SkillReference, StepAction, StepDefinition, TaskAttemptView, TaskOutcome, TaskResultAssertion,
    TrustedLocalCollector, WorkflowDefinition, WorkflowSource, WorkflowTask, runtime_scope_digest,
};
use agentscm::store::{Engine, digest};
use agentscm::team::auth::Role;
use axum::http::header;
use ed25519_dalek::SigningKey;
use reqwest::{Method, Url};
use serde_json::{Value, json};
use workspace_hub::{Env, NativeSourceProfile, native_source_profile};

const REPO: &str = "/v1/hub/workspace/repositories/synthetic%2Fproject";

fn git_path() -> PathBuf {
    PathBuf::from(std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into()))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(git_path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=synthetic contributor",
            "-c",
            "user.email=contributor@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn json_request(builder: reqwest::RequestBuilder, value: &Value) -> reqwest::RequestBuilder {
    builder
        .header(header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(value).unwrap())
}

fn isolated_git(repo: &Path) -> PathBuf {
    let common = PathBuf::from(git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    let wrapper = common.join("synthetic-git");
    let executable = git_path().to_string_lossy().replace('\'', "'\\''");
    let transport = common.join("synthetic-transport.json");
    if transport.exists() {
        let mapping: Value = serde_json::from_slice(&fs::read(transport).unwrap()).unwrap();
        let script = format!(
            "#!/usr/bin/python3\nimport os,sys\nif 'push' in sys.argv[1:]:\n with open({push_log},'a') as f: f.write('push\\n')\nos.environ['GIT_CONFIG_GLOBAL']='/dev/null'\nos.environ['GIT_CONFIG_NOSYSTEM']='1'\na=[{remote} if x=={endpoint} else ('protocol.file.allow=always' if x=='protocol.file.allow=never' else x) for x in sys.argv[1:]]\nos.execv({exe},[{exe},'-c','protocol.file.allow=always']+a)\n",
            push_log = serde_json::to_string(
                &PathBuf::from(mapping["remote"].as_str().unwrap()).with_extension("push-log")
            )
            .unwrap(),
            remote = mapping["remote"],
            endpoint = mapping["endpoint"],
            exe = serde_json::to_string(&git_path()).unwrap()
        );
        fs::write(&wrapper, script).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        return wrapper;
    }
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1\nexec '{executable}' \"$@\"\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    wrapper
}

fn attachment_request(worktree: &Path, actor: &str, output: &str, paths: &[&str]) -> AttachRequest {
    AttachRequest {
        git: isolated_git(worktree),
        worktree: worktree.into(),
        actor: actor.into(),
        remote: "origin".into(),
        target: "main".into(),
        output: output.into(),
        source: SourceMode::Worktree,
        paths: paths.iter().map(|path| (*path).into()).collect(),
    }
}

/// Probe the existing publication boundaries without inventing a published candidate or
/// replacing the imported receipt with a controller-native workflow run.
fn assert_attached_publication_boundary(
    native: &NativeSourceProfile,
    state: &agentscm::attached_worktree::AttachmentState,
) {
    use agentscm::candidate::CandidateSubject;
    use agentscm::hub::forge_check::{NativeState, native_state};

    let authority = Authority::open_read_only(&native.profile.data_dir).unwrap();
    let expected =
        "requires a successful workflow for the exact current revision and review context";
    let error = authority
        .qualify_draft_candidate(&native.context.change)
        .unwrap_err();
    assert!(error.to_string().contains(expected), "{error}");
    let mirror = native.profile.data_dir.join("candidate.git");
    let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(&native.profile.data_dir)
        .args([
            "attached-worktree",
            "prepare",
            "--id",
            &state.id,
            "--expected-generation",
            &state.generation.to_string(),
            "--mirror",
        ])
        .arg(&mirror)
        .args([
            "--operation-id",
            "prepare-imported-current",
            "--name",
            "Synthetic",
            "--email",
            "synthetic@example.invalid",
            "--timestamp",
            "1780000000",
            "--message",
            "Current imported check",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "{stderr}");

    let engine = Engine::open(&native.profile.data_dir).unwrap();
    let journal =
        AttachmentJournal::open_existing(&native.profile.data_dir, &state.repository).unwrap();
    let comparison = journal
        .validate_current(&engine, &state.id, state.generation)
        .unwrap();
    let mut subject = CandidateSubject {
        id: String::new(),
        attachment: state.id.clone(),
        endpoint: state.remote_url.clone(),
        output: state.output.clone(),
        change: native.context.change.clone(),
        change_version: native.subject.change_version,
        context_digest: native.subject.context_digest.clone(),
        policy_epoch: native.context.policy_epoch,
        revision: native.subject.revision.clone(),
        target: state.target.clone(),
        baseline_commit: state.target_commit.clone(),
        parent_commit: state.target_commit.clone(),
        source_commit: state.head_commit.clone(),
        source_fingerprint: state.source_fingerprint.clone(),
        comparison_fingerprint: comparison,
    };
    subject.id = subject.canonical_id().unwrap();
    let profile = AgentTaskProfile {
        repository_id: native.profile.repository_id.clone(),
        profile_id: native.profile.profile_id.clone(),
        native_repository: native.profile.native_repository.clone(),
        data_dir: native.profile.data_dir.clone(),
        target: native.profile.target.clone(),
        requirement_digest: native.profile.requirement_digest.clone(),
        publish_forge_status: true,
    };
    assert_eq!(native_state(&profile, &subject), NativeState::NotQualified);
    assert_eq!(journal.state(&state.id).unwrap(), *state);
    assert!(authority.target("main").unwrap().is_none());
}

mod fake_docker {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/common/fake_docker.rs"
    ));
}

async fn execution_env(
    root: &std::path::Path,
    native: &NativeSourceProfile,
) -> (Env, Arc<ExecutionService>) {
    let profile = native.profile.clone();
    let queue = root.join("execution.sqlite");
    let installed = Arc::new(std::sync::OnceLock::new());
    let retain = installed.clone();
    let env = Env::with_state(move |state, workspace| {
        let service = ExecutionService::open(
            &queue,
            ExecutionConfig {
                attached_publication_profiles: vec![],
                version: 1,
                source_profiles: vec![],
                agent_task_profiles: vec![],
                profiles: vec![profile],
            },
            workspace,
        )
        .unwrap();
        let service = Arc::new(service);
        retain.set(service.clone()).ok().unwrap();
        state.with_workspace_execution(service)
    })
    .await;
    (env, installed.get().unwrap().clone())
}

async fn agent_only_env(
    root: &std::path::Path,
    native: &NativeSourceProfile,
) -> (Env, Arc<ExecutionService>) {
    let profile = AgentTaskProfile {
        repository_id: native.profile.repository_id.clone(),
        profile_id: native.profile.profile_id.clone(),
        native_repository: native.profile.native_repository.clone(),
        data_dir: native.profile.data_dir.clone(),
        target: native.profile.target.clone(),
        requirement_digest: native.profile.requirement_digest.clone(),
        publish_forge_status: false,
    };
    let queue = root.join("agent-only.sqlite");
    let installed = Arc::new(std::sync::OnceLock::new());
    let retain = installed.clone();
    let env = Env::with_state(move |state, workspace| {
        let service = Arc::new(
            ExecutionService::open(
                &queue,
                ExecutionConfig {
                    attached_publication_profiles: vec![],
                    version: 1,
                    profiles: vec![],
                    source_profiles: vec![],
                    agent_task_profiles: vec![profile],
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

async fn mixed_execution_env(
    root: &std::path::Path,
    native: &NativeSourceProfile,
) -> (Env, Arc<ExecutionService>) {
    let server = native.profile.clone();
    let mut task = AgentTaskProfile::from(&server);
    task.profile_id = "agent-checks".into();
    let queue = root.join("mixed-execution.sqlite");
    let installed = Arc::new(std::sync::OnceLock::new());
    let retain = installed.clone();
    let env = Env::with_state(move |state, workspace| {
        let service = Arc::new(
            ExecutionService::open(
                &queue,
                ExecutionConfig {
                    attached_publication_profiles: vec![],
                    version: 1,
                    profiles: vec![server],
                    source_profiles: vec![],
                    agent_task_profiles: vec![task],
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

fn install_executor(native: &NativeSourceProfile) -> serde_json::Value {
    let public = SigningKey::from_bytes(&[7_u8; 32])
        .verifying_key()
        .to_bytes();
    let public_hex = public
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let enrollment = ExecutorEnrollment {
        version: 1,
        registration_id: "registration-http-1".into(),
        executor_id: "executor-http-1".into(),
        operator_id: "operator-http-1".into(),
        repository: native.context.change.repository.clone(),
        step_ids: vec!["source".into()],
        registration_epoch: 1,
        verification_key_id: agentscm::policy::issuer_key_id(&public_hex).unwrap(),
        verification_key_sha256: agentscm::store::digest(&public),
        signature_algorithm: ObservationSignatureAlgorithm::Ed25519,
        issued_at_ms: 1,
    };
    Authority::open(&native.profile.data_dir)
        .unwrap()
        .enroll_workflow_executor(&enrollment, &public_hex)
        .unwrap();
    serde_json::to_value(enrollment.binding().unwrap()).unwrap()
}

async fn agent_client(
    root: &std::path::Path,
    native: &NativeSourceProfile,
) -> (Env, String, Value) {
    let executor = install_executor(native);
    let (env, _) = agent_only_env(root, native).await;
    let (_, owner) = env.signin(101).await;
    let owner_session = env.session(&owner).await;
    let grant = env.member(
        202,
        &[Role::Reader, Role::Contributor],
        true,
        env.workspace.settings().unwrap().generation,
        "add-agent-task-transport-contributor",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_session["csrf_token"].as_str().unwrap()),
            None,
            Some(grant),
        )
        .await
        .0,
        200
    );
    env.signin(202).await;
    let user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == 202)
        .unwrap();
    let (_, token) = mint_agent_token(
        &*env.store,
        &user.id,
        "agent-task-transport-client",
        "synthetic/project",
        3600,
    )
    .unwrap();
    (env, token.expose().to_owned(), executor)
}

fn claim_input(task: &WorkflowTask, executor: &Value, suffix: &str) -> Value {
    json!({
        "version":1,
        "request_id":format!("claim-{suffix}"),
        "execution_operation_id":format!("execute-{suffix}"),
        "task_id":task.task_id,
        "task_digest":task.digest().unwrap(),
        "attempt_id":format!("attempt-{suffix}"),
        "executor":executor,
        "requested_lease_ms":60_000
    })
}

fn mutation_input(view: &TaskAttemptView, request_id: &str, kind: &str) -> Value {
    let action = if kind == "cancel" {
        json!({"kind":"cancel","reason":"synthetic cancellation"})
    } else {
        json!({"kind":kind})
    };
    json!({
        "version":1,
        "request_id":request_id,
        "execution_operation_id":view.attempt.execution_operation_id,
        "task_id":view.attempt.task_id,
        "task_digest":view.attempt.task_digest,
        "attempt_id":view.attempt.attempt_id,
        "attempt_digest":view.attempt.digest().unwrap(),
        "fence":view.attempt.lease.fence,
        "executor":view.attempt.executor,
        "action":action
    })
}

fn large_assertion_input(task: &WorkflowTask, view: &TaskAttemptView, request_id: &str) -> Value {
    let assertion = TaskResultAssertion {
        version: 1,
        task_id: task.task_id.clone(),
        task_digest: task.digest().unwrap(),
        attempt_id: view.attempt.attempt_id.clone(),
        attempt_digest: view.attempt.digest().unwrap(),
        request_id: request_id.into(),
        execution_operation_id: view.attempt.execution_operation_id.clone(),
        agent: view.attempt.agent.clone(),
        executor: view.attempt.executor.clone(),
        fence: view.attempt.lease.fence,
        trust_class: EvidenceTrustClass::AgentAssertion,
        outcome: TaskOutcome::Incomplete,
        criteria: vec![CriterionResult {
            criterion: task.entrypoint.required_criteria[0].id.clone(),
            outcome: CriterionOutcome::Incomplete,
            findings: vec!["\u{1}".repeat(700); 16],
            evidence_digests: vec![],
        }],
        executor_provenance: None,
    };
    let canonical = assertion.canonical_bytes().unwrap();
    assert!(canonical.len() > 64 * 1024);
    assert!(canonical.len() <= 256 * 1024);
    let mut value = serde_json::to_value(assertion).unwrap();
    value.as_object_mut().unwrap().remove("agent");
    value
}

fn mcp_call(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})
}

fn mcp_envelope(response: &Value) -> Value {
    assert_eq!(response["result"]["isError"], false, "{response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// Digest of every row of every table, to prove a read path wrote nothing.
fn database_snapshot(path: &std::path::Path) -> Vec<(String, String)> {
    let Ok(connection) =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return vec![];
    };
    let mut tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    let tables = tables
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    tables
        .into_iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{table}\""))
                .unwrap();
            let columns = statement.column_count();
            let mut rows = statement.query([]).unwrap();
            let mut dump = String::new();
            while let Some(row) = rows.next().unwrap() {
                for column in 0..columns {
                    dump.push_str(&format!("{:?}|", row.get_ref(column).unwrap()));
                }
                dump.push('\n');
            }
            (table, digest(dump.as_bytes()))
        })
        .collect()
}

fn task_state_snapshot(data_dir: &std::path::Path) -> [Vec<(String, String)>; 2] {
    [
        database_snapshot(&data_dir.join("state.sqlite")),
        database_snapshot(&data_dir.join("skill-workflows/state.sqlite")),
    ]
}

const MCP_PATH: &str = "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks";

/// Read-only `qualification` through MCP; asserts it changed no authority or journal row.
async fn qualification(
    env: &Env,
    token: &str,
    data_dir: &std::path::Path,
    subject: &agentscm::hub::workspace_execution::RunSubject,
) -> Value {
    let before = task_state_snapshot(data_dir);
    let response = env
        .request(
            Method::POST,
            MCP_PATH,
            None,
            None,
            Some(token),
            Some(mcp_call(
                "agentscm_task_qualification",
                json!({"subject":subject}),
            )),
        )
        .await;
    assert_eq!(response.0, 200, "{response:?}");
    assert_eq!(
        task_state_snapshot(data_dir),
        before,
        "qualification must not write"
    );
    let envelope = mcp_envelope(&response.1);
    assert_eq!(envelope["capability"], "agentscm.agent-workflow-tasks/v1");
    envelope["result"].clone()
}

fn qualified_task<'a>(report: &'a Value, step: &str) -> &'a Value {
    report["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["step_id"] == step)
        .unwrap()
}

/// Local Change workspace at `revision`, for the read-only Stop hook.
fn stop_workspace(
    data_dir: &std::path::Path,
    change: &str,
    revision: &str,
    path: &std::path::Path,
) -> std::path::PathBuf {
    let repository = Authority::open_read_only(data_dir)
        .unwrap()
        .repository_id()
        .unwrap();
    let engine = Engine::open(data_dir).unwrap();
    agentscm::workspace::Workspace::create(
        &engine,
        path,
        ChangeId {
            repository,
            id: change.into(),
        },
        revision,
    )
    .unwrap();
    fs::canonicalize(path).unwrap()
}

fn stop_rounds(data_dir: &std::path::Path) -> u64 {
    fs::read_dir(data_dir.join("agent-stop"))
        .map(|entries| {
            entries
                .map(|entry| {
                    let state: Value =
                        serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap();
                    state["rounds"].as_u64().unwrap()
                })
                .sum()
        })
        .unwrap_or(0)
}

/// Runs `task-codex-stop` with a Codex payload; it must exit 0 and print one JSON object.
fn task_codex_stop(
    data_dir: &std::path::Path,
    endpoint: &str,
    token_file: &std::path::Path,
    workspace: &std::path::Path,
    change: &str,
    stop_hook_active: bool,
) -> Value {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data_dir)
        .args(["skill-workflow", "task-codex-stop", change, "--endpoint"])
        .arg(endpoint)
        .args([
            "--repository",
            "synthetic/project",
            "--profile",
            "checks",
            "--token-file",
        ])
        .arg(token_file)
        .arg("--workspace")
        .arg(workspace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let payload = json!({"hook_event_name":"Stop","session_id":"synthetic-codex","cwd":workspace,
        "stop_hook_active":stop_hook_active});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Names under the data directory and the workspace metadata directory, main database bytes and
/// `user_version`. Ignored: SQLite's own `-wal`/`-shm` sidecars and the Stop guard's own
/// `agent-stop` counter directory, which is not store state.
fn hook_inventory(data_dir: &std::path::Path, workspace: &std::path::Path) -> Vec<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.to_string_lossy().into_owned();
            if name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("/agent-stop") {
                continue;
            }
            out.push(name);
            if path.is_dir() {
                walk(&path, out);
            }
        }
    }
    let metadata = fs::read_dir(workspace)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("state.sqlite").is_file())
        .unwrap();
    let (mut data_names, mut workspace_names) = (Vec::new(), Vec::new());
    walk(data_dir, &mut data_names);
    walk(&metadata, &mut workspace_names);
    data_names.sort();
    workspace_names.sort();
    let databases = [
        data_dir.join("state.sqlite"),
        data_dir.join("skill-workflows/state.sqlite"),
        metadata.join("state.sqlite"),
    ]
    .into_iter()
    .map(|path| {
        let version: i64 = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
        format!(
            "{}:{}:{version}",
            path.display(),
            digest(&fs::read(&path).unwrap())
        )
    });
    data_names
        .into_iter()
        .chain(workspace_names)
        .chain(databases)
        .collect()
}

fn stop_feedback(output: &Value) -> Value {
    assert_eq!(output["decision"], "block", "{output}");
    serde_json::from_str(output["reason"].as_str().unwrap()).unwrap()
}

fn source_command_profile(
    root: &std::path::Path,
    docker: &std::path::Path,
    docker_socket: &std::path::Path,
    docker_config: &std::path::Path,
) -> (NativeSourceProfile, CollectorProfile) {
    let backend = OciBackend::new(docker, docker_socket, docker_config).unwrap();
    let runtime_identity = backend.probe().identity.expect("fake Docker is available");
    let environment = std::collections::BTreeMap::new();
    let recipe = CommandRecipe {
        version: 1,
        image: format!("sha256:{}", "1".repeat(64)),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "test -r /source/src/example.rs".into(),
        ],
        environment_sha256: environment_digest(&environment).unwrap(),
        environment,
        toolchain_sha256: "2".repeat(64),
        limits: ExecutionLimits {
            timeout_ms: 1_000,
            max_output_bytes: 4_096,
            memory_bytes: 64 * 1024 * 1024,
            scratch_bytes: 1024 * 1024,
            pids: 8,
            cpu_millis: 500,
        },
        syscall_policy: SyscallPolicy::DockerBuiltin,
        no_new_privileges: true,
    };
    let native = root.join("native-transport");
    fs::create_dir_all(&native).unwrap();
    let engine = Engine::open(&native).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                FileChange {
                    path: "skills/check/SKILL.md".into(),
                    content: Some(b"# Run the pinned local check\n".to_vec()),
                },
                FileChange {
                    path: "skills/check/runner.json".into(),
                    content: Some(serde_json::to_vec(&recipe).unwrap()),
                },
                FileChange {
                    path: "src/example.rs".into(),
                    content: Some(b"fn example() -> bool { false }\n".to_vec()),
                },
            ],
        )
        .unwrap();
    let mut authority = Authority::open(&native).unwrap();
    let repository = authority.repository_id().unwrap();
    let identity = |sha256: &str| InputIdentity::Digest {
        sha256: sha256.into(),
    };
    let mut context = ReviewContext {
        version: REVIEW_CONTEXT_VERSION,
        change: ChangeId {
            repository: repository.clone(),
            id: "transport-work".into(),
        },
        change_version: 1,
        revision: revision.clone(),
        base: None,
        target: "main".into(),
        policy_epoch: 0,
        intent: "exercise the complete agent-owned collector transport".into(),
        acceptance_criteria: vec!["the pinned source and command checks pass".into()],
        scope: vec!["src/example.rs".into()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: identity(&recipe.digest().unwrap()),
        toolchain: identity(&recipe.toolchain_sha256),
        environment: identity(&recipe.environment_sha256),
        required_evidence: vec![RequiredEvidence {
            identity: COMMAND_EVIDENCE.into(),
            digest: identity(&recipe.digest().unwrap()),
        }],
        required_capabilities: vec![
            COMMAND_CAPABILITY.into(),
            "agentscm.review-context/v1".into(),
        ],
    };
    let definition = WorkflowDefinition {
        version: 1,
        name: "transport-source-command".into(),
        source: WorkflowSource {
            repository: repository.clone(),
            revision: revision.clone(),
            policy_epoch: 0,
        },
        skill: SkillReference {
            entrypoint: "skills/check/SKILL.md".into(),
            resources: vec!["skills/check/runner.json".into()],
        },
        steps: vec![
            StepDefinition {
                id: "command".into(),
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
                id: "source".into(),
                needs: vec!["command".into()],
                required: true,
                action: StepAction::SourcePaths {
                    expectations: vec![SourcePathExpectation {
                        path: "skills/check/SKILL.md".into(),
                        sha256: digest(b"# Run the pinned local check\n"),
                    }],
                },
            },
        ],
    };
    authority.start_change(&context).unwrap();
    let initial = skill_workflow::compile(&native, &definition).unwrap();
    let requirement =
        agentscm::authority::workflows::WorkflowRequirement::from_plan("main", &initial).unwrap();
    let requirement_digest = authority
        .install_workflow_requirement(&requirement)
        .unwrap();
    let signing = SigningKey::from_bytes(&[7_u8; 32]);
    let public = signing.verifying_key().to_bytes();
    let public_hex = public
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let enrollment = ExecutorEnrollment {
        version: 1,
        registration_id: "registration-http-1".into(),
        executor_id: "executor-http-1".into(),
        operator_id: "operator-http-1".into(),
        repository: repository.clone(),
        step_ids: vec!["command".into()],
        registration_epoch: 1,
        verification_key_id: agentscm::policy::issuer_key_id(&public_hex).unwrap(),
        verification_key_sha256: digest(&public),
        signature_algorithm: ObservationSignatureAlgorithm::Ed25519,
        issued_at_ms: 1,
    };
    authority
        .enroll_workflow_executor(&enrollment, &public_hex)
        .unwrap();
    let collector_profile = CollectorProfile {
        version: 1,
        profile_id: "command-profile".into(),
        authority_audience: repository.clone(),
        repository: repository.clone(),
        workflow_profile: "checks".into(),
        step_id: "command".into(),
        registration_id: enrollment.registration_id.clone(),
        registration_epoch: enrollment.registration_epoch,
        verification_key_id: enrollment.verification_key_id.clone(),
        action: CollectorProfileAction::Command {
            recipe,
            runtime_scope_sha256: runtime_scope_digest(&runtime_identity),
        },
    };
    authority
        .install_workflow_collector_profile(&collector_profile)
        .unwrap();
    let previous_version = context.change_version;
    context.change_version += 1;
    context.policy_epoch = authority.policy_epoch().unwrap();
    let change = authority
        .update_change_context(previous_version, &context)
        .unwrap();
    let plan =
        skill_workflow::compile(&native, &requirement.instantiate(&context).unwrap()).unwrap();
    (
        NativeSourceProfile {
            profile: agentscm::hub::workspace_execution::ExecutionProfile {
                repository_id: "synthetic/project".into(),
                profile_id: "checks".into(),
                native_repository: repository,
                data_dir: native,
                target: "main".into(),
                requirement_digest: requirement_digest.clone(),
                docker: "/usr/bin/false".into(),
                docker_socket: root.join("not-used.socket"),
                claude: "/usr/bin/false".into(),
            },
            subject: agentscm::hub::workspace_execution::RunSubject {
                change_id: context.change.id.clone(),
                change_version: change.version,
                context_digest: change.context_digest,
                revision,
                requirement_digest,
            },
            context,
            plan,
        },
        collector_profile,
    )
}

fn fake_docker_effect_counts(path: &std::path::Path) -> (usize, usize, usize) {
    let calls = fs::read_to_string(path).unwrap_or_default();
    calls.lines().fold((0, 0, 0), |mut counts, line| {
        let args: Vec<String> = serde_json::from_str(line).unwrap();
        if args.first().is_some_and(|value| value == "info")
            || args.first().is_some_and(|value| value == "version")
            || args.first().is_some_and(|value| value == "image")
                && args.get(1).is_some_and(|value| value == "inspect")
        {
            counts.0 += 1;
        } else if args.first().is_some_and(|value| value == "container")
            && args.get(1).is_some_and(|value| value == "create")
        {
            counts.1 += 1;
        } else if args.first().is_some_and(|value| value == "container")
            && args.get(1).is_some_and(|value| value == "start")
        {
            counts.2 += 1;
        }
        counts
    })
}

#[tokio::test]
async fn agent_only_http_and_workspace_mcp_expose_the_same_side_effect_free_requirements() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, service) = agent_only_env(root.path(), &native).await;
    assert!(
        !service.server_execution_enabled(),
        "agent-only configuration must not start the server executor"
    );
    let (_, owner) = env.signin(101).await;
    let owner_session = env.session(&owner).await;
    let grant = env.member(
        202,
        &[Role::Reader, Role::Contributor],
        true,
        env.workspace.settings().unwrap().generation,
        "add-agent-task-contributor",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_session["csrf_token"].as_str().unwrap()),
            None,
            Some(grant),
        )
        .await
        .0,
        200
    );
    let (_, member_cookie) = env.signin(202).await;
    let user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == 202)
        .unwrap();
    let (_, token) = mint_agent_token(
        &*env.store,
        &user.id,
        "agent-task-client",
        "synthetic/project",
        3600,
    )
    .unwrap();
    let task_path = format!("{REPO}/profiles/checks/agent-tasks");
    let body = json!({"operation":"requirements","subject":native.subject});
    let journal = native.profile.data_dir.join("skill-workflows/state.sqlite");
    assert!(!journal.exists());
    let http = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(body.clone()),
        )
        .await;
    assert_eq!(http.0, 200, "{http:?}");
    assert_eq!(http.1["capability"], "agentscm.agent-workflow-tasks/v1");
    assert_eq!(http.1["schema_version"], 1);
    assert!(
        http.1["result"]
            .as_array()
            .is_some_and(|tasks| !tasks.is_empty())
    );
    assert!(
        !journal.exists(),
        "requirements must not initialize the task journal"
    );

    let mcp = env
        .request(
            Method::POST,
            "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks",
            None,
            None,
            Some(token.expose()),
            Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_task_requirements","arguments":{"subject":native.subject}}})),
        )
        .await;
    assert_eq!(mcp.0, 200, "{mcp:?}");
    assert_eq!(mcp.1["result"]["isError"], false);
    let envelope: Value =
        serde_json::from_str(mcp.1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(envelope, http.1);
    assert!(
        !journal.exists(),
        "MCP requirements must remain side-effect free"
    );
    let capabilities_http = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(json!({"operation":"capabilities"})),
        )
        .await;
    assert_eq!(capabilities_http.0, 200, "{capabilities_http:?}");
    let capabilities_mcp = env
        .request(
            Method::POST,
            "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks",
            None,
            None,
            Some(token.expose()),
            Some(mcp_call("agentscm_task_capabilities", json!({}))),
        )
        .await;
    assert_eq!(capabilities_mcp.0, 200, "{capabilities_mcp:?}");
    assert_eq!(mcp_envelope(&capabilities_mcp.1), capabilities_http.1);
    let capabilities = &capabilities_http.1["result"];
    assert_eq!(
        capabilities["capability"],
        "agentscm.agent-workflow-tasks/v1"
    );
    assert_eq!(capabilities["supported_schema_versions"], json!([1]));
    let operations = capabilities["operations"].as_array().unwrap();
    let mut sorted = operations.clone();
    sorted.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    assert_eq!(operations, &sorted);
    for operation in ["capabilities", "qualification", "requirements", "reference"] {
        assert!(operations.contains(&json!(operation)), "{operation}");
    }
    let tools = env
        .request(
            Method::POST,
            "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks",
            None,
            None,
            Some(token.expose()),
            Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})),
        )
        .await;
    let mut tool_operations = tools.1["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .unwrap()
                .trim_start_matches("agentscm_task_")
                .to_owned()
        })
        .collect::<Vec<_>>();
    tool_operations.sort();
    assert_eq!(
        json!(tool_operations),
        capabilities["operations"],
        "capabilities must list exactly the MCP-exposed operations"
    );
    let unauthenticated = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            None,
            Some(json!({"operation":"capabilities"})),
        )
        .await;
    assert!(
        matches!(unauthenticated.0, 401 | 403),
        "{unauthenticated:?}"
    );

    let qualification_http = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(json!({"operation":"qualification","subject":native.subject})),
        )
        .await;
    assert_eq!(qualification_http.0, 200, "{qualification_http:?}");
    let report = &qualification_http.1["result"];
    assert_eq!(report["qualified"], false);
    assert_eq!(report["subject_current"], true);
    assert_eq!(report["receipt_digest"], Value::Null);
    assert_eq!(report["revision"], native.subject.revision);
    assert!(report["next"]["operation"].is_string());
    assert!(
        !journal.exists(),
        "qualification must not initialize the task journal"
    );
    let reference = json!({
        "operation":"reference","subject":native.subject,"task_id":"task-unknown",
        "task_digest":"0".repeat(64),"reference_name":"entrypoint",
        "expected_sha256":"0".repeat(64),"expected_schema_version":2
    });
    let mismatch = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(reference.clone()),
        )
        .await;
    assert_eq!(mismatch.0, 200, "{mismatch:?}");
    assert_eq!(
        mismatch.1["result"],
        json!({
            "status":"schema_mismatch",
            "capability":"agentscm.agent-workflow-tasks/v1",
            "required_schema_version":1,
            "supported_schema_versions":[2],
            "server_schema_versions":[1],
            "action":"Retry with expected_schema_version 1, or update agentscm to a client supporting it."
        })
    );
    let mut mcp_reference = reference;
    mcp_reference.as_object_mut().unwrap().remove("operation");
    let mismatch_mcp = env
        .request(
            Method::POST,
            "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks",
            None,
            None,
            Some(token.expose()),
            Some(mcp_call("agentscm_task_reference", mcp_reference)),
        )
        .await;
    assert_eq!(mcp_envelope(&mismatch_mcp.1), mismatch.1);

    let digest = "0".repeat(64);
    let executor = json!({"registration_id":"missing-registration","executor_id":"missing-executor","registration_epoch":1,"verification_key_id":"missing-key"});
    let status = json!({"version":1,"task_id":"missing-task","task_digest":digest.clone(),"attempt_id":"missing-attempt","attempt_digest":digest.clone(),"fence":1,"executor":executor.clone()});
    let response = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(json!({"operation":"status","subject":native.subject,"status":status})),
        )
        .await;
    assert_eq!(response.0, 200, "{response:?}");
    assert_eq!(response.1["result"], Value::Null);
    let recovery = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(json!({"operation":"recovery","subject":native.subject,"mutation":{
                "version":1,"request_id":"missing-recovery","execution_operation_id":"missing-execution",
                "task_id":"missing-task","task_digest":digest.clone(),"attempt_id":"missing-attempt",
                "attempt_digest":digest,"fence":1,"executor":executor,"action":{"kind":"heartbeat"}
            }})),
        )
        .await;
    assert_eq!(recovery.0, 200, "{recovery:?}");
    assert_eq!(recovery.1["result"]["disposition"], "conclusive_rejection");
    assert_eq!(recovery.1["result"]["observed_state"], "unknown");
    assert!(
        !journal.exists(),
        "status and recovery must not initialize the task journal"
    );

    let mut injected_actor = body.clone();
    injected_actor["actor"] = json!({"owner_id":"caller","agent_id":"caller"});
    assert_eq!(
        env.request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(injected_actor),
        )
        .await
        .0,
        400,
        "the wire contract must not accept a caller-selected actor"
    );
    let (_, read_token, _) = mint_bound_agent_token(
        &*env.store,
        &user.id,
        "read-only-task-client",
        "synthetic/project",
        7,
        3600,
        true,
    )
    .unwrap();
    assert_eq!(
        env.request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(read_token.expose()),
            Some(body.clone()),
        )
        .await
        .0,
        403,
        "legacy read-only scope must not gain task tools"
    );

    let browser = env
        .request(
            Method::POST,
            &task_path,
            Some(&member_cookie),
            Some("not-a-valid-csrf-token"),
            None,
            Some(body),
        )
        .await;
    assert_eq!(browser.0, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_cli_uses_the_real_authenticated_task_route_without_server_execution() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let native = native_source_profile(root.path());
    let (env, token, _) = agent_client(root.path(), &native).await;
    let token_file = root.path().join("agent.token");
    let request_file = root.path().join("requirements.json");
    fs::write(&token_file, token).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &request_file,
        serde_json::to_vec(&json!({"operation":"requirements","subject":native.subject})).unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args([
            "agent-task",
            "call",
            "--endpoint",
            &env.base,
            "--repository",
            "synthetic/project",
            "--profile",
            "checks",
            "--token-file",
            token_file.to_str().unwrap(),
            request_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["capability"], "agentscm.agent-workflow-tasks/v1");
    assert!(
        response["result"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_owned_transport_failure_revision_success_and_native_landing_are_end_to_end() {
    agent_owned_transport_lifecycle(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_git_worktrees_exercise_shared_candidate_branch_publication() {
    agent_owned_transport_lifecycle(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_git_worktrees_require_independent_exact_human_merge() {
    agent_owned_transport_lifecycle_mode(true, "merge").await;
}

async fn agent_owned_transport_lifecycle(attached: bool) {
    agent_owned_transport_lifecycle_mode(attached, "member").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_git_worktrees_refuse_revoked_executor() {
    agent_owned_transport_lifecycle_mode(true, "executor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_git_worktrees_refuse_revoked_publication_token() {
    agent_owned_transport_lifecycle_mode(true, "token").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_git_worktrees_cold_restore_at_same_paths() {
    agent_owned_transport_lifecycle_mode(true, "cold").await;
}

async fn agent_owned_transport_lifecycle_mode(attached: bool, revocation: &str) {
    let human_merge = revocation == "merge";
    let root = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let docker_home = external.path().join("fake-docker-home");
    let docker_config = root.path().join("fake-docker-config");
    let collector_state = root.path().join("collector-state");
    for directory in [&docker_home, &docker_config, &collector_state] {
        fs::create_dir(directory).unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let docker = root.path().join("docker.py");
    let fake = fake_docker::FAKE_DOCKER.replace(
        "home = pathlib.Path(os.environ[\"HOME\"])",
        &format!(
            "home = pathlib.Path({})",
            serde_json::to_string(docker_home.to_str().unwrap()).unwrap()
        ),
    );
    fs::write(&docker, fake).unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    let docker_socket = root.path().join("docker.sock");
    let _socket = std::os::unix::net::UnixListener::bind(&docker_socket).unwrap();
    let (mut native, collector_profile) =
        source_command_profile(root.path(), &docker, &docker_socket, &docker_config);
    let engine = Engine::open(&native.profile.data_dir).unwrap();
    let (env, service) = agent_only_env(root.path(), &native).await;
    let git_fixture = if attached {
        let git_repository = root.path().join("git-repository");
        let contributor_a = root.path().join("contributor-a");
        let contributor_b = root.path().join("contributor-b");
        let target_writer = root.path().join("target-writer");
        fs::create_dir(&git_repository).unwrap();
        git(&git_repository, &["init", "-q"]);
        git(&git_repository, &["config", "core.excludesFile", ""]);
        for (path, contents) in [
            (
                "skills/check/SKILL.md",
                b"# Run the pinned local check\n".as_slice(),
            ),
            (
                "src/example.rs",
                b"fn example() -> bool { false }\n".as_slice(),
            ),
        ] {
            let full = git_repository.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, contents).unwrap();
        }
        let recipe = match &collector_profile.action {
            CollectorProfileAction::Command { recipe, .. } => recipe,
            _ => unreachable!(),
        };
        fs::write(
            git_repository.join("skills/check/runner.json"),
            serde_json::to_vec(recipe).unwrap(),
        )
        .unwrap();
        git(&git_repository, &["add", "."]);
        git(&git_repository, &["commit", "-q", "-m", "synthetic base"]);
        git(&git_repository, &["update-ref", "refs/heads/main", "HEAD"]);
        let remote = external.path().join("provider.git");
        git(
            root.path(),
            &[
                "clone",
                "-q",
                "--bare",
                git_repository.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git(
            &git_repository,
            &[
                "remote",
                "add",
                "origin",
                &format!("https://{}/synthetic/project.git", env.host),
            ],
        );
        git(
            &git_repository,
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(
            &git_repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "contributor-a",
                contributor_a.to_str().unwrap(),
            ],
        );
        git(
            &git_repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "contributor-b",
                contributor_b.to_str().unwrap(),
            ],
        );
        git(
            &git_repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "target-writer",
                target_writer.to_str().unwrap(),
            ],
        );
        let engine = Engine::open(&native.profile.data_dir).unwrap();
        // A synthetic transport adapter connects the pinned HTTPS identity to real bare Git.
        // TLS itself is qualified separately by git_remote/network, not by this scenario.
        fs::write(git_repository.join(".git/synthetic-transport.json"), serde_json::to_vec(&json!({"remote":remote,"endpoint":format!("https://{}/synthetic/project.git", env.host)})).unwrap()).unwrap();
        let attachments =
            AttachmentJournal::open(&native.profile.data_dir, &native.context.change.repository)
                .unwrap();
        let attached_a = attachments
            .attach(
                &engine,
                "attach-contributor-a",
                &attachment_request(
                    &contributor_a,
                    "forge-user-202",
                    "agents/contributor-a",
                    &["src/example.rs"],
                ),
            )
            .unwrap();
        let attached_b = attachments
            .attach(
                &engine,
                "attach-contributor-b",
                &attachment_request(
                    &contributor_b,
                    "forge-user-303",
                    "agents/contributor-b",
                    &["notes/contributor-b.txt"],
                ),
            )
            .unwrap();
        assert_eq!(attached_a.candidate_revision, native.subject.revision);
        assert_eq!(attached_b.candidate_revision, native.subject.revision);
        let attached_a = attachments
            .bind_context(
                &Authority::open(&native.profile.data_dir).unwrap(),
                "bind-contributor-a-initial",
                &attached_a.id,
                attached_a.generation,
                &native.context.change,
            )
            .unwrap();
        Some((
            git_repository,
            contributor_a,
            contributor_b,
            target_writer,
            attachments,
            attached_a,
            attached_b,
        ))
    } else {
        None
    };
    assert!(
        !service.server_execution_enabled(),
        "the task-only profile must not enable server model or command dispatch"
    );

    let (_, owner) = env.signin(101).await;
    let owner_session = env.session(&owner).await;
    let owner_csrf = owner_session["csrf_token"].as_str().unwrap();
    let roles = if attached {
        vec![Role::Reader, Role::Contributor]
    } else {
        vec![
            Role::Reader,
            Role::Contributor,
            Role::Reviewer,
            Role::Integrator,
        ]
    };
    let grant = env.member(
        202,
        &roles,
        true,
        env.workspace.settings().unwrap().generation,
        "grant-agent-owned-transport",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(grant),
        )
        .await
        .0,
        200
    );
    let reviewer_grant = env.member(
        404,
        &[Role::Reader, Role::Reviewer],
        true,
        env.workspace.settings().unwrap().generation,
        "grant-independent-native-reviewer",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(reviewer_grant),
        )
        .await
        .0,
        200
    );
    let integrator_grant = env.member(
        505,
        &[Role::Reader, Role::Integrator],
        true,
        env.workspace.settings().unwrap().generation,
        "grant-independent-native-integrator",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(integrator_grant),
        )
        .await
        .0,
        200
    );
    let (_, reviewer) = env.signin(404).await;
    let reviewer_session = env.session(&reviewer).await;
    let (_, integrator) = env.signin(505).await;
    let integrator_session = env.session(&integrator).await;
    let integrate_path = format!("{REPO}/profiles/checks/integrate");

    if attached {
        let grant = env.member(
            303,
            &[Role::Reader, Role::Contributor],
            true,
            env.workspace.settings().unwrap().generation,
            "second-contributor",
        );
        assert_eq!(
            env.request(
                Method::PUT,
                "/v1/hub/workspace/members",
                Some(&owner),
                Some(owner_csrf),
                None,
                Some(grant)
            )
            .await
            .0,
            200
        );
        let (_, second) = env.signin(303).await;
        assert_ne!(
            env.session(&second).await["actor"],
            reviewer_session["actor"]
        );
    }
    let (_, member) = env.signin(202).await;
    let mut member_session = env.session(&member).await;
    let user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == 202)
        .unwrap();
    let mint_token = || {
        mint_agent_token(
            &*env.store,
            &user.id,
            "agent-owned-transport",
            "synthetic/project",
            3_600,
        )
        .unwrap()
        .1
        .expose()
        .to_owned()
    };
    let mut token = mint_token();
    let token_file = root.path().join("agent.token");
    let key_file = root.path().join("collector.key");
    fs::write(&token_file, &token).unwrap();
    fs::write(&key_file, [7_u8; 32]).unwrap();
    for private in [&token_file, &key_file] {
        fs::set_permissions(private, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let executor = json!({
        "registration_id":"registration-http-1",
        "executor_id":"executor-http-1",
        "registration_epoch":1,
        "verification_key_id":collector_profile.verification_key_id
    });
    let mcp_path = "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks";
    let task_path = format!("{REPO}/profiles/checks/agent-tasks");
    let upload_path = format!("{task_path}/collector-upload");

    let requirements = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_requirements",
                json!({"subject":native.subject}),
            )),
        )
        .await;
    assert_eq!(requirements.0, 200, "{requirements:?}");
    let requirements = mcp_envelope(&requirements.1);
    let first_task: WorkflowTask = serde_json::from_value(
        requirements["result"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["subject"]["step_id"] == "command")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let reads_before = task_state_snapshot(&native.profile.data_dir);
    let instructions = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_instructions",
                json!({
                    "subject":native.subject,
                    "task_id":first_task.task_id,
                    "task_digest":first_task.digest().unwrap(),
                    "supported_schema_versions":[1]
                }),
            )),
        )
        .await;
    assert_eq!(instructions.0, 200, "{instructions:?}");
    assert_eq!(
        mcp_envelope(&instructions.1)["result"]["status"],
        "available"
    );
    let entrypoint = &first_task.entrypoint.references[0];
    let reference = json!({
        "subject":native.subject,
        "task_id":first_task.task_id,
        "task_digest":first_task.digest().unwrap(),
        "reference_name":entrypoint.name,
        "expected_sha256":entrypoint.sha256,
        "expected_schema_version":1
    });
    let fetched = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call("agentscm_task_reference", reference.clone())),
        )
        .await;
    assert_eq!(fetched.0, 200, "{fetched:?}");
    let fetched = mcp_envelope(&fetched.1)["result"].clone();
    assert_eq!(fetched["status"], "available");
    assert_eq!(fetched["task_id"], first_task.task_id);
    assert_eq!(fetched["task_digest"], first_task.digest().unwrap());
    assert_eq!(fetched["sha256"], entrypoint.sha256);
    assert!(fetched["content"].as_str().unwrap().len() <= 128 * 1024);

    let mut stale_digest = reference.clone();
    stale_digest["task_digest"] = json!("0".repeat(64));
    let rejected = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call("agentscm_task_reference", stale_digest)),
        )
        .await;
    assert_eq!(rejected.0, 200, "{rejected:?}");
    assert_eq!(rejected.1["result"]["isError"], true, "{rejected:?}");
    let mut stale_subject = reference.clone();
    stale_subject["subject"]["context_digest"] = json!("0".repeat(64));
    let rejected = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call("agentscm_task_reference", stale_subject)),
        )
        .await;
    assert_eq!(rejected.0, 200, "{rejected:?}");
    assert_eq!(rejected.1["error"]["code"], -32603, "{rejected:?}");
    let mut stale_schema = reference.clone();
    stale_schema["expected_schema_version"] = json!(2);
    let rejected = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call("agentscm_task_reference", stale_schema)),
        )
        .await;
    assert_eq!(rejected.0, 200, "{rejected:?}");
    assert_eq!(
        mcp_envelope(&rejected.1)["result"]["status"],
        "schema_mismatch"
    );
    let (_, cross_repository_token) = mint_agent_token(
        &*env.store,
        &user.id,
        "cross-repository-reference",
        "synthetic/other",
        3_600,
    )
    .unwrap();
    let rejected = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(cross_repository_token.expose()),
            Some(mcp_call("agentscm_task_reference", reference)),
        )
        .await;
    assert_eq!(rejected.0, 200, "{rejected:?}");
    assert_eq!(rejected.1["result"]["isError"], true, "{rejected:?}");
    assert_eq!(
        task_state_snapshot(&native.profile.data_dir),
        reads_before,
        "instruction/reference reads and refusals must not mutate native state"
    );
    let next = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_next",
                json!({"subject":native.subject}),
            )),
        )
        .await;
    assert_eq!(next.0, 200, "{next:?}");
    assert_eq!(
        mcp_envelope(&next.1)["result"]["task_id"],
        first_task.task_id
    );
    let uncollected = qualification(&env, &token, &native.profile.data_dir, &native.subject).await;
    assert_eq!(uncollected["qualified"], false);
    assert_eq!(uncollected["subject_current"], true);
    assert_eq!(uncollected["receipt_digest"], Value::Null);
    assert_eq!(uncollected["revision"], native.subject.revision);
    assert_eq!(qualified_task(&uncollected, "command")["state"], "pending");
    assert_eq!(
        qualified_task(&uncollected, "command")["task_digest"],
        first_task.digest().unwrap()
    );
    assert_eq!(
        uncollected["next"],
        json!({"operation":"collector_claim","task_id":first_task.task_id})
    );
    // Read-only Stop hook before any collection: blocks with the typed next step, and an
    // unreachable endpoint also blocks rather than passes.
    let change_id = native.subject.change_id.clone();
    let stop_ws = stop_workspace(
        &native.profile.data_dir,
        &change_id,
        &native.subject.revision,
        &root.path().join("stop-workspace"),
    );
    let before_stop = task_state_snapshot(&native.profile.data_dir);
    let blocked = stop_feedback(&task_codex_stop(
        &native.profile.data_dir,
        &env.base,
        &token_file,
        &stop_ws,
        &change_id,
        false,
    ));
    assert_eq!(
        blocked["next"],
        json!({"operation":"collector_claim","task_id":first_task.task_id})
    );
    assert!(
        blocked["next_action"]
            .as_str()
            .unwrap()
            .contains("agent-task")
    );
    assert_eq!(stop_rounds(&native.profile.data_dir), 1);
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unreachable = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let offline = stop_feedback(&task_codex_stop(
        &native.profile.data_dir,
        &unreachable,
        &token_file,
        &stop_ws,
        &change_id,
        true,
    ));
    assert!(offline["failures"][0]["detail"].is_string(), "{offline}");
    assert_eq!(stop_rounds(&native.profile.data_dir), 2);
    assert_eq!(
        task_state_snapshot(&native.profile.data_dir),
        before_stop,
        "the Stop hook must not write task state"
    );
    assert!(
        !native
            .profile
            .data_dir
            .join("skill-workflows/state.sqlite")
            .exists(),
        "qualification must not initialize the task journal"
    );
    let claim = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_collector_claim",
                json!({
                    "subject":native.subject,
                    "claim":claim_input(&first_task,&executor,"transport-failure"),
                    "collector_profile_id":"command-profile"
                }),
            )),
        )
        .await;
    assert_eq!(claim.0, 200, "{claim:?}");
    let first_attempt: TaskAttemptView =
        serde_json::from_value(mcp_envelope(&claim.1)["result"].clone()).unwrap();
    let in_progress = qualification(&env, &token, &native.profile.data_dir, &native.subject).await;
    assert_eq!(qualified_task(&in_progress, "command")["state"], "claimed");
    assert_eq!(
        in_progress["failures"],
        json!([{"task_id":first_task.task_id,"code":"attempt_in_progress"}])
    );
    assert_eq!(in_progress["next"], json!({"operation":"none"}));
    let descriptor = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_collector_descriptor",
                json!({"subject":native.subject,"status":{
                    "version":1,
                    "task_id":first_attempt.attempt.task_id,
                    "task_digest":first_attempt.attempt.task_digest,
                    "attempt_id":first_attempt.attempt.attempt_id,
                    "attempt_digest":first_attempt.attempt.digest().unwrap(),
                    "fence":first_attempt.attempt.lease.fence,
                    "executor":first_attempt.attempt.executor
                }}),
            )),
        )
        .await;
    assert_eq!(descriptor.0, 200, "{descriptor:?}");
    let first_descriptor = mcp_envelope(&descriptor.1)["result"].clone();
    let first_descriptor_file = root.path().join("first-descriptor.json");
    fs::write(
        &first_descriptor_file,
        serde_json::to_vec(&first_descriptor).unwrap(),
    )
    .unwrap();
    fs::write(docker_home.join("scenario"), "nonzero").unwrap();

    let cli_at = |endpoint: &str,
                  subcommand: &str,
                  descriptor: &std::path::Path,
                  transfer_request_id: &str,
                  token_file: &std::path::Path,
                  key_file: &std::path::Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command.env("HOME", &docker_home).args([
            "--data-dir",
            native.profile.data_dir.to_str().unwrap(),
            "agent-task",
            subcommand,
            "--endpoint",
            endpoint,
            "--repository",
            "synthetic/project",
            "--profile",
            "checks",
            "--token-file",
            token_file.to_str().unwrap(),
            "--descriptor",
            descriptor.to_str().unwrap(),
            "--state-dir",
            collector_state.to_str().unwrap(),
            "--private-key",
            key_file.to_str().unwrap(),
            "--transfer-request-id",
            transfer_request_id,
        ]);
        if subcommand == "collect" {
            command.args([
                "--docker",
                docker.to_str().unwrap(),
                "--docker-socket",
                docker_socket.to_str().unwrap(),
            ]);
        }
        command.output().unwrap()
    };
    let cli = |subcommand: &str,
               descriptor: &std::path::Path,
               transfer_request_id: &str,
               token_file: &std::path::Path| {
        cli_at(
            &env.base,
            subcommand,
            descriptor,
            transfer_request_id,
            token_file,
            &key_file,
        )
    };

    let cancelled = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_cancel",
                json!({"subject":native.subject,"mutation":mutation_input(&first_attempt,"cancel-before-cli","cancel")}),
            )),
        )
        .await;
    assert_eq!(cancelled.0, 200, "{cancelled:?}");
    let cancelled_report =
        qualification(&env, &token, &native.profile.data_dir, &native.subject).await;
    assert_eq!(cancelled_report["qualified"], false);
    assert_eq!(
        qualified_task(&cancelled_report, "command")["state"],
        "uncertain"
    );
    assert_eq!(
        cancelled_report["failures"],
        json!([{"task_id":first_task.task_id,"code":"attempt_cancelled"}])
    );
    assert_eq!(
        cancelled_report["next"],
        json!({"operation":"reconcile","task_id":first_task.task_id})
    );
    let effects_before_refusals = fake_docker_effect_counts(&docker_home.join("calls.jsonl"));
    let cancelled_collect = cli(
        "collect",
        &first_descriptor_file,
        "cancelled-must-not-run",
        &token_file,
    );
    assert!(!cancelled_collect.status.success());
    assert_eq!(
        fake_docker_effect_counts(&docker_home.join("calls.jsonl")),
        effects_before_refusals
    );
    let reconciled = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_reconcile",
                json!({"subject":native.subject,"mutation":mutation_input(&first_attempt,"reconcile-cancelled-cli","reconcile")}),
            )),
        )
        .await;
    assert_eq!(reconciled.0, 200, "{reconciled:?}");

    let mut expiry_claim_request = claim_input(&first_task, &executor, "expiry-refusal");
    expiry_claim_request["requested_lease_ms"] = json!(1);
    let expiry_claim = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_collector_claim",
                json!({"subject":native.subject,"claim":expiry_claim_request,"collector_profile_id":"command-profile"}),
            )),
        )
        .await;
    assert_eq!(expiry_claim.0, 200, "{expiry_claim:?}");
    let expired_attempt: TaskAttemptView =
        serde_json::from_value(mcp_envelope(&expiry_claim.1)["result"].clone()).unwrap();
    let expired_descriptor = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_collector_descriptor",
                json!({"subject":native.subject,"status":{
                    "version":1,
                    "task_id":expired_attempt.attempt.task_id,
                    "task_digest":expired_attempt.attempt.task_digest,
                    "attempt_id":expired_attempt.attempt.attempt_id,
                    "attempt_digest":expired_attempt.attempt.digest().unwrap(),
                    "fence":expired_attempt.attempt.lease.fence,
                    "executor":expired_attempt.attempt.executor
                }}),
            )),
        )
        .await;
    assert_eq!(expired_descriptor.0, 200, "{expired_descriptor:?}");
    let expired_descriptor_file = root.path().join("expired-descriptor.json");
    fs::write(
        &expired_descriptor_file,
        serde_json::to_vec(&mcp_envelope(&expired_descriptor.1)["result"]).unwrap(),
    )
    .unwrap();
    let expiry_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = env
            .request(
                Method::POST,
                mcp_path,
                None,
                None,
                Some(&token),
                Some(mcp_call(
                    "agentscm_task_status",
                    json!({"subject":native.subject,"status":{
                        "version":1,
                        "task_id":expired_attempt.attempt.task_id,
                        "task_digest":expired_attempt.attempt.task_digest,
                        "attempt_id":expired_attempt.attempt.attempt_id,
                        "attempt_digest":expired_attempt.attempt.digest().unwrap(),
                        "fence":expired_attempt.attempt.lease.fence,
                        "executor":expired_attempt.attempt.executor
                    }}),
                )),
            )
            .await;
        assert_eq!(status.0, 200, "{status:?}");
        if mcp_envelope(&status.1)["result"]["state"] == "expired" {
            break;
        }
        assert!(std::time::Instant::now() < expiry_deadline);
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let expired_report =
        qualification(&env, &token, &native.profile.data_dir, &native.subject).await;
    assert_eq!(expired_report["qualified"], false);
    assert_eq!(
        qualified_task(&expired_report, "command")["state"],
        "uncertain"
    );
    assert_eq!(expired_report["failures"][0]["code"], "attempt_expired");
    assert_eq!(expired_report["next"]["operation"], "reconcile");
    let exact_transport_retry = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_collector_claim",
                json!({"subject":native.subject,"claim":expiry_claim_request,"collector_profile_id":"command-profile"}),
            )),
        )
        .await;
    assert_eq!(exact_transport_retry.0, 200, "{exact_transport_retry:?}");
    assert_eq!(
        exact_transport_retry.1["result"]["isError"], false,
        "{exact_transport_retry:?}"
    );
    let exact_transport_retry: TaskAttemptView =
        serde_json::from_value(mcp_envelope(&exact_transport_retry.1)["result"].clone()).unwrap();
    assert_eq!(exact_transport_retry.attempt, expired_attempt.attempt);
    assert_eq!(exact_transport_retry.state, expired_attempt.state);
    assert!(!exact_transport_retry.currently_applicable);
    let unauthorized_new_attempt = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_collector_claim",
                json!({
                    "subject":native.subject,
                    "claim":claim_input(&first_task,&executor,"expiry-is-not-replacement"),
                    "collector_profile_id":"command-profile"
                }),
            )),
        )
        .await;
    assert_eq!(
        unauthorized_new_attempt.0, 200,
        "{unauthorized_new_attempt:?}"
    );
    assert_eq!(
        unauthorized_new_attempt.1["result"]["isError"], true,
        "{unauthorized_new_attempt:?}"
    );
    let expired_collect = cli(
        "collect",
        &expired_descriptor_file,
        "expired-must-not-run",
        &token_file,
    );
    assert!(!expired_collect.status.success());
    assert_eq!(
        fake_docker_effect_counts(&docker_home.join("calls.jsonl")),
        effects_before_refusals
    );
    let reconciled_expiry = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_reconcile",
                json!({"subject":native.subject,"mutation":mutation_input(&expired_attempt,"reconcile-expired-cli","reconcile")}),
            )),
        )
        .await;
    assert_eq!(reconciled_expiry.0, 200, "{reconciled_expiry:?}");
    let state_after_reconcile = task_state_snapshot(&native.profile.data_dir);
    let reconcile_mutation = mutation_input(&expired_attempt, "reconcile-expired-cli", "reconcile");
    let recovery_http = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({
                "operation":"recovery",
                "subject":native.subject,
                "mutation":reconcile_mutation
            })),
        )
        .await;
    assert_eq!(recovery_http.0, 200, "{recovery_http:?}");
    let recovery_mcp = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_recovery",
                json!({"subject":native.subject,"mutation":reconcile_mutation}),
            )),
        )
        .await;
    assert_eq!(recovery_mcp.0, 200, "{recovery_mcp:?}");
    assert_eq!(
        recovery_mcp.1["result"]["isError"], false,
        "{recovery_mcp:?}"
    );
    let recovered = recovery_http.1["result"].clone();
    assert_eq!(mcp_envelope(&recovery_mcp.1)["result"], recovered);
    assert_eq!(recovered["disposition"], "uncertain_effect");
    assert_eq!(recovered["observed_state"], "replaced");
    assert_eq!(recovered["next_action"], "prepare_fresh_attempt");
    assert_eq!(
        recovered["unresolved_effects"],
        json!([{"effect":"execution_may_still_run"}])
    );
    for (field, expected) in [
        ("request_id", json!("reconcile-expired-cli")),
        (
            "execution_operation_id",
            json!(expired_attempt.attempt.execution_operation_id),
        ),
        ("task_id", json!(expired_attempt.attempt.task_id)),
        ("task_digest", json!(expired_attempt.attempt.task_digest)),
        ("attempt_id", json!(expired_attempt.attempt.attempt_id)),
        (
            "attempt_digest",
            json!(expired_attempt.attempt.digest().unwrap()),
        ),
        ("fence", json!(expired_attempt.attempt.lease.fence)),
    ] {
        assert_eq!(recovered[field], expected, "recovery field {field}");
    }
    let replaced_before_late = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_status",
                json!({"subject":native.subject,"status":{
                    "version":1,
                    "task_id":expired_attempt.attempt.task_id,
                    "task_digest":expired_attempt.attempt.task_digest,
                    "attempt_id":expired_attempt.attempt.attempt_id,
                    "attempt_digest":expired_attempt.attempt.digest().unwrap(),
                    "fence":expired_attempt.attempt.lease.fence,
                    "executor":expired_attempt.attempt.executor
                }}),
            )),
        )
        .await;
    assert_eq!(replaced_before_late.0, 200, "{replaced_before_late:?}");
    assert_eq!(
        replaced_before_late.1["result"]["isError"], false,
        "{replaced_before_late:?}"
    );
    let replaced_before_late = mcp_envelope(&replaced_before_late.1)["result"].clone();
    let receipt_before_late = qualification(
        &env,
        &token,
        &native.profile.data_dir,
        &native.subject,
    )
    .await["receipt_digest"]
        .clone();
    let late_heartbeat = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_heartbeat",
                json!({
                    "subject":native.subject,
                    "mutation":mutation_input(&expired_attempt,"late-heartbeat-after-replacement","heartbeat")
                }),
            )),
        )
        .await;
    assert_eq!(late_heartbeat.0, 200, "{late_heartbeat:?}");
    assert_eq!(
        late_heartbeat.1["result"]["isError"], true,
        "{late_heartbeat:?}"
    );
    let late_result = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({
                "operation":"result",
                "subject":native.subject,
                "assertion":large_assertion_input(&first_task,&expired_attempt,"late-result-after-replacement")
            })),
        )
        .await;
    assert_eq!(late_result.0, 409, "{late_result:?}");
    let replaced_after_late = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_status",
                json!({"subject":native.subject,"status":{
                    "version":1,
                    "task_id":expired_attempt.attempt.task_id,
                    "task_digest":expired_attempt.attempt.task_digest,
                    "attempt_id":expired_attempt.attempt.attempt_id,
                    "attempt_digest":expired_attempt.attempt.digest().unwrap(),
                    "fence":expired_attempt.attempt.lease.fence,
                    "executor":expired_attempt.attempt.executor
                }}),
            )),
        )
        .await;
    assert_eq!(replaced_after_late.0, 200, "{replaced_after_late:?}");
    assert_eq!(
        replaced_after_late.1["result"]["isError"], false,
        "{replaced_after_late:?}"
    );
    assert_eq!(
        mcp_envelope(&replaced_after_late.1)["result"],
        replaced_before_late
    );
    assert_eq!(replaced_before_late["state"], "replaced");
    assert_eq!(
        qualification(&env, &token, &native.profile.data_dir, &native.subject).await["receipt_digest"],
        receipt_before_late
    );
    assert_eq!(
        task_state_snapshot(&native.profile.data_dir),
        state_after_reconcile,
        "recovery reads and fenced late traffic must not mutate native state"
    );
    assert_eq!(
        fake_docker_effect_counts(&docker_home.join("calls.jsonl")),
        effects_before_refusals,
        "expiry, recovery reads and fenced late traffic must not execute externally"
    );

    let replacement_claim = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_collector_claim",
                json!({
                    "subject":native.subject,
                    "claim":claim_input(&first_task,&executor,"transport-failure-replacement"),
                    "collector_profile_id":"command-profile"
                }),
            )),
        )
        .await;
    assert_eq!(replacement_claim.0, 200, "{replacement_claim:?}");
    let first_attempt: TaskAttemptView =
        serde_json::from_value(mcp_envelope(&replacement_claim.1)["result"].clone()).unwrap();
    let replacement_descriptor = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_collector_descriptor",
                json!({"subject":native.subject,"status":{
                    "version":1,
                    "task_id":first_attempt.attempt.task_id,
                    "task_digest":first_attempt.attempt.task_digest,
                    "attempt_id":first_attempt.attempt.attempt_id,
                    "attempt_digest":first_attempt.attempt.digest().unwrap(),
                    "fence":first_attempt.attempt.lease.fence,
                    "executor":first_attempt.attempt.executor
                }}),
            )),
        )
        .await;
    assert_eq!(replacement_descriptor.0, 200, "{replacement_descriptor:?}");
    let first_descriptor = mcp_envelope(&replacement_descriptor.1)["result"].clone();
    fs::write(
        &first_descriptor_file,
        serde_json::to_vec(&first_descriptor).unwrap(),
    )
    .unwrap();
    let failed = cli(
        "collect",
        &first_descriptor_file,
        "transfer-failure",
        &token_file,
    );
    assert!(
        failed.status.success(),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    let failed_envelope: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(failed_envelope["result"]["state"], "finished");
    let failed_report =
        qualification(&env, &token, &native.profile.data_dir, &native.subject).await;
    assert_eq!(failed_report["qualified"], false);
    assert_eq!(qualified_task(&failed_report, "command")["state"], "failed");
    assert_eq!(failed_report["failures"][0]["code"], "collector_not_passed");
    assert_eq!(failed_report["next"], json!({"operation":"none"}));
    let effects_after_failure = fake_docker_effect_counts(&docker_home.join("calls.jsonl"));
    assert_eq!((effects_after_failure.1, effects_after_failure.2), (1, 1));

    let collector = TrustedLocalCollector::new(
        &collector_state,
        collector_profile.clone(),
        SigningKey::from_bytes(&[7_u8; 32]),
    )
    .unwrap();
    let no_http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    no_http.set_nonblocking(true).unwrap();
    let no_http_endpoint = format!("http://{}", no_http.local_addr().unwrap());
    for (label, pointer) in [
        ("profile", "/profile/profile_id"),
        ("task", "/task/task_id"),
        ("attempt", "/attempt/attempt_id"),
        ("origin", "/origin/grant_id"),
    ] {
        let mut mismatched = first_descriptor.clone();
        *mismatched.pointer_mut(pointer).unwrap() = json!(format!("mismatched-{label}"));
        let path = root.path().join(format!("mismatched-{label}.json"));
        fs::write(&path, serde_json::to_vec(&mismatched).unwrap()).unwrap();
        let rejected = cli_at(
            &no_http_endpoint,
            "retry-upload",
            &path,
            "transfer-failure",
            &token_file,
            &key_file,
        );
        assert!(!rejected.status.success(), "{label} mismatch was accepted");
        assert!(matches!(
            no_http.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }
    let wrong_key = root.path().join("wrong-collector.key");
    fs::write(&wrong_key, [8_u8; 32]).unwrap();
    fs::set_permissions(&wrong_key, fs::Permissions::from_mode(0o600)).unwrap();
    let rejected_key = cli_at(
        &no_http_endpoint,
        "retry-upload",
        &first_descriptor_file,
        "transfer-failure",
        &token_file,
        &wrong_key,
    );
    assert!(!rejected_key.status.success());
    assert!(matches!(
        no_http.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    let collector_database = collector_state.join("collector.sqlite");
    let connection = rusqlite::Connection::open(&collector_database).unwrap();
    connection
        .execute(
            "UPDATE collector_transfers SET transfer_request_id='aliased-transfer' WHERE transfer_request_id='transfer-failure'",
            [],
        )
        .unwrap();
    let rejected_request = cli_at(
        &no_http_endpoint,
        "retry-upload",
        &first_descriptor_file,
        "aliased-transfer",
        &token_file,
        &key_file,
    );
    assert!(!rejected_request.status.success());
    assert!(matches!(
        no_http.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    connection
        .execute(
            "UPDATE collector_transfers SET transfer_request_id='transfer-failure' WHERE transfer_request_id='aliased-transfer'",
            [],
        )
        .unwrap();
    drop(connection);
    let mut fabricated = collector.retained_transfer("transfer-failure").unwrap();
    let CollectorPayload::Command { record, .. } = &mut fabricated.payload else {
        unreachable!()
    };
    record.detail.push_str(" fabricated");
    let fabricated_upload = env
        .request(
            Method::POST,
            &upload_path,
            None,
            None,
            Some(&token),
            Some(json!({"subject":native.subject,"transfer":fabricated})),
        )
        .await;
    assert_eq!(fabricated_upload.0, 400, "{fabricated_upload:?}");

    let approve_path = format!("{REPO}/profiles/checks/approve");
    let blocked = env
        .request(
            Method::POST,
            &approve_path,
            Some(&member),
            Some(member_session["csrf_token"].as_str().unwrap()),
            None,
            Some(json!({
                "request_id":"failure-must-block",
                "expected_generation":member_session["actor"]["generation"],
                "subject":native.subject
            })),
        )
        .await;
    assert_eq!(blocked.0, if attached { 403 } else { 409 }, "{blocked:?}");

    let retried = cli(
        "retry-upload",
        &first_descriptor_file,
        "transfer-failure",
        &token_file,
    );
    assert!(
        retried.status.success(),
        "{}",
        String::from_utf8_lossy(&retried.stderr)
    );
    let repeated_collect = cli(
        "collect",
        &first_descriptor_file,
        "different-upload-same-execution",
        &token_file,
    );
    assert!(!repeated_collect.status.success());
    assert_eq!(
        fake_docker_effect_counts(&docker_home.join("calls.jsonl")),
        effects_after_failure,
        "retry-upload and a restarted collector must not repeat probes or execution"
    );

    let disable = env.member(
        202,
        &roles,
        false,
        env.workspace.settings().unwrap().generation,
        "revoke-agent-owned-origin",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(disable),
        )
        .await
        .0,
        200
    );
    let revoked_retry = cli(
        "retry-upload",
        &first_descriptor_file,
        "transfer-failure",
        &token_file,
    );
    assert!(!revoked_retry.status.success());
    assert_eq!(
        fake_docker_effect_counts(&docker_home.join("calls.jsonl")),
        effects_after_failure
    );
    let reenable = env.member(
        202,
        &roles,
        true,
        env.workspace.settings().unwrap().generation,
        "reenroll-agent-owned-origin",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(reenable),
        )
        .await
        .0,
        200
    );
    let (_, current_member) = env.signin(202).await;
    member_session = env.session(&current_member).await;
    token = mint_token();
    fs::write(&token_file, &token).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();

    let old_subject = native.subject.clone();
    let attached_current = if let Some((
        git_repository,
        contributor_a,
        contributor_b,
        target_writer,
        attachments,
        attached_a,
        attached_b,
    )) = &git_fixture
    {
        fs::write(
            contributor_a.join("src/example.rs"),
            b"fn example() -> bool { true }\n",
        )
        .unwrap();
        let fixed = attachments
            .capture(
                &engine,
                "capture-contributor-a-fix",
                &attached_a.id,
                attached_a.generation,
            )
            .unwrap();
        assert_eq!(fixed.changed_paths, ["src/example.rs"]);

        fs::create_dir_all(contributor_b.join("notes")).unwrap();
        fs::write(
            contributor_b.join("notes/contributor-b.txt"),
            b"independent concurrent edit\n",
        )
        .unwrap();
        let concurrent = attachments
            .capture(
                &engine,
                "capture-contributor-b-concurrent-edit",
                &attached_b.id,
                attached_b.generation,
            )
            .unwrap();
        assert_eq!(concurrent.changed_paths, ["notes/contributor-b.txt"]);
        assert_eq!(
            engine
                .read_file(&concurrent.revision, "notes/contributor-b.txt")
                .unwrap(),
            b"independent concurrent edit\n"
        );
        let mut concurrent_context = native.context.clone();
        concurrent_context.change = ChangeId {
            repository: native.context.change.repository.clone(),
            id: "contributor-b-concurrent-work".into(),
        };
        concurrent_context.change_version = 1;
        concurrent_context.revision = concurrent.revision.clone();
        concurrent_context.intent = "preserve the second contributor's concurrent work".into();
        concurrent_context.acceptance_criteria =
            vec!["the independent note remains attached to its exact source".into()];
        concurrent_context.scope = vec!["notes/contributor-b.txt".into()];
        let mut authority = Authority::open(&native.profile.data_dir).unwrap();
        authority.start_change(&concurrent_context).unwrap();
        attachments
            .bind_context(
                &authority,
                "bind-contributor-b-concurrent-change",
                &concurrent.attachment.id,
                concurrent.attachment.generation,
                &concurrent_context.change,
            )
            .unwrap();

        let previous_version = native.context.change_version;
        native.context.change_version += 1;
        native.context.revision = fixed.revision.clone();
        let changed = Authority::open(&native.profile.data_dir)
            .unwrap()
            .update_change_context(previous_version, &native.context)
            .unwrap();
        native.subject.change_version = changed.version;
        native.subject.context_digest = changed.context_digest;
        native.subject.revision = fixed.revision;
        let fixed_subject = native.subject.clone();
        let fixed = attachments
            .bind_context(
                &Authority::open(&native.profile.data_dir).unwrap(),
                "bind-contributor-a-fix",
                &fixed.attachment.id,
                fixed.attachment.generation,
                &native.context.change,
            )
            .unwrap();

        fs::write(
            target_writer.join("target-advance.txt"),
            b"independent target advancement\n",
        )
        .unwrap();
        git(target_writer, &["add", "target-advance.txt"]);
        git(
            target_writer,
            &["commit", "-q", "-m", "advance synthetic target"],
        );
        let advanced_target = git(target_writer, &["rev-parse", "HEAD"]);
        git(
            git_repository,
            &["update-ref", "refs/remotes/origin/main", &advanced_target],
        );
        git(
            target_writer,
            &[
                "push",
                "-q",
                external.path().join("provider.git").to_str().unwrap(),
                "HEAD:main",
            ],
        );
        let target_error = attachments
            .validate_current(&engine, &fixed.id, fixed.generation)
            .unwrap_err()
            .to_string();
        assert!(target_error.contains("target advanced"), "{target_error}");
        let refreshed = attachments
            .refresh(
                &engine,
                "refresh-contributor-a-target",
                &fixed.id,
                fixed.generation,
            )
            .unwrap();
        assert_eq!(refreshed.target_commit, advanced_target);
        assert_eq!(
            engine
                .read_file(&refreshed.candidate_revision, "src/example.rs")
                .unwrap(),
            b"fn example() -> bool { true }\n"
        );
        assert_eq!(
            engine
                .read_file(&refreshed.candidate_revision, "target-advance.txt")
                .unwrap(),
            b"independent target advancement\n"
        );
        assert_eq!(
            fs::read(contributor_b.join("notes/contributor-b.txt")).unwrap(),
            b"independent concurrent edit\n",
            "target refresh must not overwrite the other contributor's worktree"
        );
        let previous_version = native.context.change_version;
        native.context.change_version += 1;
        native.context.revision = refreshed.candidate_revision.clone();
        let changed = Authority::open(&native.profile.data_dir)
            .unwrap()
            .update_change_context(previous_version, &native.context)
            .unwrap();
        native.subject.change_version = changed.version;
        native.subject.context_digest = changed.context_digest;
        native.subject.revision = refreshed.candidate_revision.clone();
        let refreshed = attachments
            .bind_context(
                &Authority::open(&native.profile.data_dir).unwrap(),
                "bind-contributor-a-refreshed-target",
                &refreshed.id,
                refreshed.generation,
                &native.context.change,
            )
            .unwrap();

        Some((refreshed, fixed_subject, concurrent_context))
    } else {
        let CollectorProfileAction::Command { recipe, .. } = &collector_profile.action else {
            unreachable!()
        };
        let next_revision = engine
            .checkpoint(
                None,
                &[
                    FileChange {
                        path: "skills/check/SKILL.md".into(),
                        content: Some(b"# Run the pinned local check\n".to_vec()),
                    },
                    FileChange {
                        path: "skills/check/runner.json".into(),
                        content: Some(serde_json::to_vec(recipe).unwrap()),
                    },
                    FileChange {
                        path: "src/example.rs".into(),
                        content: Some(b"fn example() -> bool { true }\n".to_vec()),
                    },
                ],
            )
            .unwrap();
        let previous_version = native.context.change_version;
        native.context.change_version += 1;
        native.context.revision = next_revision.clone();
        let changed = Authority::open(&native.profile.data_dir)
            .unwrap()
            .update_change_context(previous_version, &native.context)
            .unwrap();
        native.subject.change_version = changed.version;
        native.subject.context_digest = changed.context_digest;
        native.subject.revision = next_revision;

        None
    };

    let stale_retry = cli(
        "retry-upload",
        &first_descriptor_file,
        "transfer-failure",
        &token_file,
    );
    assert!(!stale_retry.status.success());
    let stale_blocked = env
        .request(
            Method::POST,
            &approve_path,
            Some(&current_member),
            Some(member_session["csrf_token"].as_str().unwrap()),
            None,
            Some(json!({
                "request_id":"stale-failure-must-not-authorize",
                "expected_generation":member_session["actor"]["generation"],
                "subject":native.subject
            })),
        )
        .await;
    assert_eq!(
        stale_blocked.0,
        if attached { 403 } else { 409 },
        "{stale_blocked:?}"
    );

    if let Some((_, fixed_subject, _)) = &attached_current {
        let stale_fixed =
            qualification(&env, &token, &native.profile.data_dir, fixed_subject).await;
        assert_eq!(stale_fixed["qualified"], false);
        assert_eq!(stale_fixed["subject_current"], false);
        assert_eq!(stale_fixed["receipt_digest"], Value::Null);
    }

    let fresh_requirements = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_requirements",
                json!({"subject":native.subject}),
            )),
        )
        .await;
    assert_eq!(fresh_requirements.0, 200, "{fresh_requirements:?}");
    let fresh_requirements = mcp_envelope(&fresh_requirements.1);
    let fresh_task: WorkflowTask = serde_json::from_value(
        fresh_requirements["result"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["subject"]["step_id"] == "command")
            .unwrap()
            .clone(),
    )
    .unwrap();
    assert_ne!(fresh_task.task_id, first_task.task_id);
    let fresh_next = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_next",
                json!({"subject":native.subject}),
            )),
        )
        .await;
    assert_eq!(fresh_next.0, 200, "{fresh_next:?}");
    assert_eq!(
        mcp_envelope(&fresh_next.1)["result"]["task_id"],
        fresh_task.task_id
    );
    // An older subject's unresolved attempt sends the agent to reconcile it, never to a claim
    // the claim path would refuse. The journal row is restored afterwards.
    let journal_path = native.profile.data_dir.join("skill-workflows/state.sqlite");
    let journal = rusqlite::Connection::open(&journal_path).unwrap();
    type Retained = (
        String,
        String,
        i64,
        Option<i64>,
        Option<String>,
        Option<String>,
    );
    let retained: Retained = journal
        .query_row(
            "SELECT attempt_id,state,lease_expires_at,superseding_fence,assertion_digest,collector_transfer_digest FROM agent_workflow_attempts WHERE task_id=?1 ORDER BY fence DESC LIMIT 1",
            [&first_task.task_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    journal
        .execute(
            "UPDATE agent_workflow_attempts SET state='cancelled',superseding_fence=NULL,assertion_digest=NULL,collector_transfer_digest=NULL WHERE attempt_id=?1",
            [&retained.0],
        )
        .unwrap();
    let prior = qualification(&env, &token, &native.profile.data_dir, &native.subject).await;
    assert_eq!(prior["qualified"], false);
    assert_eq!(prior["subject_current"], true);
    assert_eq!(
        prior["next"],
        json!({"operation":"reconcile","task_id":first_task.task_id})
    );
    assert_eq!(
        prior["failures"],
        json!([{"task_id":first_task.task_id,"code":"prior_subject_unresolved"}])
    );
    journal
        .execute(
            "UPDATE agent_workflow_attempts SET state=?2,lease_expires_at=?3,superseding_fence=?4,assertion_digest=?5,collector_transfer_digest=?6 WHERE attempt_id=?1",
            rusqlite::params![retained.0, retained.1, retained.2, retained.3, retained.4, retained.5],
        )
        .unwrap();
    drop(journal);

    let fresh_claim = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_collector_claim",
                json!({
                    "subject":native.subject,
                    "claim":claim_input(&fresh_task,&executor,"transport-success"),
                    "collector_profile_id":"command-profile"
                }),
            )),
        )
        .await;
    assert_eq!(fresh_claim.0, 200, "{fresh_claim:?}");
    let fresh_attempt: TaskAttemptView =
        serde_json::from_value(mcp_envelope(&fresh_claim.1)["result"].clone()).unwrap();
    let fresh_descriptor = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_collector_descriptor",
                json!({"subject":native.subject,"status":{
                    "version":1,
                    "task_id":fresh_attempt.attempt.task_id,
                    "task_digest":fresh_attempt.attempt.task_digest,
                    "attempt_id":fresh_attempt.attempt.attempt_id,
                    "attempt_digest":fresh_attempt.attempt.digest().unwrap(),
                    "fence":fresh_attempt.attempt.lease.fence,
                    "executor":fresh_attempt.attempt.executor
                }}),
            )),
        )
        .await;
    assert_eq!(fresh_descriptor.0, 200, "{fresh_descriptor:?}");
    let fresh_descriptor_file = root.path().join("fresh-descriptor.json");
    fs::write(
        &fresh_descriptor_file,
        serde_json::to_vec(&mcp_envelope(&fresh_descriptor.1)["result"]).unwrap(),
    )
    .unwrap();
    fs::write(docker_home.join("scenario"), "success").unwrap();
    let success = cli(
        "collect",
        &fresh_descriptor_file,
        "transfer-success",
        &token_file,
    );
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stderr)
    );
    let effects_after_success = fake_docker_effect_counts(&docker_home.join("calls.jsonl"));
    assert_eq!((effects_after_success.1, effects_after_success.2), (2, 2));
    assert!(effects_after_success.0 > effects_after_failure.0);
    let qualified = qualification(&env, &token, &native.profile.data_dir, &native.subject).await;
    assert_eq!(qualified["qualified"], true, "{qualified}");
    assert_eq!(qualified["subject_current"], true);
    assert_eq!(qualified["revision"], native.subject.revision);
    assert_eq!(qualified_task(&qualified, "command")["state"], "succeeded");
    assert_eq!(qualified["next"], json!({"operation":"none"}));
    assert!(qualified["failures"].as_array().unwrap().is_empty());
    let receipt_digest = qualified["receipt_digest"].as_str().unwrap().to_owned();
    assert_eq!(receipt_digest.len(), 64);
    let qualified_ws = stop_workspace(
        &native.profile.data_dir,
        &native.subject.change_id,
        &native.subject.revision,
        &root.path().join("qualified-workspace"),
    );
    let before_pass = task_state_snapshot(&native.profile.data_dir);
    assert_eq!(
        task_codex_stop(
            &native.profile.data_dir,
            &env.base,
            &token_file,
            &qualified_ws,
            &native.subject.change_id,
            true,
        ),
        json!({})
    );
    fs::write(qualified_ws.join("uncheckpointed.txt"), b"edit\n").unwrap();
    let inventory_before = hook_inventory(&native.profile.data_dir, &qualified_ws);
    let dirty = stop_feedback(&task_codex_stop(
        &native.profile.data_dir,
        &env.base,
        &token_file,
        &qualified_ws,
        &native.subject.change_id,
        true,
    ));
    assert!(
        dirty["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["detail"] == "workspace has uncheckpointed edits"),
        "{dirty}"
    );
    assert_eq!(dirty["next"], json!({"operation":"checkpoint"}));
    assert_eq!(
        hook_inventory(&native.profile.data_dir, &qualified_ws),
        inventory_before,
        "the Stop hook must not create or change store or workspace database files"
    );
    fs::remove_file(qualified_ws.join("uncheckpointed.txt")).unwrap();
    assert_eq!(
        task_state_snapshot(&native.profile.data_dir),
        before_pass,
        "the Stop hook must not write task state"
    );
    // An older-schema workspace is neither migrated nor trusted: the hook blocks with a fixed
    // category and leaves the database file untouched.
    let older_ws = stop_workspace(
        &native.profile.data_dir,
        &native.subject.change_id,
        &native.subject.revision,
        &root.path().join("older-schema-workspace"),
    );
    let older_db = fs::read_dir(&older_ws)
        .unwrap()
        .map(|entry| entry.unwrap().path().join("state.sqlite"))
        .find(|path| path.is_file())
        .unwrap();
    let raw = rusqlite::Connection::open(&older_db).unwrap();
    raw.pragma_update(None, "user_version", 3).unwrap();
    drop(raw);
    let older_before = (
        fs::read(&older_db).unwrap(),
        fs::metadata(&older_db).unwrap().modified().unwrap(),
    );
    let older = stop_feedback(&task_codex_stop(
        &native.profile.data_dir,
        &env.base,
        &token_file,
        &older_ws,
        &native.subject.change_id,
        true,
    ));
    assert_eq!(
        older["failures"][0]["detail"],
        "local native store, workspace or token file is unavailable"
    );
    assert!(!older.to_string().contains(older_ws.to_str().unwrap()));
    assert_eq!(
        (
            fs::read(&older_db).unwrap(),
            fs::metadata(&older_db).unwrap().modified().unwrap()
        ),
        older_before
    );
    let raw = rusqlite::Connection::open(&older_db).unwrap();
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
    drop(raw);
    let superseded = qualification(&env, &token, &native.profile.data_dir, &old_subject).await;
    assert_eq!(superseded["qualified"], false);
    assert_eq!(superseded["subject_current"], false);
    assert_eq!(superseded["receipt_digest"], Value::Null);
    assert_eq!(superseded["revision"], old_subject.revision);
    assert_eq!(superseded["next"], json!({"operation":"checkpoint"}));

    let agent_approval = env
        .request(
            Method::POST,
            &approve_path,
            None,
            None,
            Some(&token),
            Some(json!({
                "request_id":"agent-token-cannot-approve",
                "expected_generation":member_session["actor"]["generation"],
                "subject":native.subject
            })),
        )
        .await;
    assert_eq!(agent_approval.0, 403, "{agent_approval:?}");
    let integrator_approval = env
        .request(
            Method::POST,
            &approve_path,
            Some(&integrator),
            Some(integrator_session["csrf_token"].as_str().unwrap()),
            None,
            Some(json!({
                "request_id":"integrator-cannot-approve",
                "expected_generation":integrator_session["actor"]["generation"],
                "subject":native.subject
            })),
        )
        .await;
    assert_eq!(integrator_approval.0, 403, "{integrator_approval:?}");

    let approval = env
        .request(
            Method::POST,
            &approve_path,
            Some(&reviewer),
            Some(reviewer_session["csrf_token"].as_str().unwrap()),
            None,
            Some(json!({
                "request_id":"approve-agent-owned-success",
                "expected_generation":reviewer_session["actor"]["generation"],
                "subject":native.subject
            })),
        )
        .await;
    assert_eq!(approval.0, 200, "{approval:?}");
    let reviewer_integration = env
        .request(
            Method::POST,
            &integrate_path,
            Some(&reviewer),
            Some(reviewer_session["csrf_token"].as_str().unwrap()),
            None,
            Some(json!({
                "request_id":"reviewer-cannot-integrate",
                "expected_generation":reviewer_session["actor"]["generation"],
                "subject":native.subject,
                "approval_id":approval.1["approval_id"]
            })),
        )
        .await;
    assert_eq!(reviewer_integration.0, 403, "{reviewer_integration:?}");
    let agent_integration = env
        .request(
            Method::POST,
            &integrate_path,
            None,
            None,
            Some(&token),
            Some(json!({
                "request_id":"agent-token-cannot-integrate",
                "expected_generation":member_session["actor"]["generation"],
                "subject":native.subject,
                "approval_id":approval.1["approval_id"]
            })),
        )
        .await;
    assert_eq!(agent_integration.0, 403, "{agent_integration:?}");
    if let Some((state, _, concurrent_context)) = &attached_current {
        let mirror_path = root.path().join("hub-candidate.git");
        let mirror = Mirror::initialize_with_seed(
            Path::new(&state.git),
            &mirror_path,
            &native.profile.native_repository,
            &state.target,
            Path::new(&state.worktree),
            &state.target_commit,
        )
        .unwrap();
        mirror
            .remote_bind_network(
                agentscm::git_export::remote::Endpoint::Https {
                    url: state.remote_url.clone(),
                    trust: agentscm::git_export::remote::HttpsTrust::System,
                },
                &state.output,
            )
            .unwrap();
        let owner_user = env
            .store
            .list_users()
            .unwrap()
            .into_iter()
            .find(|user| user.identity.user_id == 202)
            .unwrap();
        let credential = root.path().join("synthetic-git-credential.json");
        fs::write(
            &credential,
            br#"{"username":"synthetic","password":"synthetic-fixture-only"}"#,
        )
        .unwrap();
        fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
        let publication_profile = AttachedPublicationProfile {
            repository_id: native.profile.repository_id.clone(),
            profile_id: native.profile.profile_id.clone(),
            attachment_id: state.id.clone(),
            owner_id: owner_user.id.clone(),
            git: PathBuf::from(&state.git),
            worktree: PathBuf::from(&state.worktree),
            git_common_dir: PathBuf::from(&state.git_common_dir),
            git_dir: PathBuf::from(&state.git_dir),
            index: PathBuf::from(&state.index),
            mirror: mirror_path,
            endpoint: state.remote_url.clone(),
            target: state.target.clone(),
            output: state.output.clone(),
            credential_file: Some(credential),
            ca_file: None,
        };
        let config_file = root.path().join("publication-config.json");
        fs::write(&config_file, serde_json::to_vec(&json!({
            "version":1, "profiles":[], "source_profiles":[],
            "agent_task_profiles":[AgentTaskProfile {
                repository_id:native.profile.repository_id.clone(), profile_id:native.profile.profile_id.clone(),
                native_repository:native.profile.native_repository.clone(), data_dir:native.profile.data_dir.clone(),
                target:native.profile.target.clone(), requirement_digest:native.profile.requirement_digest.clone(),
                publish_forge_status:true,
            }], "attached_publication_profiles":[publication_profile]
        })).unwrap()).unwrap();
        let publication_service = Arc::new(
            ExecutionService::open(
                &root.path().join("publication.sqlite"),
                ExecutionConfig::from_file(&config_file).unwrap(),
                env.workspace.clone(),
            )
            .unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let publication_base = format!("http://{}", listener.local_addr().unwrap());
        fs::write(
            root.path().join("publication-listen.json"),
            serde_json::to_vec(&listener.local_addr().unwrap().to_string()).unwrap(),
        )
        .unwrap();
        let mut fake = fake_github::FakeGithub::start().await;
        fake.with(|s| {
            let mut repo = fake_github::Repo::new(100, "synthetic/project");
            repo.git = Some(external.path().join("provider.git"));
            s.repos.insert(100, repo);
            s.pats.insert(
                "synthetic-publication-pat".into(),
                ("robot".into(), 77, [100].into()),
            );
        });
        let secrets = root.path().join("publication-secrets");
        agentscm::hub::sync::prepare_secrets_dir(&secrets, true).unwrap();
        let pat_file = root.path().join("publication.pat");
        fs::write(&pat_file, "synthetic-publication-pat").unwrap();
        fs::set_permissions(&pat_file, fs::Permissions::from_mode(0o600)).unwrap();
        let sync_file = root.path().join("sync-config.json");
        fs::write(
            &sync_file,
            serde_json::to_vec(&json!({
                "AGENTSCM_GITHUB_URL":format!("http://{}", env.host),
                "AGENTSCM_GITHUB_API_URL":fake.base,
                "AGENTSCM_GITHUB_PAT_FILE":pat_file,
            }))
            .unwrap(),
        )
        .unwrap();
        let sync_config = cold_restore::sync_config(root.path());
        let publication_state = agentscm::hub::HubState::new(
            env.store.clone(),
            Arc::new(
                agentscm::backend::LocalBackend::open(root.path().join("publication-objects"))
                    .unwrap(),
            ),
        )
        .with_workspace(env.workspace.clone())
        .with_workspace_execution(publication_service)
        .with_sync(agentscm::hub::sync::Sync::new(sync_config).unwrap());
        agentscm::hub::sync::start(&publication_state)
            .await
            .unwrap();
        let connection = env.store.pat_connection(&env.host).unwrap().unwrap();
        env.store
            .upsert_repository(
                &connection.id,
                &env.host,
                100,
                "synthetic/project",
                None,
                None,
            )
            .unwrap();
        env.store
            .set_connection_outbound(&connection.id, true, "synthetic")
            .unwrap();
        let publication_app = agentscm::hub::router(publication_state.clone());
        let publication_server = tokio::spawn(async move {
            axum::serve(listener, publication_app).await.unwrap();
        });
        let path = format!(
            "{publication_base}{REPO}/profiles/checks/attachments/{}/publication",
            state.id
        );
        let prepare = json!({
            "request_id":"prepare-imported-receipt",
            "expected_generation":state.generation,
            "subject":native.subject,
            "metadata":CommitMetadata { name:"Synthetic".into(), email:"synthetic@example.invalid".into(), timestamp:1780000001, message:"Prepare imported receipt".into() }
        });
        let first = json_request(env.client.post(&path).bearer_auth(&token), &prepare)
            .send()
            .await
            .unwrap();
        assert_eq!(
            first.status().as_u16(),
            200,
            "{}",
            first.text().await.unwrap()
        );
        let first: Value = serde_json::from_slice(&first.bytes().await.unwrap()).unwrap();
        assert_eq!(first["currentness"], "current_at_prepare");
        let encoded = first.to_string();
        for private in ["endpoint", "worktree", "credential", "ca_file"] {
            assert!(!encoded.contains(private), "{encoded}");
        }
        for private in [
            &state.worktree,
            &state.git_dir,
            &state.git_common_dir,
            &state.index,
        ] {
            assert!(!encoded.contains(private), "{encoded}");
        }
        let exact = json_request(env.client.post(&path).bearer_auth(&token), &prepare)
            .send()
            .await
            .unwrap();
        assert_eq!(exact.status().as_u16(), 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&exact.bytes().await.unwrap()).unwrap(),
            first
        );
        let mut changed = prepare.clone();
        changed["expected_generation"] = json!(state.generation + 1);
        assert_eq!(
            json_request(env.client.post(&path).bearer_auth(&token), &changed)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            409
        );
        let status_path = format!("{path}/status");
        let status_request = json!({"request_id":"prepare-imported-receipt"});
        let status = json_request(
            env.client.post(&status_path).bearer_auth(&token),
            &status_request,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(status.status().as_u16(), 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&status.bytes().await.unwrap()).unwrap()["candidate"],
            first["candidate"]
        );
        let branch_path = format!("{path}/branch");
        let branch = json!({
            "request_id":"publish-imported-branch",
            "prepare_request_id":"prepare-imported-receipt",
            "candidate_id":first["candidate"]["candidate_id"],
            "expected_generation":state.generation
        });
        let published = json_request(env.client.post(&branch_path).bearer_auth(&token), &branch)
            .send()
            .await
            .unwrap();
        let published_status = published.status().as_u16();
        let published_bytes = published.bytes().await.unwrap();
        assert_eq!(
            published_status,
            200,
            "{}",
            String::from_utf8_lossy(&published_bytes)
        );
        let published: Value = serde_json::from_slice(&published_bytes).unwrap();
        assert_eq!(published["state"], "published");
        assert_eq!(published["currentness"], "current");
        assert_eq!(
            git(
                &external.path().join("provider.git"),
                &["rev-parse", &format!("refs/heads/{}", state.output)]
            ),
            first["candidate"]["commit"].as_str().unwrap()
        );
        let repeated = json_request(env.client.post(&branch_path).bearer_auth(&token), &branch)
            .send()
            .await
            .unwrap();
        assert_eq!(repeated.status().as_u16(), 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&repeated.bytes().await.unwrap()).unwrap(),
            published
        );
        let forge_path = format!("{path}/forge");
        let forge_request = json!({
            "request_id":"publish-imported-pr", "prepare_request_id":prepare["request_id"],
            "branch_request_id":branch["request_id"], "candidate_id":first["candidate"]["candidate_id"],
            "expected_generation":state.generation,"title":"Imported candidate","body":"Synthetic independent human landing"
        });
        let provider = external.path().join("provider.git");
        let prepared_head = first["candidate"]["commit"].as_str().unwrap();
        let provider_base = git(&provider, &["rev-parse", "refs/heads/main"]);
        for (reference, replacement, original) in [
            (
                format!("refs/heads/{}", state.output),
                provider_base.as_str(),
                prepared_head,
            ),
            (
                "refs/heads/main".into(),
                prepared_head,
                provider_base.as_str(),
            ),
        ] {
            git(
                &provider,
                &["update-ref", &reference, replacement, original],
            );
            let response = json_request(
                env.client.post(&forge_path).bearer_auth(&token),
                &forge_request,
            )
            .send()
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let refused: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
            assert!(refused["error"].is_string(), "{refused}");
            assert!(fake.with(|s| s.repos[&100].pulls.is_empty()));
            git(
                &provider,
                &["update-ref", &reference, original, replacement],
            );
        }
        fake.with(|s| s.lose_next_pull_reply = true);
        let result = json_request(
            env.client.post(&forge_path).bearer_auth(&token),
            &forge_request,
        )
        .send()
        .await
        .unwrap();
        let code = result.status();
        let result: Value = serde_json::from_slice(&result.bytes().await.unwrap()).unwrap();
        assert_eq!(code.as_u16(), 200, "{result}");
        let publication_id = result["id"].as_str().unwrap();
        assert_eq!(
            env.store
                .publication("synthetic/project", publication_id)
                .unwrap()
                .unwrap()
                .branch_state,
            "pr_attempting"
        );
        let recovery = json!({"publication_id":publication_id});
        let hidden_pull = fake.with(|s| s.repos.get_mut(&100).unwrap().pulls.remove(&1).unwrap());
        for (url, request) in [
            (forge_path.clone(), &forge_request),
            (format!("{forge_path}/recover"), &recovery),
        ] {
            let response = json_request(env.client.post(url).bearer_auth(&token), request)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let unknown: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
            assert!(unknown["error"].is_string());
        }
        assert!(fake.with(|s| s.repos[&100].pulls.is_empty()));
        fake.with(|s| s.repos.get_mut(&100).unwrap().pulls.insert(1, hidden_pull));
        // Concurrent retries adopt the one exact PR; durable attempting forbids another create.
        let (a, b) = tokio::join!(
            json_request(
                env.client
                    .post(format!("{forge_path}/recover"))
                    .bearer_auth(&token),
                &recovery
            )
            .send(),
            json_request(
                env.client.post(&forge_path).bearer_auth(&token),
                &forge_request
            )
            .send()
        );
        for response in [a.unwrap(), b.unwrap()] {
            let code = response.status();
            let value: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
            assert_eq!(code.as_u16(), 200, "{value}");
            assert_eq!(
                value["states"]["pull_request"]["state"], "opened",
                "{value}"
            );
        }
        assert_eq!(
            fake.with(|s| s
                .requests
                .iter()
                .filter(|r| r.method == "POST" && r.path_and_query.ends_with("/pulls"))
                .count()),
            1
        );
        for suffix in ["run", "recover"] {
            assert_eq!(env.client.post(format!("{publication_base}/v1/hub/repos/synthetic%2Fproject/publications/{publication_id}/{suffix}")).bearer_auth(&token).send().await.unwrap().status().as_u16(), 409);
        }
        fake.with(|s| s.lose_next_status_reply = true);
        agentscm::hub::sync::poll_once(&publication_state)
            .await
            .unwrap();
        agentscm::hub::sync::poll_once(&publication_state)
            .await
            .unwrap();
        let statuses = fake.with(|s| s.repos[&100].statuses.clone());
        assert_eq!(
            statuses.last().unwrap().1["state"],
            "success",
            "{statuses:?}"
        );
        assert_eq!(statuses.last().unwrap().0, first["candidate"]["commit"]);

        if revocation == "cold" {
            // All production state above is real; this branch only changes the stop/restore boundary.
            cold_restore::browser_hold(&env.base).await;
            let retained_transfer = collector.retained_transfer("transfer-success").unwrap();
            let retained_attachments = {
                let (_, _, _, _, journal, a, b) = git_fixture.as_ref().unwrap();
                [journal.state(&a.id).unwrap(), journal.state(&b.id).unwrap()]
            };
            let mut parts = axum::http::Request::builder()
                .header(header::COOKIE, &owner)
                .body(())
                .unwrap()
                .into_parts()
                .0;
            let maintenance_actor =
                <agentscm::hub::workspace::WorkspaceActor as axum::extract::FromRequestParts<
                    agentscm::hub::HubState,
                >>::from_request_parts(&mut parts, &publication_state)
                .await
                .ok()
                .expect("authenticated maintenance actor");
            let token_id = env
                .store
                .authenticate(
                    &agentscm::hub::store::agent_token_digest(&token).unwrap(),
                    agentscm::hub::store::unix_now(),
                )
                .unwrap()
                .unwrap()
                .token_id;
            publication_server.abort();
            let stopped =
                tokio::time::timeout(std::time::Duration::from_secs(5), publication_server)
                    .await
                    .unwrap();
            assert!(stopped.is_ok() || stopped.unwrap_err().is_cancelled());
            fake.stop().await;
            drop(publication_state);
            drop(mirror);
            drop(collector);
            drop(service);
            drop(engine);
            drop(git_fixture);
            drop(no_http);
            drop(_socket);
            fs::remove_file(&docker_socket).unwrap();
            let storage = env.stop().await;
            cold_restore::qualify(
                &root,
                storage,
                &native,
                &collector_profile,
                &mut fake,
                maintenance_actor,
                token_id,
                &token,
                &current_member,
                &retained_transfer,
                &retained_attachments,
                &first,
                publication_id,
                &forge_request,
                &recovery,
                &docker_home,
            )
            .await;
            return;
        }

        if matches!(revocation, "executor" | "token") {
            if revocation == "executor" {
                let mut authority = Authority::open(&native.profile.data_dir).unwrap();
                let epoch = authority.policy_epoch().unwrap() + 1;
                authority
                    .revoke_workflow_executor(&agentscm::skill_workflow::ExecutorRevocation {
                        version: 1,
                        registration_id: "registration-http-1".into(),
                        repository: native.profile.native_repository.clone(),
                        registration_epoch: 1,
                        revoked_at_policy_epoch: epoch,
                        reason: "synthetic operator revocation".into(),
                    })
                    .unwrap();
            } else {
                let principal = env
                    .store
                    .authenticate(
                        &agentscm::hub::store::agent_token_digest(&token).unwrap(),
                        agentscm::hub::store::unix_now(),
                    )
                    .unwrap()
                    .unwrap();
                env.store.revoke_agent_token(&principal.token_id).unwrap();
            }
            let response = json_request(
                env.client.post(&forge_path).bearer_auth(&token),
                &forge_request,
            )
            .send()
            .await
            .unwrap();
            assert!(response.status().is_client_error(), "{revocation}");
            agentscm::hub::sync::poll_once(&publication_state)
                .await
                .unwrap();
            assert_eq!(
                fake.with(|s| s.repos[&100].statuses.last().unwrap().1["state"].clone()),
                "failure",
                "{revocation}"
            );
            let (_, observer) = mint_agent_token(
                &*env.store,
                &owner_user.id,
                "historical-observer",
                "synthetic/project",
                3600,
            )
            .unwrap();
            let response = json_request(
                env.client
                    .post(format!("{forge_path}/recover"))
                    .bearer_auth(observer.expose()),
                &recovery,
            )
            .send()
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            assert_eq!(
                fake.with(|s| s
                    .requests
                    .iter()
                    .filter(|r| r.method == "POST" && r.path_and_query.ends_with("/pulls"))
                    .count()),
                1
            );
            let (_, _, contributor_b, _, _, _, _) = git_fixture.as_ref().unwrap();
            assert_eq!(
                fs::read(contributor_b.join("notes/contributor-b.txt")).unwrap(),
                b"independent concurrent edit\n"
            );
            publication_server.abort();
            return;
        }
        let baseline = git(
            &external.path().join("provider.git"),
            &["rev-parse", "refs/heads/main"],
        );
        let head = first["candidate"]["commit"].as_str().unwrap();
        // A reviewer approves a specific head and baseline, never a branch name alone.
        let human_can_merge = |approved: bool, expected_head: &str, expected_base: &str| {
            fake.with(|s| {
                let repo = &s.repos[&100];
                let pull = &repo.pulls[&1];
                approved
                    && pull.head_sha == expected_head
                    && pull.base_ref == "main"
                    && git(
                        &external.path().join("provider.git"),
                        &["rev-parse", "refs/heads/main"],
                    ) == expected_base
                    && repo
                        .statuses
                        .iter()
                        .rev()
                        .find(|(sha, v)| {
                            sha == expected_head && v["context"] == "agentscm/workflow/main"
                        })
                        .is_some_and(|(_, v)| v["state"] == "success")
            })
        };
        assert!(!human_can_merge(false, head, &baseline));
        assert!(!human_can_merge(true, &"0".repeat(40), &baseline));
        assert!(!human_can_merge(true, head, &"0".repeat(40)));
        // Provider head/base drift produces a nonpassing status. Returning to the exact
        // qualified tuple permits a fresh success; old green status alone is insufficient.
        fake.with(|s| {
            s.repos
                .get_mut(&100)
                .unwrap()
                .pulls
                .get_mut(&1)
                .unwrap()
                .base_ref = "other".into()
        });
        agentscm::hub::sync::poll_once(&publication_state)
            .await
            .unwrap();
        assert!(!human_can_merge(true, head, &baseline));
        fake.with(|s| {
            s.repos
                .get_mut(&100)
                .unwrap()
                .pulls
                .get_mut(&1)
                .unwrap()
                .base_ref = "main".into()
        });
        agentscm::hub::sync::poll_once(&publication_state)
            .await
            .unwrap();
        assert!(human_can_merge(true, head, &baseline));
        if human_merge {
            // Explicit synthetic human action, outside every agent API and native authority.
            // The provider applies its exact-head, baseline and status guard before landing.
            git(
                &external.path().join("provider.git"),
                &["update-ref", "refs/heads/main", head, &baseline],
            );
            fake.with(|s| {
                let pull = s.repos.get_mut(&100).unwrap().pulls.get_mut(&1).unwrap();
                pull.merged = true;
                pull.state = "closed".into();
                pull.merge_commit_sha = Some(head.into());
            });
            agentscm::hub::sync::poll_once(&publication_state)
                .await
                .unwrap();
            assert_eq!(
                env.store
                    .publication("synthetic/project", publication_id)
                    .unwrap()
                    .unwrap()
                    .merge_commit
                    .as_deref(),
                Some(head)
            );
        }
        let (_, read_only, _) = mint_bound_agent_token(
            &*env.store,
            &owner_user.id,
            "publication-read-only",
            "synthetic/project",
            1,
            3600,
            true,
        )
        .unwrap();
        assert_eq!(
            json_request(
                env.client.post(&path).bearer_auth(read_only.expose()),
                &prepare
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
            403
        );
        let hub_owner = env
            .store
            .list_users()
            .unwrap()
            .into_iter()
            .find(|user| user.identity.user_id == 101)
            .unwrap();
        let (_, wrong_owner) = mint_agent_token(
            &*env.store,
            &hub_owner.id,
            "publication-wrong-owner",
            "synthetic/project",
            3600,
        )
        .unwrap();
        assert_eq!(
            json_request(
                env.client.post(&path).bearer_auth(wrong_owner.expose()),
                &prepare
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
            401
        );
        for secret in [read_only.expose(), wrong_owner.expose()] {
            let response = json_request(
                env.client.post(&forge_path).bearer_auth(secret),
                &forge_request,
            )
            .send()
            .await
            .unwrap();
            assert!(matches!(response.status().as_u16(), 401 | 403 | 409));
        }
        let (_, cross_repo) = mint_agent_token(
            &*env.store,
            &owner_user.id,
            "publication-cross-repo",
            "synthetic/other",
            3600,
        )
        .unwrap();
        assert!(
            json_request(
                env.client
                    .post(&forge_path)
                    .bearer_auth(cross_repo.expose()),
                &forge_request
            )
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
        );
        let mut wrong_generation = forge_request.clone();
        wrong_generation["expected_generation"] = json!(state.generation + 1);
        assert!(
            json_request(
                env.client.post(&forge_path).bearer_auth(&token),
                &wrong_generation
            )
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
        );
        let reader_only = env.member(
            202,
            &[Role::Reader],
            true,
            env.workspace.settings().unwrap().generation,
            "publication-remove-contributor",
        );
        assert_eq!(
            env.request(
                Method::PUT,
                "/v1/hub/workspace/members",
                Some(&owner),
                Some(owner_csrf),
                None,
                Some(reader_only)
            )
            .await
            .0,
            200
        );
        let mut new_request = prepare.clone();
        new_request["request_id"] = json!("prepare-without-contributor");
        let (_, reader_token) = mint_agent_token(
            &*env.store,
            &owner_user.id,
            "publication-reader-role",
            "synthetic/project",
            3600,
        )
        .unwrap();
        assert_eq!(
            json_request(
                env.client.post(&path).bearer_auth(reader_token.expose()),
                &new_request,
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
            409
        );
        if !human_merge {
            // A refused downgrade must retain the possibility of visible success.
            fake.with(|s| {
                s.fail.push_back(fake_github::Failure {
                    path_contains: "/statuses/".into(),
                    status: 403,
                    headers: vec![],
                })
            });
            agentscm::hub::sync::poll_once(&publication_state)
                .await
                .unwrap();
            let connected = env
                .store
                .repository_named(&env.host, "synthetic/project")
                .unwrap()
                .unwrap();
            let row = env
                .store
                .forge_check(&connected.id, 1, "agentscm/workflow/main")
                .unwrap()
                .unwrap();
            assert_eq!(row.phase, "attempting");
            agentscm::hub::sync::poll_once(&publication_state)
                .await
                .unwrap();
            assert_eq!(
                fake.with(|s| s.repos[&100].statuses.last().unwrap().1["state"].clone()),
                "failure"
            );
            assert!(!human_can_merge(true, head, &baseline));
        }
        assert!(
            json_request(
                env.client
                    .post(&forge_path)
                    .bearer_auth(reader_token.expose()),
                &forge_request
            )
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
        );
        let observed = json_request(
            env.client
                .post(format!("{forge_path}/recover"))
                .bearer_auth(reader_token.expose()),
            &recovery,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(observed.status().as_u16(), 200);
        let regrant = env.member(
            202,
            &[Role::Reader, Role::Contributor],
            true,
            env.workspace.settings().unwrap().generation,
            "publication-regrant-contributor",
        );
        assert_eq!(
            env.request(
                Method::PUT,
                "/v1/hub/workspace/members",
                Some(&owner),
                Some(owner_csrf),
                None,
                Some(regrant)
            )
            .await
            .0,
            200
        );
        let (_, fresh_token) = mint_agent_token(
            &*env.store,
            &owner_user.id,
            "publication-fresh-actor",
            "synthetic/project",
            3600,
        )
        .unwrap();
        new_request["request_id"] = json!("prepare-with-revoked-origin");
        assert_eq!(
            json_request(
                env.client.post(&path).bearer_auth(fresh_token.expose()),
                &new_request
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
            409
        );
        publication_server.abort();
        assert_attached_publication_boundary(&native, state);
        let (_, _, contributor_b, _, attachments, _, attached_b) = git_fixture.as_ref().unwrap();
        assert_eq!(
            fs::read(contributor_b.join("notes/contributor-b.txt")).unwrap(),
            b"independent concurrent edit\n"
        );
        let second = attachments.state(&attached_b.id).unwrap();
        assert_eq!(second.change_id.as_ref(), Some(&concurrent_context.change));
        assert_eq!(second.candidate_revision, concurrent_context.revision);
        assert_eq!(
            engine
                .read_file(&second.candidate_revision, "notes/contributor-b.txt")
                .unwrap(),
            b"independent concurrent edit\n"
        );
        assert!(
            Authority::open_read_only(&native.profile.data_dir)
                .unwrap()
                .target("main")
                .unwrap()
                .is_none()
        );
    } else {
        let integration = env
            .request(
                Method::POST,
                &integrate_path,
                Some(&integrator),
                Some(integrator_session["csrf_token"].as_str().unwrap()),
                None,
                Some(json!({
                    "request_id":"integrate-agent-owned-success",
                    "expected_generation":integrator_session["actor"]["generation"],
                    "subject":native.subject,
                    "approval_id":approval.1["approval_id"]
                })),
            )
            .await;
        assert_eq!(integration.0, 200, "{integration:?}");
        assert_eq!(
            Authority::open(&native.profile.data_dir)
                .unwrap()
                .target("main")
                .unwrap()
                .unwrap()
                .revision,
            native.context.revision
        );
    }

    let revoke_success_origin = env.member(
        202,
        &roles,
        false,
        env.workspace.settings().unwrap().generation,
        "revoke-successful-collector-origin",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(revoke_success_origin),
        )
        .await
        .0,
        200
    );
    let fresh_entrypoint = &fresh_task.entrypoint.references[0];
    let before_revoked_read = task_state_snapshot(&native.profile.data_dir);
    let revoked_reference = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_reference",
                json!({
                    "subject":native.subject,
                    "task_id":fresh_task.task_id,
                    "task_digest":fresh_task.digest().unwrap(),
                    "reference_name":fresh_entrypoint.name,
                    "expected_sha256":fresh_entrypoint.sha256,
                    "expected_schema_version":1
                }),
            )),
        )
        .await;
    assert!(
        matches!(revoked_reference.0, 401 | 403),
        "{revoked_reference:?}"
    );
    assert_eq!(
        task_state_snapshot(&native.profile.data_dir),
        before_revoked_read,
        "revoked reference read must not mutate native state"
    );
    let revoked_origin_approval = env
        .request(
            Method::POST,
            &approve_path,
            Some(&reviewer),
            Some(reviewer_session["csrf_token"].as_str().unwrap()),
            None,
            Some(json!({
                "request_id":"successful-receipt-revoked-origin",
                "expected_generation":reviewer_session["actor"]["generation"],
                "subject":native.subject
            })),
        )
        .await;
    assert_eq!(
        revoked_origin_approval.0, 409,
        "{revoked_origin_approval:?}"
    );
}

#[tokio::test]
async fn task_http_accepts_a_claimed_assertion_above_64_kib_and_rejects_over_route_bound() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, token, executor) = agent_client(root.path(), &native).await;
    let task_path = format!("{REPO}/profiles/checks/agent-tasks");
    let requirements = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"operation":"requirements","subject":native.subject})),
        )
        .await;
    assert_eq!(requirements.0, 200, "{requirements:?}");
    let task: WorkflowTask = serde_json::from_value(requirements.1["result"][0].clone()).unwrap();
    let claim = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"operation":"claim","subject":native.subject,"claim":claim_input(&task,&executor,"http-large")})),
        )
        .await;
    assert_eq!(claim.0, 200, "{claim:?}");
    let view: TaskAttemptView = serde_json::from_value(claim.1["result"].clone()).unwrap();
    let assertion = large_assertion_input(&task, &view, "result-http-large");
    let request = json!({"operation":"result","subject":native.subject,"assertion":assertion});
    assert!(request.to_string().len() > 64 * 1024);
    let result = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(request),
        )
        .await;
    assert_eq!(result.0, 200, "{result:?}");
    assert_eq!(result.1["result"]["state"], "finished");

    let over_bound = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"padding":"x".repeat(400 * 1024)})),
        )
        .await;
    assert_eq!(over_bound.0, 400, "{over_bound:?}");
}

#[tokio::test]
async fn workspace_mcp_accepts_real_mutation_and_large_result_and_rejects_over_route_bound() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, token, executor) = agent_client(root.path(), &native).await;
    let mcp_path = "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks";
    let requirements = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_requirements",
                json!({"subject":native.subject}),
            )),
        )
        .await;
    assert_eq!(requirements.0, 200, "{requirements:?}");
    let requirements = mcp_envelope(&requirements.1);
    let task: WorkflowTask = serde_json::from_value(requirements["result"][0].clone()).unwrap();
    let claim = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_claim",
                json!({"subject":native.subject,"claim":claim_input(&task,&executor,"mcp-large")}),
            )),
        )
        .await;
    assert_eq!(claim.0, 200, "{claim:?}");
    let claim = mcp_envelope(&claim.1);
    let view: TaskAttemptView = serde_json::from_value(claim["result"].clone()).unwrap();
    let heartbeat = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_heartbeat",
                json!({"subject":native.subject,"mutation":mutation_input(&view,"heartbeat-mcp-large","heartbeat")}),
            )),
        )
        .await;
    assert_eq!(heartbeat.0, 200, "{heartbeat:?}");
    assert_eq!(heartbeat.1["result"]["isError"], false);
    let heartbeat = mcp_envelope(&heartbeat.1);
    let view: TaskAttemptView = serde_json::from_value(heartbeat["result"].clone()).unwrap();
    let assertion = large_assertion_input(&task, &view, "result-mcp-large");
    let request = mcp_call(
        "agentscm_task_result",
        json!({"subject":native.subject,"assertion":assertion}),
    );
    assert!(request.to_string().len() > 64 * 1024);
    let result = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(request),
        )
        .await;
    assert_eq!(result.0, 200, "{result:?}");
    assert_eq!(result.1["result"]["isError"], false);
    assert_eq!(mcp_envelope(&result.1)["result"]["state"], "finished");

    let over_bound = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(json!({"padding":"x".repeat(400 * 1024)})),
        )
        .await;
    assert_ne!(over_bound.0, 200, "{over_bound:?}");
}

#[tokio::test]
async fn task_http_and_mcp_distinguish_invalid_claims_from_unavailable_journal() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, token, executor) = agent_client(root.path(), &native).await;
    let task_path = format!("{REPO}/profiles/checks/agent-tasks");
    let mcp_path = "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks";
    let requirements = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"operation":"requirements","subject":native.subject})),
        )
        .await;
    assert_eq!(requirements.0, 200, "{requirements:?}");
    let task: WorkflowTask = serde_json::from_value(requirements.1["result"][0].clone()).unwrap();
    let mut unknown = claim_input(&task, &executor, "unknown");
    unknown["task_id"] = json!("unknown-task");
    unknown["task_digest"] = json!("0".repeat(64));
    let invalid_http = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"operation":"claim","subject":native.subject,"claim":unknown.clone()})),
        )
        .await;
    assert_eq!(invalid_http, (400, json!({"error":"invalid_request"})));
    let invalid_mcp = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_claim",
                json!({"subject":native.subject,"claim":unknown}),
            )),
        )
        .await;
    assert_eq!(invalid_mcp.0, 200, "{invalid_mcp:?}");
    assert_eq!(invalid_mcp.1["result"]["isError"], true);
    assert_eq!(
        invalid_mcp.1["result"]["content"][0]["text"],
        "AgentSCM task request was invalid or not found"
    );

    std::fs::write(
        native.profile.data_dir.join("skill-workflows"),
        b"block journal directory creation",
    )
    .unwrap();
    let unavailable_http = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"operation":"claim","subject":native.subject,"claim":claim_input(&task,&executor,"unavailable-http")})),
        )
        .await;
    assert_eq!(unavailable_http, (503, json!({"error":"unavailable"})));
    let unavailable_mcp = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_claim",
                json!({"subject":native.subject,"claim":claim_input(&task,&executor,"unavailable-mcp")}),
            )),
        )
        .await;
    assert_eq!(unavailable_mcp.0, 200, "{unavailable_mcp:?}");
    assert_eq!(unavailable_mcp.1["error"]["code"], -32603);
    assert_eq!(
        unavailable_mcp.1["error"]["message"],
        "temporarily unavailable; inspect task status or recovery before retrying"
    );
    assert!(
        !unavailable_mcp
            .1
            .to_string()
            .contains(root.path().to_str().unwrap())
    );
}

fn start_request(native: &NativeSourceProfile, generation: &Value, id: &str) -> Value {
    json!({"request_id":id,"expected_generation":generation,"profile_id":"checks","subject":native.subject})
}

#[tokio::test]
async fn task_only_repository_exposes_authorized_empty_server_queue_without_enabling_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, service) = agent_only_env(root.path(), &native).await;
    assert!(!service.server_execution_enabled());
    let (_, owner) = env.signin(101).await;
    let session = env.session(&owner).await;
    let csrf = session["csrf_token"].as_str().unwrap();

    let repositories = env
        .request(
            Method::GET,
            "/v1/hub/workspace/repositories",
            Some(&owner),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(repositories.0, 200, "{repositories:?}");
    assert_eq!(
        repositories.1["repositories"][0]["repository_id"],
        "synthetic/project"
    );
    assert_eq!(
        repositories.1["repositories"][0]["profiles"][0]["server_execution"],
        false
    );
    assert_eq!(
        env.request(
            Method::GET,
            &format!("{REPO}/runs"),
            Some(&owner),
            None,
            None,
            None
        )
        .await,
        (200, json!({"runs":[],"next_cursor":null}))
    );
    for query in ["?limit=0", "?limit=51", "?before=0", "?before=not-a-number"] {
        assert_eq!(
            env.request(
                Method::GET,
                &format!("{REPO}/runs{query}"),
                Some(&owner),
                None,
                None,
                None
            )
            .await
            .0,
            400,
            "{query}"
        );
    }
    assert_eq!(
        env.request(Method::GET, &format!("{REPO}/runs"), None, None, None, None)
            .await
            .0,
        401
    );
    assert_eq!(
        env.request(
            Method::GET,
            "/v1/hub/workspace/repositories/unconfigured%2Frepository/runs",
            Some(&owner),
            None,
            None,
            None,
        )
        .await
        .0,
        401
    );
    assert_eq!(
        env.request(
            Method::POST,
            &format!("{REPO}/runs"),
            Some(&owner),
            Some(csrf),
            None,
            Some(start_request(
                &native,
                &session["actor"]["generation"],
                "task-only-refusal"
            )),
        )
        .await
        .0,
        401,
        "task-only profiles must never enter the server queue"
    );

    let grant = env.member(
        202,
        &[Role::Reader],
        true,
        env.workspace.settings().unwrap().generation,
        "task-only-reader",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(csrf),
            None,
            Some(grant)
        )
        .await
        .0,
        200
    );
    let (_, reader) = env.signin(202).await;
    assert_eq!(
        env.request(
            Method::GET,
            "/v1/hub/workspace/repositories/other%2Frepository/runs",
            Some(&reader),
            None,
            None,
            None,
        )
        .await
        .0,
        403
    );
    let revoke = env.member(
        202,
        &[Role::Reader],
        false,
        env.workspace.settings().unwrap().generation,
        "revoke-task-only-reader",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(csrf),
            None,
            Some(revoke)
        )
        .await
        .0,
        200
    );
    assert_eq!(
        env.request(
            Method::GET,
            &format!("{REPO}/runs"),
            Some(&reader),
            None,
            None,
            None
        )
        .await
        .0,
        401
    );
}

#[tokio::test]
async fn repository_profile_views_distinguish_server_and_task_only_execution() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, service) = mixed_execution_env(root.path(), &native).await;
    assert!(service.server_execution_enabled());
    let (_, owner) = env.signin(101).await;
    let repositories = env
        .request(
            Method::GET,
            "/v1/hub/workspace/repositories",
            Some(&owner),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(repositories.0, 200, "{repositories:?}");
    let profiles = repositories.1["repositories"][0]["profiles"]
        .as_array()
        .unwrap();
    assert_eq!(profiles.len(), 2);
    assert_eq!(profiles[0]["profile_id"], "agent-checks");
    assert_eq!(profiles[0]["server_execution"], false);
    assert_eq!(profiles[1]["profile_id"], "checks");
    assert_eq!(profiles[1]["server_execution"], true);
}

#[tokio::test]
async fn http_queue_is_exact_idempotent_scoped_and_cancellable_with_retained_read_projection() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, _service) = execution_env(root.path(), &native).await;
    let (_, cookie) = env.signin(101).await;
    let session = env.session(&cookie).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let generation = &session["actor"]["generation"];
    let repos = env
        .request(
            Method::GET,
            "/v1/hub/workspace/repositories",
            Some(&cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(repos.0, 200);
    assert_eq!(
        repos.1["repositories"][0]["repository_id"],
        "synthetic/project"
    );
    assert_eq!(
        repos.1["repositories"][0]["profiles"][0]["readiness"],
        "configured"
    );
    assert!(
        !repos.1.to_string().contains(root.path().to_str().unwrap()),
        "read views must not expose operator paths"
    );
    let subjects = env
        .request(
            Method::GET,
            &format!("{REPO}/profiles/checks/subjects?limit=1"),
            Some(&cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(
        subjects,
        (200, json!({"subjects":[native.subject],"next_cursor":null}))
    );
    let subject = env
        .request(
            Method::GET,
            &format!("{REPO}/profiles/checks/subject?change_id=work"),
            Some(&cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(subject, (200, json!({"subject":native.subject})));
    let path = format!("{REPO}/runs");
    let request = start_request(&native, generation, "queued-http");
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            None,
            None,
            Some(request.clone())
        )
        .await
        .0,
        403
    );
    for field in ["data_dir", "executable", "docker_socket", "provider_url"] {
        let mut unknown = request.clone();
        unknown[field] = json!("untrusted-request-value");
        assert_eq!(
            env.request(
                Method::POST,
                &path,
                Some(&cookie),
                Some(csrf),
                None,
                Some(unknown)
            )
            .await
            .0,
            400
        );
    }
    let mut unknown = request.clone();
    unknown["subject"]["policy_epoch"] = json!(0);
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(unknown)
        )
        .await
        .0,
        400
    );
    let mut stale = request.clone();
    stale["request_id"] = json!("stale-http");
    stale["subject"]["context_digest"] = json!("0".repeat(64));
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(stale)
        )
        .await
        .0,
        400
    );
    let mut stale = request.clone();
    stale["expected_generation"] = json!(0);
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(stale)
        )
        .await,
        (409, json!({"error":"generation_conflict"}))
    );
    let (status, run) = env
        .request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(request.clone()),
        )
        .await;
    assert_eq!(status, 200, "{run}");
    assert_eq!(run["state"], "queued");
    assert_eq!(run["receipt"], Value::Null);
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(request.clone())
        )
        .await,
        (200, run.clone())
    );
    let mut collision = request;
    collision["profile_id"] = json!("different-profile");
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(collision)
        )
        .await,
        (409, json!({"error":"idempotency_conflict"}))
    );
    let run_id = run["run_id"].as_str().unwrap();
    assert_eq!(
        env.request(
            Method::GET,
            &format!("{REPO}/runs/{run_id}"),
            Some(&cookie),
            None,
            None,
            None
        )
        .await,
        (200, run.clone())
    );
    assert_eq!(
        env.request(
            Method::GET,
            &format!("{REPO}/runs?limit=1"),
            Some(&cookie),
            None,
            None,
            None
        )
        .await,
        (200, json!({"runs":[run.clone()],"next_cursor":null}))
    );
    // Populate retained source-only native observations through the existing local
    // runtime. This is not queue dispatch or controller admission finalization.
    let native_run = run["native_run_id"].as_str().unwrap();
    skill_workflow::start(&native.profile.data_dir, native_run, &native.plan).unwrap();
    skill_workflow::advance(&native.profile.data_dir, native_run, 1).unwrap();
    let projection = env
        .request(
            Method::GET,
            &format!("{REPO}/runs/{run_id}/workflow"),
            Some(&cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(projection.0, 200, "{}", projection.1);
    assert_eq!(projection.1["availability"], "available");
    assert_eq!(projection.1["projection"]["run"]["state"], "succeeded");
    assert!(
        !projection
            .1
            .to_string()
            .contains(root.path().to_str().unwrap())
    );
    let cancel_path = format!("{REPO}/runs/{run_id}/cancel");
    let mut cancel = json!({"request_id":"cancel-http","expected_generation":0});
    assert_eq!(
        env.request(
            Method::POST,
            &cancel_path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(cancel.clone())
        )
        .await,
        (409, json!({"error":"generation_conflict"}))
    );
    cancel["expected_generation"] = run["generation"].clone();
    let (status, cancelled) = env
        .request(
            Method::POST,
            &cancel_path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(cancel.clone()),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(cancelled["state"], "cancelled");
    assert_eq!(cancelled["receipt"], Value::Null);
    assert_eq!(
        env.request(
            Method::POST,
            &cancel_path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(cancel.clone())
        )
        .await,
        (200, cancelled)
    );
    cancel["expected_generation"] = json!(999);
    assert_eq!(
        env.request(
            Method::POST,
            &cancel_path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(cancel)
        )
        .await,
        (409, json!({"error":"idempotency_conflict"}))
    );
    assert!(
        Authority::open(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn http_native_actions_require_explicit_human_roles_exact_subject_and_native_evidence() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, _service) = execution_env(root.path(), &native).await;
    let (_, owner) = env.signin(101).await;
    let owner_session = env.session(&owner).await;
    let owner_csrf = owner_session["csrf_token"].as_str().unwrap();
    let approve = format!("{REPO}/profiles/checks/approve");
    let integrate = format!("{REPO}/profiles/checks/integrate");
    let request = json!({"request_id":"approval-http","expected_generation":owner_session["actor"]["generation"],"subject":native.subject});
    // Administrator does not imply either independent native action role.
    assert_eq!(
        env.request(
            Method::POST,
            &approve,
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(request.clone())
        )
        .await
        .0,
        403
    );
    let mut integration = request.clone();
    integration["approval_id"] = json!("not-issued");
    assert_eq!(
        env.request(
            Method::POST,
            &integrate,
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(integration)
        )
        .await
        .0,
        403
    );
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
        "add-native-reviewer",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(grant)
        )
        .await
        .0,
        200
    );
    let (_, reviewer) = env.signin(202).await;
    let reviewer_session = env.session(&reviewer).await;
    let csrf = reviewer_session["csrf_token"].as_str().unwrap();
    let mut request = request;
    request["expected_generation"] = reviewer_session["actor"]["generation"].clone();
    let mut integration = request.clone();
    integration["approval_id"] = json!("not-issued");
    for (path, body) in [
        (&approve, request.clone()),
        (&integrate, integration.clone()),
    ] {
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reviewer),
                None,
                None,
                Some(body.clone())
            )
            .await
            .0,
            403
        );
        for field in ["actor_id", "policy_epoch", "data_dir"] {
            let mut unknown = body.clone();
            unknown[field] = json!("request-cannot-supply-this");
            assert_eq!(
                env.request(
                    Method::POST,
                    path,
                    Some(&reviewer),
                    Some(csrf),
                    None,
                    Some(unknown)
                )
                .await
                .0,
                400
            );
        }
        let mut stale = body.clone();
        stale["subject"]["change_version"] = json!(99);
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reviewer),
                Some(csrf),
                None,
                Some(stale)
            )
            .await
            .0,
            409
        );
        let mut stale = body;
        stale["expected_generation"] = json!(0);
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reviewer),
                Some(csrf),
                None,
                Some(stale)
            )
            .await,
            (409, json!({"error":"generation_conflict"}))
        );
    }
    let user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == 202)
        .unwrap();
    let (_, token) = mint_agent_token(
        &*env.store,
        &user.id,
        "synthetic-run-agent",
        "synthetic/project",
        3600,
    )
    .unwrap();
    let (_, read_token, _) = mint_bound_agent_token(
        &*env.store,
        &user.id,
        "synthetic-read-agent",
        "synthetic/project",
        42,
        3600,
        true,
    )
    .unwrap();
    for secret in [&token, &read_token] {
        assert_eq!(
            env.request(
                Method::POST,
                &approve,
                None,
                None,
                Some(secret.expose()),
                Some(request.clone())
            )
            .await
            .0,
            403
        );
        assert_eq!(
            env.request(
                Method::POST,
                &integrate,
                None,
                None,
                Some(secret.expose()),
                Some(integration.clone())
            )
            .await
            .0,
            403
        );
    }
    assert_eq!(
        env.request(
            Method::GET,
            &format!("{REPO}/profiles/checks/subject?change_id=work"),
            None,
            None,
            Some(read_token.expose()),
            None
        )
        .await
        .0,
        200
    );
    assert_eq!(
        env.request(
            Method::POST,
            &format!("{REPO}/runs"),
            None,
            None,
            Some(read_token.expose()),
            Some(start_request(
                &native,
                &reviewer_session["actor"]["generation"],
                "read-token-run"
            ))
        )
        .await
        .0,
        403
    );
    for suffix in ["runs", "profiles/checks/subjects", "runs/unknown/workflow"] {
        assert_eq!(
            env.request(
                Method::GET,
                &format!("/v1/hub/workspace/repositories/synthetic%2Fother/{suffix}"),
                Some(&reviewer),
                None,
                None,
                None
            )
            .await
            .0,
            403
        );
    }
    // Passing authentication and roles cannot bypass the existing native gate:
    // no controller-finalized receipt or approval exists for this exact subject.
    let approval = env
        .request(
            Method::POST,
            &approve,
            Some(&reviewer),
            Some(csrf),
            None,
            Some(request),
        )
        .await;
    assert_eq!(approval.0, 409, "{approval:?}");
    let landing = env
        .request(
            Method::POST,
            &integrate,
            Some(&reviewer),
            Some(csrf),
            None,
            Some(integration),
        )
        .await;
    assert_eq!(landing.0, 409, "{landing:?}");
    assert!(
        Authority::open(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    let audit = rusqlite::Connection::open(env.dir.path().join("workspace.sqlite")).unwrap();
    let failed: i64 = audit
        .query_row(
            "SELECT count(*) FROM workspace_audit WHERE kind='native_action_failed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(failed >= 2);
}

#[tokio::test]
async fn synthetic_browser_authorize_page_uses_exact_registered_loopback_callback() {
    let env = Env::new().await;
    let start = env
        .client
        .get(format!("{}/v1/hub/login/github?workspace=1", env.base))
        .send()
        .await
        .unwrap();
    let cookie = start.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let authorize = Url::parse(start.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let page = env.client.get(authorize.clone()).send().await.unwrap();
    assert_eq!(page.status().as_u16(), 200);
    let html = page.text().await.unwrap();
    assert!(
        html.contains("Sign in as synthetic user 101")
            && html.contains("Sign in as synthetic user 202")
    );
    let callback = html
        .split("href=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .replace("&amp;", "&");
    let selected = env
        .client
        .get(callback)
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(selected.status().as_u16(), 302);
    assert_eq!(selected.headers()[header::LOCATION], "/ui/#workspace");
    let mut wrong = authorize.clone();
    let params: Vec<_> = authorize
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    wrong.set_query(None);
    for (key, value) in params {
        wrong.query_pairs_mut().append_pair(
            &key,
            if key == "redirect_uri" {
                "https://external.example/callback"
            } else {
                &value
            },
        );
    }
    let refused = env.client.get(wrong).send().await.unwrap();
    assert_eq!(refused.status().as_u16(), 400);
    assert!(!refused.text().await.unwrap().contains("href="));
}

#[tokio::test]
async fn source_only_native_receipt_allows_explicit_http_approval_and_landing_after_current_role_check()
 {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let socket = std::os::unix::net::UnixListener::bind(&native.profile.docker_socket).unwrap();
    socket.set_nonblocking(true).unwrap();
    let (env, service) = execution_env(root.path(), &native).await;
    let (_, owner) = env.signin(101).await;
    let session = env.session(&owner).await;
    let owner_csrf = session["csrf_token"].as_str().unwrap();
    let roles = [Role::Reader, Role::Reviewer, Role::Integrator];
    let grant = env.member(
        202,
        &roles,
        true,
        env.workspace.settings().unwrap().generation,
        "grant-landing-roles",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(grant)
        )
        .await
        .0,
        200
    );
    let (_, reviewer) = env.signin(202).await;
    let reviewer_session = env.session(&reviewer).await;
    let reviewer_csrf = reviewer_session["csrf_token"].as_str().unwrap();
    // Process the real shared queue with a source-only plan. OciBackend only
    // validates these local paths; no workflow step invokes its executable or
    // the inert Unix listener, and there is no provider step.
    let queued = env
        .request(
            Method::POST,
            &format!("{REPO}/runs"),
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(start_request(
                &native,
                &session["actor"]["generation"],
                "source-only-native-http",
            )),
        )
        .await;
    assert_eq!(queued.0, 200, "{queued:?}");
    let run_id = queued.1["run_id"].as_str().unwrap();
    let executor = service.clone();
    assert!(
        tokio::task::spawn_blocking(move || executor.process_next())
            .await
            .unwrap()
            .unwrap()
    );
    let completed = env
        .request(
            Method::GET,
            &format!("{REPO}/runs/{run_id}"),
            Some(&owner),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(completed.0, 200);
    assert_eq!(completed.1["state"], "succeeded", "{}", completed.1);
    assert!(
        completed.1["receipt"]
            .as_str()
            .is_some_and(|receipt| !receipt.is_empty())
    );
    assert!(matches!(socket.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
    let approve_path = format!("{REPO}/profiles/checks/approve");
    let approval_request = json!({"request_id":"approve-source-http","expected_generation":reviewer_session["actor"]["generation"],"subject":native.subject});
    let approval = env
        .request(
            Method::POST,
            &approve_path,
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(approval_request.clone()),
        )
        .await;
    assert_eq!(approval.0, 200, "{approval:?}");
    assert_eq!(
        env.request(
            Method::POST,
            &approve_path,
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(approval_request.clone())
        )
        .await,
        approval
    );
    assert!(
        Authority::open(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none(),
        "Approval must not automatically land the revision"
    );
    // Idempotent approval retry acknowledges history without unrevoking it.
    let approval_id = approval.1["approval_id"].as_str().unwrap();
    Authority::open(&native.profile.data_dir)
        .unwrap()
        .revoke_approval(approval_id)
        .unwrap();
    assert_eq!(
        env.request(
            Method::POST,
            &approve_path,
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(approval_request.clone())
        )
        .await,
        approval
    );
    let revoked_landing = json!({"request_id":"revoked-approval-landing","expected_generation":reviewer_session["actor"]["generation"],"subject":native.subject,"approval_id":approval_id});
    assert_eq!(
        env.request(
            Method::POST,
            &format!("{REPO}/profiles/checks/integrate"),
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(revoked_landing)
        )
        .await
        .0,
        409
    );
    assert!(
        Authority::open(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    let mut fresh_approval = approval_request;
    fresh_approval["request_id"] = json!("approve-current-source-http");
    let approval = env
        .request(
            Method::POST,
            &approve_path,
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(fresh_approval.clone()),
        )
        .await;
    assert_eq!(approval.0, 200, "{approval:?}");
    let integrate_path = format!("{REPO}/profiles/checks/integrate");
    let mut integration = json!({"request_id":"land-source-http","expected_generation":reviewer_session["actor"]["generation"],"subject":native.subject,"approval_id":approval.1["approval_id"]});
    let revoke = env.member(
        202,
        &[Role::Reader, Role::Reviewer],
        true,
        env.workspace.settings().unwrap().generation,
        "remove-integrator",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(revoke)
        )
        .await
        .0,
        200
    );
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(integration.clone())
        )
        .await
        .0,
        401
    );
    let (_, limited) = env.signin(202).await;
    let limited_session = env.session(&limited).await;
    integration["expected_generation"] = limited_session["actor"]["generation"].clone();
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&limited),
            limited_session["csrf_token"].as_str(),
            None,
            Some(integration.clone())
        )
        .await
        .0,
        403
    );
    assert!(
        Authority::open(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    let restore = env.member(
        202,
        &roles,
        true,
        env.workspace.settings().unwrap().generation,
        "restore-integrator",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(restore)
        )
        .await
        .0,
        200
    );
    let (_, current) = env.signin(202).await;
    let current_session = env.session(&current).await;
    integration["expected_generation"] = current_session["actor"]["generation"].clone();
    let landed = env
        .request(
            Method::POST,
            &integrate_path,
            Some(&current),
            current_session["csrf_token"].as_str(),
            None,
            Some(integration.clone()),
        )
        .await;
    assert_eq!(landed.0, 200, "{landed:?}");
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&current),
            current_session["csrf_token"].as_str(),
            None,
            Some(integration.clone())
        )
        .await,
        landed
    );
    let original_integration = integration.clone();
    integration["approval_id"] = json!("different-approval");
    let collision = env
        .request(
            Method::POST,
            &integrate_path,
            Some(&current),
            current_session["csrf_token"].as_str(),
            None,
            Some(integration),
        )
        .await;
    assert_eq!(collision.0, 409, "{collision:?}");
    let target = Authority::open(&native.profile.data_dir)
        .unwrap()
        .target("main")
        .unwrap()
        .unwrap();
    assert_eq!(target.revision, native.subject.revision);
    assert_eq!(target.version, 1);
    let audit = rusqlite::Connection::open(env.dir.path().join("workspace.sqlite")).unwrap();
    let successful: i64 = audit
        .query_row(
            "SELECT count(*) FROM workspace_audit WHERE kind='native_action_succeeded'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(successful >= 2);
    // Retained native results survive a benign role regrant and fresh session;
    // generation remains a precondition only for a genuinely new mutation.
    let regrant = env.member(
        202,
        &roles,
        true,
        env.workspace.settings().unwrap().generation,
        "benign-regrant",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(regrant)
        )
        .await
        .0,
        200
    );
    let (_, reauthenticated) = env.signin(202).await;
    let reauthenticated_session = env.session(&reauthenticated).await;
    let reauthenticated_csrf = reauthenticated_session["csrf_token"].as_str();
    assert_eq!(
        env.request(
            Method::POST,
            &approve_path,
            Some(&reauthenticated),
            reauthenticated_csrf,
            None,
            Some(fresh_approval.clone())
        )
        .await,
        approval
    );
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&reauthenticated),
            reauthenticated_csrf,
            None,
            Some(original_integration.clone())
        )
        .await,
        landed
    );
    let mut changed = original_integration.clone();
    changed["approval_id"] = json!("changed-body-after-reauth");
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&reauthenticated),
            reauthenticated_csrf,
            None,
            Some(changed)
        )
        .await
        .0,
        409
    );
    for (path, mut body) in [
        (&approve_path, fresh_approval.clone()),
        (&integrate_path, original_integration.clone()),
    ] {
        body["request_id"] = json!("new-request-old-generation");
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reauthenticated),
                reauthenticated_csrf,
                None,
                Some(body)
            )
            .await,
            (409, json!({"error":"generation_conflict"}))
        );
    }
    // Diagnostic only: the public authority API keeps requirements immutable.
    // An offline direct DB edit simulates unsupported requirement removal so
    // recovery/history must not accidentally require today's installed profile.
    let native_db =
        rusqlite::Connection::open(native.profile.data_dir.join("state.sqlite")).unwrap();
    native_db
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    native_db
        .execute("DELETE FROM workflow_requirements WHERE target='main'", [])
        .unwrap();
    drop(native_db);
    let retained = env
        .request(
            Method::GET,
            &format!("{REPO}/runs/{run_id}/workflow"),
            Some(&reauthenticated),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(retained.0, 200, "{retained:?}");
    assert_eq!(
        retained.1["projection"]["run"]["source"]["revision"],
        native.subject.revision
    );
    assert_eq!(
        env.request(
            Method::POST,
            &approve_path,
            Some(&reauthenticated),
            reauthenticated_csrf,
            None,
            Some(fresh_approval.clone())
        )
        .await,
        approval
    );
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&reauthenticated),
            reauthenticated_csrf,
            None,
            Some(original_integration.clone())
        )
        .await,
        landed
    );
    for (path, mut body) in [
        (&approve_path, fresh_approval.clone()),
        (&integrate_path, original_integration.clone()),
    ] {
        body["request_id"] = json!("new-request-obsolete-profile");
        body["expected_generation"] = reauthenticated_session["actor"]["generation"].clone();
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reauthenticated),
                reauthenticated_csrf,
                None,
                Some(body)
            )
            .await
            .0,
            400
        );
    }
    let remove = env.member(
        202,
        &[Role::Reader],
        true,
        env.workspace.settings().unwrap().generation,
        "remove-native-roles",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(remove)
        )
        .await
        .0,
        200
    );
    let (_, reader) = env.signin(202).await;
    let reader_session = env.session(&reader).await;
    for (path, body) in [
        (&approve_path, fresh_approval),
        (&integrate_path, original_integration),
    ] {
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reader),
                reader_session["csrf_token"].as_str(),
                None,
                Some(body)
            )
            .await
            .0,
            403
        );
    }
}

#[tokio::test]
async fn exact_queue_retry_survives_membership_regrant_but_new_old_generation_request_fails() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, _service) = execution_env(root.path(), &native).await;
    let (_, owner) = env.signin(101).await;
    let owner_session = env.session(&owner).await;
    let owner_csrf = owner_session["csrf_token"].as_str();
    let grant = env.member(
        202,
        &[Role::Reader, Role::Contributor],
        true,
        env.workspace.settings().unwrap().generation,
        "queue-member",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            owner_csrf,
            None,
            Some(grant)
        )
        .await
        .0,
        200
    );
    let (_, member) = env.signin(202).await;
    let member_session = env.session(&member).await;
    let request = start_request(
        &native,
        &member_session["actor"]["generation"],
        "retained-queue-request",
    );
    let path = format!("{REPO}/runs");
    let retained = env
        .request(
            Method::POST,
            &path,
            Some(&member),
            member_session["csrf_token"].as_str(),
            None,
            Some(request.clone()),
        )
        .await;
    assert_eq!(retained.0, 200);
    let grant = env.member(
        202,
        &[Role::Reader, Role::Contributor, Role::Reviewer],
        true,
        env.workspace.settings().unwrap().generation,
        "queue-member-regrant",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            owner_csrf,
            None,
            Some(grant)
        )
        .await
        .0,
        200
    );
    let (_, member) = env.signin(202).await;
    let session = env.session(&member).await;
    let csrf = session["csrf_token"].as_str();
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&member),
            csrf,
            None,
            Some(request.clone())
        )
        .await,
        retained
    );
    let mut changed = request.clone();
    changed["subject"]["change_version"] = json!(99);
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&member),
            csrf,
            None,
            Some(changed)
        )
        .await,
        (409, json!({"error":"idempotency_conflict"}))
    );
    let mut new = request.clone();
    new["request_id"] = json!("new-queue-old-generation");
    assert_eq!(
        env.request(Method::POST, &path, Some(&member), csrf, None, Some(new))
            .await,
        (409, json!({"error":"generation_conflict"}))
    );
    let revoke = env.member(
        202,
        &[Role::Reader],
        true,
        env.workspace.settings().unwrap().generation,
        "remove-queue-role",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            owner_csrf,
            None,
            Some(revoke)
        )
        .await
        .0,
        200
    );
    let (_, reader) = env.signin(202).await;
    let reader_session = env.session(&reader).await;
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&reader),
            reader_session["csrf_token"].as_str(),
            None,
            Some(request)
        )
        .await
        .0,
        403
    );
}

/// SYNTHETIC protocol client, not a live editor test: a hand-written JSON-RPC client that
/// speaks plain MCP over HTTP with no vendor hook and discovers the common task API.
#[tokio::test]
async fn synthetic_mcp_protocol_client_discovers_and_reads_tasks_without_a_vendor_hook() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, _, _) = agent_client(root.path(), &native).await;
    let user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == 202)
        .unwrap();
    let (token_id, secret) = mint_agent_token(
        &*env.store,
        &user.id,
        "synthetic-protocol-client",
        "synthetic/project",
        3600,
    )
    .unwrap();
    let token = secret.expose().to_owned();
    let rpc = |body: Value, token: String| {
        let env = &env;
        async move {
            env.request(Method::POST, MCP_PATH, None, None, Some(&token), Some(body))
                .await
        }
    };
    let call = |name: &str, arguments: Value| mcp_call(name, arguments);

    // Handshake: initialize, the initialized notification (202, no body), then tools/list.
    let init = rpc(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"synthetic-protocol-client","version":"0"}}}),
        token.clone(),
    )
    .await;
    assert_eq!(init.0, 200, "{init:?}");
    assert_eq!(init.1["result"]["protocolVersion"], "2025-06-18");
    let initialized = rpc(
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        token.clone(),
    )
    .await;
    assert_eq!(initialized.0, 202);
    let tools = rpc(
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        token.clone(),
    )
    .await;
    let names: Vec<&str> = tools.1["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for required in ["agentscm_task_capabilities", "agentscm_task_qualification"] {
        assert!(names.contains(&required), "{required} in {names:?}");
    }

    let capabilities = rpc(call("agentscm_task_capabilities", json!({})), token.clone()).await;
    assert!(
        mcp_envelope(&capabilities.1)["result"]["supported_schema_versions"]
            .as_array()
            .unwrap()
            .contains(&json!(1))
    );

    let subject = json!(native.subject);
    let requirements = rpc(
        call("agentscm_task_requirements", json!({"subject":subject})),
        token.clone(),
    )
    .await;
    let tasks = mcp_envelope(&requirements.1)["result"].clone();
    let task: WorkflowTask = serde_json::from_value(tasks[0].clone()).unwrap();
    let next = rpc(
        call("agentscm_task_next", json!({"subject":subject})),
        token.clone(),
    )
    .await;
    assert_eq!(mcp_envelope(&next.1)["result"]["task_id"], task.task_id);
    let task_digest = task.digest().unwrap();
    let instructions = rpc(
        call(
            "agentscm_task_instructions",
            json!({"subject":subject,"task_id":task.task_id,"task_digest":task_digest,
                "supported_schema_versions":[1]}),
        ),
        token.clone(),
    )
    .await;
    assert_eq!(
        mcp_envelope(&instructions.1)["result"]["status"],
        "available"
    );
    let entrypoint = &task.entrypoint.references[0];
    let reference = |version: u32| {
        json!({"subject":subject,"task_id":task.task_id,"task_digest":task_digest,
            "reference_name":entrypoint.name,"expected_sha256":entrypoint.sha256,
            "expected_schema_version":version})
    };
    let fetched = rpc(call("agentscm_task_reference", reference(1)), token.clone()).await;
    let fetched = mcp_envelope(&fetched.1)["result"].clone();
    assert_eq!(fetched["status"], "available", "{fetched}");
    assert_eq!(fetched["sha256"], entrypoint.sha256);
    assert!(fetched["content"].as_str().unwrap().len() <= 128 * 1024);
    let mismatch = rpc(
        call("agentscm_task_reference", reference(99)),
        token.clone(),
    )
    .await;
    let mismatch = mcp_envelope(&mismatch.1)["result"].clone();
    assert_eq!(mismatch["status"], "schema_mismatch");
    assert_eq!(mismatch["server_schema_versions"], json!([1]));
    assert!(mismatch["action"].is_string());

    let report = qualification(&env, &token, &native.profile.data_dir, &native.subject).await;
    assert_eq!(report["qualified"], false);
    assert!(report["next"]["operation"].is_string());
    assert!(
        report["next"]
            .as_object()
            .unwrap()
            .keys()
            .all(|key| !key.contains("command") && !key.contains("shell")),
        "next must be typed data, not a shell command: {}",
        report["next"]
    );

    // Fail closed after revocation: the MCP call is refused and the Stop hook blocks.
    let token_file = root.path().join("agent.token");
    fs::write(&token_file, &token).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(env.store.revoke_agent_token(&token_id).unwrap());
    let refused = rpc(call("agentscm_task_capabilities", json!({})), token.clone()).await;
    assert!(matches!(refused.0, 401 | 403), "{refused:?}");
    let change = native.subject.change_id.clone();
    let workspace = stop_workspace(
        &native.profile.data_dir,
        &change,
        &native.subject.revision,
        &root.path().join("stop-workspace"),
    );
    let blocked = task_codex_stop(
        &native.profile.data_dir,
        &env.base,
        &token_file,
        &workspace,
        &change,
        false,
    );
    assert_eq!(blocked["decision"], "block", "{blocked}");
}
