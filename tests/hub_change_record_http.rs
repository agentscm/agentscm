//! Authenticated HTTP projection for one concise change record.
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::change_capture::MIN_ACTION_CAPTURE_RECORDS;
use agentscm::change_record::{
    ActionFinishV1, ActionKindV1, ActionOutcomeV1, ActionStartV1, AgentHarnessV1, ArtifactMediaV1,
    ArtifactRefV1, AssuranceBasisV1, CaptureBatchV1, CaptureModeV1, CapturedEventKindV1,
    CapturedEventV1, CheckResultV1, ContentAvailabilityV1, CoverageV1, DriftV1, RecipeProvenanceV1,
    RetentionClassV1, SessionIdentityV1, SetupObservationV1, SourceBindingV1,
};
use agentscm::hub::session::SessionCall;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token, unix_now};
use agentscm::hub::work::{CheckpointKind, SourceRef, WorkCheckpoint, WorkRole, WorkStart};
use agentscm::hub::{HubState, router};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPOSITORY: &str = "acme/widgets";
const REPOSITORY_PATH: &str = "acme%2Fwidgets";

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    _directory: TempDir,
}

struct RestartableHub {
    base: String,
    address: std::net::SocketAddr,
    store: Arc<dyn Store>,
    objects: Arc<LocalBackend>,
    task: Option<tokio::task::JoinHandle<()>>,
    _directory: TempDir,
}

impl RestartableHub {
    async fn start() -> Self {
        let directory = TempDir::new().unwrap();
        let database = format!("sqlite://{}", directory.path().join("hub.sqlite").display());
        let store = store::open(&database).unwrap();
        let objects = Arc::new(LocalBackend::open(directory.path().join("objects")).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = Some(Self::serve(listener, store.clone(), objects.clone()));
        Self {
            base: format!("http://{address}"),
            address,
            store,
            objects,
            task,
            _directory: directory,
        }
    }

    fn serve(
        listener: tokio::net::TcpListener,
        store: Arc<dyn Store>,
        objects: Arc<LocalBackend>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            axum::serve(listener, router(HubState::new(store, objects)))
                .await
                .unwrap();
        })
    }

    async fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }

    async fn restart(&mut self) {
        let listener = tokio::net::TcpListener::bind(self.address).await.unwrap();
        self.task = Some(Self::serve(
            listener,
            self.store.clone(),
            self.objects.clone(),
        ));
    }
}

fn run_git(root: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_capture_cli(
    config: &std::path::Path,
    home: &std::path::Path,
    args: &[String],
    stdin: Option<&[u8]>,
) -> std::process::Output {
    let mut command = capture_cli_command(config, home, args);
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(stdin) = stdin {
        child.stdin.take().unwrap().write_all(stdin).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn capture_cli_command(
    config: &std::path::Path,
    home: &std::path::Path,
    args: &[String],
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
    command
        .args(args)
        .env_clear()
        .env("XDG_CONFIG_HOME", config)
        .env("HOME", home)
        .env("NO_PROXY", "127.0.0.1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    command
}

impl Hub {
    async fn start() -> Self {
        let directory = TempDir::new().unwrap();
        let database = format!("sqlite://{}", directory.path().join("hub.sqlite").display());
        let store = store::open(&database).unwrap();
        let objects = LocalBackend::open(directory.path().join("objects")).unwrap();
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            base,
            store,
            client: reqwest::Client::new(),
            _directory: directory,
        }
    }

    async fn get(&self, path: &str, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = self.client.get(format!("{}{path}", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let body = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        (status, body)
    }
}

fn setup_batch(work_id: &str, generation: i64, session_id: &str) -> CaptureBatchV1 {
    CaptureBatchV1 {
        schema_version: 1,
        operation_id: "http-setup".into(),
        work_id: work_id.into(),
        generation,
        session: SessionIdentityV1 {
            harness: AgentHarnessV1::ClaudeCode,
            adapter_version: "1.0.0".into(),
            session_id: session_id.into(),
            parent_session_id: None,
            capture_mode: CaptureModeV1::LocalConversation,
        },
        events: vec![CapturedEventV1 {
            event_id: "http-setup-event".into(),
            sequence: 1,
            observed_at: 100,
            kind: CapturedEventKindV1::SessionStarted,
            tool: None,
            artifact: None,
            setup: Some(SetupObservationV1 {
                provider: None,
                model: Some("model-a".into()),
                permission_mode: None,
                effort: Some("high".into()),
                artifact: ArtifactRefV1 {
                    digest: "7".repeat(64),
                    size_bytes: 10,
                    media: ArtifactMediaV1::Json,
                    retention: RetentionClassV1::WorkLocal,
                    availability: ContentAvailabilityV1::Local,
                },
                unknown_components: vec![],
            }),
            gap: None,
            boundary: None,
            subagent: None,
        }],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_setups_reach_the_projection_and_are_trimmed_first() {
    let hub = Hub::start().await;
    let owner = hub
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 3,
            },
            "writer",
        )
        .unwrap();
    let (token, secret, _) =
        mint_bound_agent_token(&*hub.store, &owner.id, "writer", REPOSITORY, 78, 600, false)
            .unwrap();
    let start = |scope: Vec<String>| WorkStart {
        work_id: None,
        source: None,
        request_key: Some("http-setups".into()),
        title: "show session setups".into(),
        session: "http-setups".into(),
        role: WorkRole::Lead,
        scope,
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
    };
    let work = hub
        .store
        .work_start(&token, &start(vec!["src/**".into()]), unix_now())
        .unwrap()
        .item;
    let path = format!("/v1/hub/repos/{REPOSITORY_PATH}/change-records/{}", work.id);
    let secret = secret.expose().to_owned();
    let (_, before) = hub.get(&path, Some(&secret)).await;
    assert!(before["summary"].get("session_setups").is_none());

    hub.store
        .change_ingest(
            &token,
            &setup_batch(&work.id, work.generation, "http-setups"),
            101,
        )
        .unwrap();
    let (status, body) = hub.get(&path, Some(&secret)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["summary"]["session_setups"],
        json!([{"session_id": "http-setups", "parent_session_id": null,
            "model": "model-a", "permission_mode": null, "effort": "high"}])
    );
    assert!(body["summary"].get("session_setups_omitted").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_token_gets_exact_projection_and_failures_are_explicit() {
    let hub = Hub::start().await;
    let owner = hub
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 1,
            },
            "writer",
        )
        .unwrap();
    let (writer_token, writer_secret, _) =
        mint_bound_agent_token(&*hub.store, &owner.id, "writer", REPOSITORY, 77, 600, false)
            .unwrap();
    let work = hub
        .store
        .work_start(
            &writer_token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("http-change-record".into()),
                title: "prepare a concise handoff".into(),
                session: "http-change-record".into(),
                role: WorkRole::Lead,
                scope: (0..256)
                    .map(|index| format!("src/{index:03}{}.rs", "\"".repeat(1000)))
                    .collect(),
                branch: None,
                worktree: None,
                next_action: Some("Request review of the retained receipt.".into()),
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
            },
            unix_now(),
        )
        .unwrap()
        .item;
    hub.store
        .work_checkpoint(
            &writer_token,
            &WorkCheckpoint {
                work_id: work.id.clone(),
                operation_id: "http-decision".into(),
                generation: work.generation,
                session: "http-change-record".into(),
                expected_updated_seq: Some(work.updated_seq),
                kind: CheckpointKind::Decision,
                summary: "Use this retained receipt for review.".into(),
                next_action: Some("Request review of the retained receipt.".into()),
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: (0..100)
                    .map(|index| SourceRef::RepositoryPath {
                        path: format!("src/ref-{index:03}{}.rs", "\"".repeat(990)),
                        line: Some(1),
                    })
                    .collect(),
                depends_on: vec![],
                related_work: vec![],
            },
            unix_now(),
        )
        .unwrap();
    let current_path = format!(
        "/v1/hub/repos/{REPOSITORY_PATH}/change-records/current?session_id=http-change-record"
    );
    let writer_secret = writer_secret.expose().to_owned();
    let (target_status, target) = hub.get(&current_path, Some(&writer_secret)).await;
    assert_eq!(target_status, StatusCode::OK, "{target}");
    assert_eq!(target["work_id"], work.id);
    assert_eq!(target["generation"], work.generation);
    assert_eq!(target["session_id"], "http-change-record");

    let start = ActionStartV1 {
        schema_version: 1,
        operation_id: "http-action-start".into(),
        work_id: work.id.clone(),
        generation: work.generation,
        session_id: "http-change-record".into(),
        action_id: "http-action".into(),
        source: SourceBindingV1::Git {
            repository: REPOSITORY.into(),
            worktree_id: "synthetic-worktree".into(),
            head: "1".repeat(40),
            tree: "2".repeat(40),
            base: "3".repeat(40),
            dirty: false,
            untracked_count: 0,
            omissions: vec![],
            captured_snapshot: None,
            snapshot_coverage: CoverageV1::Complete,
        },
        kind: ActionKindV1::Verification,
        recipe_provenance: RecipeProvenanceV1::ConfiguredExactMatch,
        recipe_digest: "4".repeat(64),
        setup_digest: "5".repeat(64),
        observed_order: 1,
        observation_scope_id: Some("scope-http".into()),
        started_at: 100,
    };
    hub.store
        .change_action_start(&writer_token, &start, 101)
        .unwrap();
    hub.store
        .change_action_finish(
            &writer_token,
            &ActionFinishV1 {
                schema_version: 1,
                operation_id: "http-action-finish".into(),
                work_id: work.id.clone(),
                generation: work.generation,
                action_id: "http-action".into(),
                outcome: ActionOutcomeV1::Succeeded,
                result: CheckResultV1::Passed,
                assurance: AssuranceBasisV1::CallerAsserted,
                coverage: CoverageV1::Partial,
                finished_at: 120,
                drift: DriftV1::NoneObserved,
                output_artifacts: vec![ArtifactRefV1 {
                    digest: "6".repeat(64),
                    size_bytes: 20,
                    media: ArtifactMediaV1::Text,
                    retention: RetentionClassV1::WorkLocal,
                    availability: ContentAvailabilityV1::Local,
                }],
            },
            121,
        )
        .unwrap();

    hub.store
        .change_ingest(
            &writer_token,
            &setup_batch(&work.id, work.generation, "http-change-record"),
            122,
        )
        .unwrap();
    let reader_owner = hub
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 2,
            },
            "reader",
        )
        .unwrap();
    let (_, reader, _) = mint_bound_agent_token(
        &*hub.store,
        &reader_owner.id,
        "reader",
        REPOSITORY,
        77,
        600,
        true,
    )
    .unwrap();
    let reader = reader.expose().to_owned();
    let path = format!("/v1/hub/repos/{REPOSITORY_PATH}/change-records/{}", work.id);
    let (status, response) = hub.get(&path, Some(&reader)).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["version"], 1);
    assert_eq!(response["availability"], "available");
    let summary = &response["summary"];
    assert_eq!(summary["work_id"], work.id);
    assert_eq!(summary["latest_action"]["result"], "passed");
    assert_eq!(summary["latest_action"]["applicability"], "unknown");
    assert_eq!(summary["latest_action"]["assurance"], "caller_asserted");
    assert_eq!(summary["latest_action"]["evidence_availability"], "local");
    assert_eq!(summary["latest_action"]["coverage"], "partial");
    assert_eq!(summary["decision"]["status"], "proposed");
    assert_eq!(summary["decision"]["author_kind"], "agent");
    assert_eq!(
        summary["decision"]["source_refs"][0]["kind"],
        "repository_path"
    );
    assert_eq!(summary["intent"]["title"], "prepare a concise handoff");
    assert!(summary["intent"]["scope"].as_array().unwrap().len() <= 16);
    assert!(summary["intent"]["scope_omitted"].as_u64().unwrap() >= 240);
    assert!(summary["decision"]["source_refs"].as_array().unwrap().len() <= 16);
    assert!(summary["decision"]["source_refs_omitted"].as_u64().unwrap() >= 84);
    assert_eq!(summary["subject"]["source"]["kind"], "git");
    assert_eq!(
        summary["next_action"],
        "Request review of the retained receipt."
    );
    // The response is full, so the optional session setup is the first thing dropped.
    assert!(summary.get("session_setups").is_none());
    assert_eq!(summary["session_setups_omitted"], 1);
    assert!(!response.to_string().contains("transcript"));
    assert!(serde_json::to_vec(&response).unwrap().len() <= 48 * 1024);

    let missing = format!(
        "/v1/hub/repos/{REPOSITORY_PATH}/change-records/wrk_{}",
        "f".repeat(32)
    );
    let (status, body) = hub.get(&missing, Some(&reader)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body, json!({"error":"not_found"}));

    let wrong_repo = path.replace(REPOSITORY_PATH, "acme%2Fother");
    assert_eq!(
        hub.get(&wrong_repo, Some(&reader)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(hub.get(&path, None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn minimum_configured_spool_completes_one_managed_recipe() {
    let hub = Hub::start().await;
    let owner = hub
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 40,
            },
            "minimum-writer",
        )
        .unwrap();
    let (writer_token, writer_secret, _) = mint_bound_agent_token(
        &*hub.store,
        &owner.id,
        "minimum-writer",
        REPOSITORY,
        77,
        600,
        false,
    )
    .unwrap();
    let session = "minimum-managed-session";
    let work = hub
        .store
        .work_start(
            &writer_token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("minimum-managed-spool".into()),
                title: "prove the minimum configured capture capacity".into(),
                session: session.into(),
                role: WorkRole::Lead,
                scope: vec!["tracked.txt".into()],
                branch: None,
                worktree: None,
                next_action: Some("Inspect the minimum-capacity receipt.".into()),
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
            },
            unix_now(),
        )
        .unwrap()
        .item;

    let private = TempDir::new().unwrap();
    let root = private.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    run_git(&root, &["init", "--quiet", "--initial-branch=main"]);
    run_git(&root, &["config", "user.email", "fixture@example.invalid"]);
    run_git(&root, &["config", "user.name", "Synthetic Fixture"]);
    std::fs::write(root.join("tracked.txt"), "fixture\n").unwrap();
    run_git(&root, &["add", "tracked.txt"]);
    run_git(&root, &["commit", "--quiet", "-m", "fixture"]);
    run_git(
        &root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    run_git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let root = root.canonicalize().unwrap();

    let config = private.path().join("config");
    let config_agentscm = config.join("agentscm");
    std::fs::create_dir_all(&config_agentscm).unwrap();
    let token_file = private.path().join("minimum-writer.token");
    std::fs::write(&token_file, format!("{}\n", writer_secret.expose())).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        config_agentscm.join("connections.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "connections": [{
                "root": root,
                "origin_host": "github.com",
                "hub": hub.base,
                "repo": REPOSITORY,
                "token_file": token_file,
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let spool = private.path().join("minimum-spool");
    let recipe = private.path().join("minimum-recipe.json");
    std::fs::write(
        &recipe,
        serde_json::to_vec(&json!({
            "id": "minimum-test",
            "argv": ["/usr/bin/true"],
            "coverage": "complete",
            "timeout_seconds": 10,
            "max_output_bytes": 4096,
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&recipe, std::fs::Permissions::from_mode(0o600)).unwrap();
    let configure = vec![
        "change-capture".into(),
        "configure".into(),
        "--root".into(),
        root.display().to_string(),
        "--spool".into(),
        spool.display().to_string(),
        "--worktree-id".into(),
        "minimum-worktree".into(),
        "--setup-digest".into(),
        "5".repeat(64),
        "--action-rule".into(),
        "Bash=execution:partial:process_exit_code".into(),
        "--recipe-file".into(),
        recipe.display().to_string(),
        "--max-spool-records".into(),
        MIN_ACTION_CAPTURE_RECORDS.to_string(),
        "--max-spool-bytes".into(),
        (64 * 1024).to_string(),
        "--max-artifact-bytes".into(),
        "4096".into(),
    ];
    let configured = run_capture_cli(&config, private.path(), &configure, None);
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );

    let hook = |payload: Value| {
        run_capture_cli(
            &config,
            private.path(),
            &["change-capture".into(), "hook-claude".into()],
            Some(&serde_json::to_vec(&payload).unwrap()),
        )
    };
    let started = hook(json!({
        "hook_event_name": "SessionStart",
        "session_id": session,
        "source": "startup",
        "timestamp": "2026-10-24T10:00:00Z",
        "cwd": root,
    }));
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let hook_output: Value = serde_json::from_slice(&started.stdout).unwrap();
    let context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    let managed_command = context
        .lines()
        .find_map(|line| line.strip_prefix("Recipe minimum-test: "))
        .expect("missing minimum managed invocation")
        .to_owned();
    let tool_use = "minimum-managed-tool";
    let pre = hook(json!({
        "hook_event_name": "PreToolUse",
        "session_id": session,
        "tool_name": "Bash",
        "tool_use_id": tool_use,
        "tool_input": {"command": managed_command.clone()},
        "cwd": root,
    }));
    assert!(
        pre.status.success(),
        "{}",
        String::from_utf8_lossy(&pre.stderr)
    );
    let mut command = capture_cli_command(
        &config,
        private.path(),
        &[
            "change-capture".into(),
            "run-recipe".into(),
            "--id".into(),
            "minimum-test".into(),
        ],
    );
    let receipt = command
        .current_dir(&root)
        .env("CLAUDE_CODE_CHILD_SESSION", "1")
        .env("CLAUDE_CODE_SESSION_ID", session)
        .output()
        .unwrap();
    assert!(
        receipt.status.success(),
        "{}",
        String::from_utf8_lossy(&receipt.stderr)
    );
    let receipt: Value = serde_json::from_slice(&receipt.stdout).unwrap();
    assert_eq!(receipt["result"], "passed");
    let post = hook(json!({
        "hook_event_name": "PostToolUse",
        "session_id": session,
        "tool_name": "Bash",
        "tool_use_id": tool_use,
        "tool_input": {"command": managed_command.clone()},
        "tool_response": {"exit_code": 0},
        "cwd": root,
    }));
    assert!(
        post.status.success(),
        "{}",
        String::from_utf8_lossy(&post.stderr)
    );
    let latest = hub
        .store
        .change_record(&writer_token, &work.id, unix_now())
        .unwrap()
        .unwrap()
        .latest_action
        .unwrap();
    assert_eq!(latest.result, CheckResultV1::Passed);
    assert_eq!(latest.kind, ActionKindV1::Verification);
    assert_eq!(latest.assurance, AssuranceBasisV1::CallerAsserted);
    let original_action_id = latest.action_id.clone();

    let unconfigured = run_capture_cli(
        &config,
        private.path(),
        &[
            "change-capture".into(),
            "unconfigure".into(),
            "--root".into(),
            root.display().to_string(),
        ],
        None,
    );
    assert!(
        unconfigured.status.success(),
        "{}",
        String::from_utf8_lossy(&unconfigured.stderr)
    );
    let status = run_capture_cli(
        &config,
        private.path(),
        &[
            "change-capture".into(),
            "status".into(),
            "--root".into(),
            root.display().to_string(),
        ],
        None,
    );
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["state"], "disabled");
    assert_eq!(status["identity_matches"], true);
    assert_eq!(status["resumable_from_copy"], false);

    let moved_spool = private.path().join("minimum-spool-moved");
    std::fs::rename(&spool, &moved_spool).unwrap();
    std::fs::create_dir(&spool).unwrap();
    let replaced_path = run_capture_cli(&config, private.path(), &configure, None);
    assert!(!replaced_path.status.success());
    assert_eq!(
        std::fs::read_dir(&spool).unwrap().count(),
        0,
        "refused reconfiguration mutated the replacement inode"
    );

    let mut updated = configure.clone();
    let spool_index = updated.iter().position(|part| part == "--spool").unwrap() + 1;
    updated[spool_index] = moved_spool.display().to_string();
    let setup_index = updated
        .iter()
        .position(|part| part == "--setup-digest")
        .unwrap()
        + 1;
    updated[setup_index] = "6".repeat(64);
    let rule_index = updated
        .iter()
        .position(|part| part == "--action-rule")
        .unwrap()
        + 1;
    updated[rule_index] = "Write=execution:unknown:unverified".into();
    let records_index = updated
        .iter()
        .position(|part| part == "--max-spool-records")
        .unwrap()
        + 1;
    updated[records_index] = (MIN_ACTION_CAPTURE_RECORDS + 1).to_string();
    let reconfigured = run_capture_cli(&config, private.path(), &updated, None);
    assert!(
        reconfigured.status.success(),
        "{}",
        String::from_utf8_lossy(&reconfigured.stderr)
    );

    for payload in [
        json!({
            "hook_event_name": "PreToolUse",
            "session_id": session,
            "tool_name": "Bash",
            "tool_use_id": tool_use,
            "tool_input": {"command": managed_command.clone()},
            "timestamp": "2026-10-24T10:10:00Z",
            "cwd": root,
        }),
        json!({
            "hook_event_name": "PostToolUse",
            "session_id": session,
            "tool_name": "Bash",
            "tool_use_id": tool_use,
            "tool_input": {"command": managed_command},
            "tool_response": {"exit_code": 0},
            "timestamp": "2026-10-24T10:11:00Z",
            "cwd": root,
        }),
    ] {
        let replay = hook(payload);
        assert!(
            replay.status.success(),
            "{}",
            String::from_utf8_lossy(&replay.stderr)
        );
    }
    let replayed = hub
        .store
        .change_record(&writer_token, &work.id, unix_now())
        .unwrap()
        .unwrap()
        .latest_action
        .unwrap();
    assert_eq!(replayed.action_id, original_action_id);
    assert_eq!(replayed.result, CheckResultV1::Passed);

    let mut lowered = updated;
    lowered[records_index] = MIN_ACTION_CAPTURE_RECORDS.to_string();
    let lowered = run_capture_cli(&config, private.path(), &lowered, None);
    assert!(!lowered.status.success());
    assert!(String::from_utf8_lossy(&lowered.stderr).contains("spool limits cannot be lowered"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_cli_accepts_canonical_alias_and_records_outage_as_a_gap() {
    let mut hub = RestartableHub::start().await;
    let owner = hub
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 41,
            },
            "capture-writer",
        )
        .unwrap();
    let (writer_token, writer_secret, _) = mint_bound_agent_token(
        &*hub.store,
        &owner.id,
        "capture-writer",
        REPOSITORY,
        77,
        600,
        false,
    )
    .unwrap();
    let now = unix_now();
    let work = hub
        .store
        .work_start(
            &writer_token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("hook-cli-alias".into()),
                title: "capture through an aliased native session".into(),
                session: "default".into(),
                role: WorkRole::Lead,
                scope: vec!["tracked.txt".into()],
                branch: None,
                worktree: None,
                next_action: Some("Inspect the retained gap.".into()),
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
            },
            now,
        )
        .unwrap()
        .item;
    let observed = "native-session-41";
    let aliased = hub
        .store
        .touch_session(
            &writer_token,
            &SessionCall {
                session: Some(observed.into()),
                via: "hook:claude".into(),
                task: None,
                paths: vec![],
                deny: false,
                report: true,
            },
            now + 1,
        )
        .unwrap();
    assert_eq!(aliased.session, "default");

    let private = TempDir::new().unwrap();
    let root = private.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    run_git(&root, &["init", "--quiet", "--initial-branch=main"]);
    run_git(&root, &["config", "user.email", "fixture@example.invalid"]);
    run_git(&root, &["config", "user.name", "Synthetic Fixture"]);
    std::fs::write(root.join("tracked.txt"), "fixture\n").unwrap();
    run_git(&root, &["add", "tracked.txt"]);
    run_git(&root, &["commit", "--quiet", "-m", "fixture"]);
    run_git(
        &root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    run_git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let root = root.canonicalize().unwrap();
    let config = private.path().join("config");
    let config_agentscm = config.join("agentscm");
    std::fs::create_dir_all(&config_agentscm).unwrap();
    let token_file = private.path().join("writer.token");
    std::fs::write(&token_file, format!("{}\n", writer_secret.expose())).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        config_agentscm.join("connections.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "connections": [{
                "root": root,
                "origin_host": "github.com",
                "hub": hub.base,
                "repo": REPOSITORY,
                "token_file": token_file,
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let spool = private.path().join("spool");
    let recipe = private.path().join("recipe.json");
    std::fs::write(
        &recipe,
        serde_json::to_vec(&json!({
            "id": "unit-test",
            "argv": ["/usr/bin/true"],
            "coverage": "complete",
            "timeout_seconds": 10,
            "max_output_bytes": 4096,
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&recipe, std::fs::Permissions::from_mode(0o600)).unwrap();
    let marker = private.path().join("slow-recipe-started");
    let effects = private.path().join("slow-recipe-effects");
    let slow_recipe = private.path().join("slow-recipe.json");
    std::fs::write(
        &slow_recipe,
        serde_json::to_vec(&json!({
            "id": "slow-test",
            "argv": ["/bin/sh", "-c", format!("printf 'run\\n' >> '{}'; printf ready > '{}'; sleep 30", effects.display(), marker.display())],
            "coverage": "partial",
            "timeout_seconds": 60,
            "max_output_bytes": 4096,
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&slow_recipe, std::fs::Permissions::from_mode(0o600)).unwrap();
    let configure = vec![
        "change-capture".into(),
        "configure".into(),
        "--root".into(),
        root.display().to_string(),
        "--spool".into(),
        spool.display().to_string(),
        "--worktree-id".into(),
        "fixture-worktree".into(),
        "--setup-digest".into(),
        "5".repeat(64),
        "--action-rule".into(),
        "Bash=execution:partial:process_exit_code".into(),
        "--recipe-file".into(),
        recipe.display().to_string(),
        "--recipe-file".into(),
        slow_recipe.display().to_string(),
    ];
    let configured = run_capture_cli(&config, private.path(), &configure, None);
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );

    let hook = |payload: Value| {
        run_capture_cli(
            &config,
            private.path(),
            &["change-capture".into(), "hook-claude".into()],
            Some(&serde_json::to_vec(&payload).unwrap()),
        )
    };
    let started = hook(
        json!({"hook_event_name":"SessionStart","session_id":observed,"source":"startup","timestamp":"2026-10-24T10:00:00Z","cwd":root}),
    );
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let hook_output: Value = serde_json::from_slice(&started.stdout).unwrap();
    let context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("change-capture run-recipe --id 'unit-test'"));
    assert!(context.contains("change-capture run-recipe --id 'slow-test'"));
    assert!(!context.contains("/usr/bin/true"));
    let managed_command = |id: &str| {
        context
            .lines()
            .find_map(|line| line.strip_prefix(&format!("Recipe {id}: ")))
            .unwrap_or_else(|| panic!("missing managed invocation for {id}"))
            .to_owned()
    };
    let unit_command = managed_command("unit-test");
    let slow_command_text = managed_command("slow-test");

    for payload in [
        json!({"hook_event_name":"PreToolUse","session_id":observed,"tool_name":"Bash","tool_use_id":"aliased-tool","tool_input":{"command":"printf fixture"},"cwd":root}),
        json!({"hook_event_name":"PostToolUse","session_id":observed,"tool_name":"Bash","tool_use_id":"aliased-tool","tool_input":{"command":"printf fixture"},"tool_response":{"exit_code":0},"cwd":root}),
    ] {
        let output = hook(payload);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let before_outage = hub
        .store
        .change_record(&writer_token, &work.id, now + 2)
        .unwrap()
        .unwrap();
    let action = before_outage.latest_action.unwrap();
    assert_eq!(action.outcome, ActionOutcomeV1::Succeeded);
    assert_eq!(action.result, CheckResultV1::Unknown);

    let run_recipe = vec![
        "change-capture".into(),
        "run-recipe".into(),
        "--id".into(),
        "unit-test".into(),
    ];
    let unit_tool_use = "managed-unit-test";
    let managed_pre = hook(json!({
        "hook_event_name": "PreToolUse",
        "session_id": observed,
        "tool_name": "Bash",
        "tool_use_id": unit_tool_use,
        "tool_input": {"command": unit_command.clone()},
        "cwd": root,
    }));
    assert!(
        managed_pre.status.success(),
        "{}",
        String::from_utf8_lossy(&managed_pre.stderr)
    );
    let mut recipe_command = capture_cli_command(&config, private.path(), &run_recipe);
    let recipe_output = recipe_command
        .current_dir(&root)
        .env("CLAUDE_CODE_CHILD_SESSION", "1")
        .env("CLAUDE_CODE_SESSION_ID", observed)
        .output()
        .unwrap();
    assert!(
        recipe_output.status.success(),
        "{}",
        String::from_utf8_lossy(&recipe_output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&recipe_output.stdout).unwrap();
    assert_eq!(receipt["result"], "passed");
    assert_eq!(receipt["assurance"], "caller_asserted");
    let mut completed_replay = capture_cli_command(&config, private.path(), &run_recipe);
    let completed_replay = completed_replay
        .current_dir(&root)
        .env("CLAUDE_CODE_CHILD_SESSION", "1")
        .env("CLAUDE_CODE_SESSION_ID", observed)
        .output()
        .unwrap();
    assert!(
        completed_replay.status.success(),
        "{}",
        String::from_utf8_lossy(&completed_replay.stderr)
    );
    let replay_receipt: Value = serde_json::from_slice(&completed_replay.stdout).unwrap();
    assert_eq!(replay_receipt["action_id"], receipt["action_id"]);
    assert_eq!(replay_receipt["result"], "passed");
    assert_eq!(replay_receipt["exit_code"], Value::Null);
    let managed_post = hook(json!({
        "hook_event_name": "PostToolUse",
        "session_id": observed,
        "tool_name": "Bash",
        "tool_use_id": unit_tool_use,
        "tool_input": {"command": unit_command},
        "tool_response": {"exit_code": 0},
        "cwd": root,
    }));
    assert!(
        managed_post.status.success(),
        "{}",
        String::from_utf8_lossy(&managed_post.stderr)
    );
    for _ in 0..3 {
        let replay = hook(json!({
            "hook_event_name": "Notification",
            "session_id": observed,
            "cwd": root,
        }));
        assert!(replay.status.success());
    }
    let after_recipe = hub
        .store
        .change_record(&writer_token, &work.id, now + 2)
        .unwrap()
        .unwrap()
        .latest_action
        .unwrap();
    assert_eq!(after_recipe.kind, ActionKindV1::Verification);
    assert_eq!(after_recipe.result, CheckResultV1::Passed);
    assert_eq!(after_recipe.assurance, AssuranceBasisV1::CallerAsserted);

    let slow_run = vec![
        "change-capture".into(),
        "run-recipe".into(),
        "--id".into(),
        "slow-test".into(),
    ];
    let slow_tool_use = "managed-slow-test";
    let slow_pre = hook(json!({
        "hook_event_name": "PreToolUse",
        "session_id": observed,
        "tool_name": "Bash",
        "tool_use_id": slow_tool_use,
        "tool_input": {"command": slow_command_text.clone()},
        "cwd": root,
    }));
    assert!(
        slow_pre.status.success(),
        "{}",
        String::from_utf8_lossy(&slow_pre.stderr)
    );
    let git_barrier_dir = private.path().join("git-barrier-bin");
    std::fs::create_dir(&git_barrier_dir).unwrap();
    let source_capture_entered = private.path().join("source-capture-entered");
    let source_capture_release = private.path().join("source-capture-release");
    struct SourceBarrier(std::path::PathBuf);
    impl Drop for SourceBarrier {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"release\n");
        }
    }
    let source_barrier = SourceBarrier(source_capture_release.clone());
    let git_wrapper = git_barrier_dir.join("git");
    std::fs::write(
        &git_wrapper,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = --show-toplevel ]; then\n    : > '{}'\n    while [ ! -e '{}' ]; do sleep 0.01; done\n    break\n  fi\ndone\nexec /usr/bin/git \"$@\"\n",
            source_capture_entered.display(),
            source_capture_release.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&git_wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let wrapped_path = format!(
        "{}:{}",
        git_barrier_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut slow_command = capture_cli_command(&config, private.path(), &slow_run);
    let mut slow_child = slow_command
        .current_dir(&root)
        .env("CLAUDE_CODE_CHILD_SESSION", "1")
        .env("CLAUDE_CODE_SESSION_ID", observed)
        .env("PATH", &wrapped_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let reserved_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !source_capture_entered.exists() && std::time::Instant::now() < reserved_deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !source_capture_entered.exists() {
        let status_before_cleanup = slow_child.try_wait().unwrap();
        let status_before_cleanup_debug = format!("{status_before_cleanup:?}");
        let kill_error = if status_before_cleanup.is_none() {
            slow_child.kill().err().map(|error| error.to_string())
        } else {
            None
        };
        std::fs::write(&source_capture_release, b"release\n").unwrap();
        let reaped_status = match status_before_cleanup {
            Some(status) => status,
            None => slow_child.wait().unwrap(),
        };
        let mut diagnostic_stdout = Vec::new();
        let mut diagnostic_stderr = Vec::new();
        slow_child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut diagnostic_stdout)
            .unwrap();
        slow_child
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut diagnostic_stderr)
            .unwrap();
        panic!(
            "first runner did not reach source capture; status before cleanup: {status_before_cleanup_debug}; cleanup kill error: {kill_error:?}; reaped status: {reaped_status:?}; stdout: {}; stderr: {}",
            String::from_utf8_lossy(&diagnostic_stdout),
            String::from_utf8_lossy(&diagnostic_stderr)
        );
    }
    assert!(!effects.exists(), "child spawned before source barrier");
    let attempts = std::fs::read_dir(spool.join("recipe-attempts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 1);
    let reserved: Value = serde_json::from_slice(&std::fs::read(&attempts[0]).unwrap()).unwrap();
    assert_eq!(reserved["phase"]["state"], "reserved");
    let mut concurrent_run = capture_cli_command(&config, private.path(), &slow_run);
    let concurrent_run = concurrent_run
        .current_dir(&root)
        .env("CLAUDE_CODE_CHILD_SESSION", "1")
        .env("CLAUDE_CODE_SESSION_ID", observed)
        .env("PATH", &wrapped_path)
        .output()
        .unwrap();

    let reconcile = vec![
        "change-capture".into(),
        "reconcile-recipe".into(),
        "--id".into(),
        "slow-test".into(),
    ];
    let mut concurrent_reconcile = capture_cli_command(&config, private.path(), &reconcile);
    let concurrent_reconcile = concurrent_reconcile
        .current_dir(&root)
        .env("CLAUDE_CODE_CHILD_SESSION", "1")
        .env("CLAUDE_CODE_SESSION_ID", observed)
        .env("PATH", &wrapped_path)
        .output()
        .unwrap();
    let concurrent_run_was_blocked = !concurrent_run.status.success();
    let reconcile_status = concurrent_reconcile.status.code();
    let no_effect_before_release = !effects.exists();
    drop(source_barrier);
    assert!(concurrent_run_was_blocked);
    assert_eq!(reconcile_status, Some(2));
    let busy: Value = serde_json::from_slice(&concurrent_reconcile.stdout).unwrap();
    assert_eq!(busy["status"], "busy");
    assert_eq!(busy["reason"], "checkout_in_use");
    assert_eq!(busy["scope"], "configured_checkout");
    assert!(busy["fence_id"].as_str().is_some_and(|id| id.len() == 64));
    assert!(
        no_effect_before_release,
        "a busy caller spawned before release"
    );
    let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() && std::time::Instant::now() < ready_deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        marker.exists(),
        "configured recipe did not start after release"
    );
    assert_eq!(std::fs::read_to_string(&effects).unwrap(), "run\n");
    // SAFETY: this sends SIGTERM only to the owned helper child.
    assert_eq!(
        unsafe { libc::kill(slow_child.id() as i32, libc::SIGTERM) },
        0
    );
    let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let slow_status = loop {
        if let Some(status) = slow_child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= exit_deadline {
            let _ = slow_child.kill();
            panic!("configured recipe did not stop after SIGTERM");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let mut slow_stdout = Vec::new();
    let mut slow_stderr = Vec::new();
    slow_child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut slow_stdout)
        .unwrap();
    slow_child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut slow_stderr)
        .unwrap();
    assert_eq!(
        slow_status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&slow_stderr)
    );
    let interrupted: Value = serde_json::from_slice(&slow_stdout).unwrap();
    assert_eq!(interrupted["result"], "incomplete");
    assert_eq!(interrupted["stop"], "cancelled");
    let slow_post = hook(json!({
        "hook_event_name": "PostToolUseFailure",
        "session_id": observed,
        "tool_name": "Bash",
        "tool_use_id": slow_tool_use,
        "tool_input": {"command": slow_command_text},
        "error": "managed runner terminated",
        "cwd": root,
    }));
    assert!(
        slow_post.status.success(),
        "{}",
        String::from_utf8_lossy(&slow_post.stderr)
    );
    // The runner retains before delivery. Exercise bounded replay without adding another
    // recognized lifecycle event so the Hub sees the interrupted finish as well.
    for _ in 0..3 {
        let replay = hook(json!({
            "hook_event_name": "Notification",
            "session_id": observed,
            "cwd": root,
        }));
        assert!(replay.status.success());
    }
    let after_signal = hub
        .store
        .change_record(&writer_token, &work.id, now + 2)
        .unwrap()
        .unwrap()
        .latest_action
        .unwrap();
    assert_eq!(after_signal.result, CheckResultV1::Incomplete);

    hub.stop().await;
    let deferred = hook(json!({
        "hook_event_name":"PreToolUse",
        "session_id":observed,
        "tool_name":"Bash",
        "tool_use_id":"outage-tool",
        "tool_input":{"command":"printf unavailable"},
        "cwd":root,
    }));
    assert!(deferred.status.success());
    assert!(String::from_utf8_lossy(&deferred.stderr).contains("could not be resolved"));
    hub.restart().await;

    let ended = hook(json!({
        "hook_event_name":"SessionEnd",
        "session_id":observed,
        "timestamp":"2026-10-24T10:00:02Z",
        "cwd":root,
    }));
    assert!(
        ended.status.success(),
        "{}",
        String::from_utf8_lossy(&ended.stderr)
    );
    let after_outage = hub
        .store
        .change_record(&writer_token, &work.id, now + 3)
        .unwrap()
        .unwrap();
    assert!(after_outage.sessions.gaps > 0);
    assert_eq!(after_outage.sessions.coverage, CoverageV1::Partial);
}

#[test]
fn set_mode_changes_only_the_capture_mode_and_keeps_local_data() {
    let private = TempDir::new().unwrap();
    let root = private.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    run_git(&root, &["init", "--quiet", "--initial-branch=main"]);
    run_git(&root, &["config", "user.email", "fixture@example.invalid"]);
    run_git(&root, &["config", "user.name", "Synthetic Fixture"]);
    std::fs::write(root.join("tracked.txt"), "fixture\n").unwrap();
    run_git(&root, &["add", "tracked.txt"]);
    run_git(&root, &["commit", "--quiet", "-m", "fixture"]);
    run_git(
        &root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    run_git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let root = root.canonicalize().unwrap();
    let config = private.path().join("config");
    std::fs::create_dir_all(config.join("agentscm")).unwrap();
    let token_file = private.path().join("token");
    std::fs::write(&token_file, "synthetic-token\n").unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        config.join("agentscm/connections.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "connections": [{
                "root": root,
                "origin_host": "github.com",
                "hub": "http://127.0.0.1:9",
                "repo": REPOSITORY,
                "token_file": token_file,
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let spool = private.path().join("spool");
    let cli = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        run_capture_cli(&config, private.path(), &args, None)
    };
    let root_arg = root.display().to_string();
    let configured = cli(&[
        "change-capture",
        "configure",
        "--root",
        &root_arg,
        "--spool",
        &spool.display().to_string(),
        "--worktree-id",
        "set-mode-worktree",
        "--setup-digest",
        &"5".repeat(64),
        "--action-rule",
        "Bash=execution:partial:process_exit_code",
    ]);
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );
    let config_file = config.join("agentscm/change-capture.json");
    let read_config =
        || -> Value { serde_json::from_slice(&std::fs::read(&config_file).unwrap()).unwrap() };
    let original = read_config();
    let spool_files = |dir: &std::path::Path| {
        let mut names: Vec<_> = walk(dir);
        names.sort();
        names
    };
    fn walk(dir: &std::path::Path) -> Vec<String> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                names.extend(walk(&entry.path()));
            } else {
                names.push(entry.path().display().to_string());
            }
        }
        names
    }
    let files_before = spool_files(&spool);
    let status = |expected: &str| {
        let output = cli(&["change-capture", "status", "--root", &root_arg]);
        let status: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["capture_mode"], expected);
    };
    status("metadata_only");

    let set = cli(&[
        "change-capture",
        "set-mode",
        "--root",
        &root_arg,
        "--mode",
        "shared-conversation",
    ]);
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    status("shared_conversation");
    let shared = read_config();
    assert_eq!(shared["version"], 6);
    let mut expected = original.clone();
    expected["version"] = json!(6);
    expected["bindings"][0]["capture_mode"] = json!("shared_conversation");
    assert_eq!(shared, expected);

    let back = cli(&[
        "change-capture",
        "set-mode",
        "--root",
        &root_arg,
        "--mode",
        "metadata-only",
    ]);
    assert!(
        back.status.success(),
        "{}",
        String::from_utf8_lossy(&back.stderr)
    );
    assert_eq!(read_config(), original);
    assert_eq!(spool_files(&spool), files_before);

    let unknown = cli(&[
        "change-capture",
        "set-mode",
        "--root",
        &private.path().display().to_string(),
        "--mode",
        "local-conversation",
    ]);
    assert!(!unknown.status.success());
}

mod shared_bodies {
    use std::sync::atomic::{AtomicBool, Ordering};

    use agentscm::backend::{
        ImmutableBackend, ObjectKey, ObjectMetadata, PutOutcome, SOURCE_PROFILE_V1,
    };
    use agentscm::hub::change_record::SharedBodyState;
    use agentscm::hub::change_record_http::{SweepReport, sweep_shared_bodies};
    use agentscm::hub::login::{Login, LoginConfig};
    use agentscm::store::digest;

    use super::*;

    const SESSION: &str = "shared-session";

    async fn json_of(response: reqwest::Response) -> Value {
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap_or(Value::Null)
    }
    const DAY: i64 = 24 * 60 * 60;

    /// Delegates to a local backend but can be told to fail the next deletion or every write.
    struct Flaky {
        inner: LocalBackend,
        fail_delete: AtomicBool,
        fail_put: AtomicBool,
    }

    impl ImmutableBackend for Flaky {
        fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<()> {
            self.inner.prepare_publication(key, incarnation)
        }

        fn put_if_absent(
            &self,
            key: &ObjectKey,
            incarnation: &str,
            bytes: &[u8],
        ) -> agentscm::Result<PutOutcome> {
            if self.fail_put.load(Ordering::SeqCst) {
                return Err(agentscm::Error::Io(std::io::Error::other("injected")));
            }
            self.inner.put_if_absent(key, incarnation, bytes)
        }

        fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<Vec<u8>> {
            self.inner.get_verified(key, incarnation)
        }

        fn inspect_verified(
            &self,
            key: &ObjectKey,
            incarnation: &str,
        ) -> agentscm::Result<ObjectMetadata> {
            self.inner.inspect_verified(key, incarnation)
        }

        fn delete_incarnation(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<()> {
            if self.fail_delete.swap(false, Ordering::SeqCst) {
                return Err(agentscm::Error::Io(std::io::Error::other("injected")));
            }
            self.inner.delete_incarnation(key, incarnation)
        }

        fn get_range_verified(
            &self,
            key: &ObjectKey,
            incarnation: &str,
            offset: u64,
            length: u64,
        ) -> agentscm::Result<Vec<u8>> {
            self.inner
                .get_range_verified(key, incarnation, offset, length)
        }
    }

    struct Fixture {
        base: String,
        store: Arc<dyn Store>,
        backend: Arc<Flaky>,
        state: HubState,
        client: reqwest::Client,
        work_id: String,
        generation: i64,
        body: Vec<u8>,
        sha: String,
        owner: String,
        owner_secret: String,
        sibling_secret: String,
        member_secret: String,
        read_only_secret: String,
        admin_secret: String,
        _directory: TempDir,
    }

    fn user(store: &dyn Store, id: i64, login: &str) -> String {
        store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: id,
                },
                login,
            )
            .unwrap()
            .id
    }

    fn token(store: &dyn Store, owner: &str, name: &str, read_only: bool) -> (String, String) {
        let (id, secret, _) =
            mint_bound_agent_token(store, owner, name, REPOSITORY, 77, 3600, read_only).unwrap();
        (id, secret.expose().to_owned())
    }

    impl Fixture {
        /// The Work and body exist, but no event has declared the body yet.
        async fn start_undeclared() -> Self {
            Self::start_undeclared_for(30).await
        }

        async fn start_undeclared_for(retention_days: i64) -> Self {
            let directory = TempDir::new().unwrap();
            let database = format!("sqlite://{}", directory.path().join("hub.sqlite").display());
            let store = store::open(&database).unwrap();
            let backend = Arc::new(Flaky {
                inner: LocalBackend::open(directory.path().join("objects")).unwrap(),
                fail_delete: AtomicBool::new(false),
                fail_put: AtomicBool::new(false),
            });
            let login = LoginConfig::from_lookup("", |name| {
                (name == "AGENTSCM_ADMINS").then(|| "github:#900".to_owned())
            })
            .unwrap();
            let state = HubState::new(store.clone(), backend.clone())
                .with_login(Login::new(login).unwrap())
                .with_shared_conversation_retention_days(retention_days)
                .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let app = router(state.clone());
            tokio::spawn(async move { axum::serve(listener, app).await });

            let owner = user(&*store, 10, "owner");
            let member = user(&*store, 11, "member");
            let admin = user(&*store, 900, "hub-admin");
            let (owner_token, owner_secret) = token(&*store, &owner, "writer", false);
            let (_, sibling_secret) = token(&*store, &owner, "sibling", false);
            let (_, read_only_secret) = token(&*store, &owner, "reader", true);
            let (_, member_secret) = token(&*store, &member, "member", false);
            let (_, admin_secret) = token(&*store, &admin, "admin", false);
            let work = store
                .work_start(
                    &owner_token,
                    &WorkStart {
                        work_id: None,
                        source: None,
                        request_key: Some("shared-body".into()),
                        title: "share a conversation".into(),
                        session: SESSION.into(),
                        role: WorkRole::Lead,
                        scope: vec!["src/**".into()],
                        branch: None,
                        worktree: None,
                        next_action: None,
                        depends_on: vec![],
                        related_work: vec![],
                        takeover_generation: None,
                    },
                    unix_now(),
                )
                .unwrap()
                .item;
            let body = br#"{"messages":[{"role":"user","text":"synthetic"}]}"#.to_vec();
            let sha = digest(&body);
            Self {
                base,
                store,
                backend,
                state,
                client: reqwest::Client::new(),
                work_id: work.id,
                generation: work.generation,
                body,
                sha,
                owner,
                owner_secret,
                sibling_secret,
                member_secret,
                read_only_secret,
                admin_secret,
                _directory: directory,
            }
        }

        fn batch(&self, operation: &str) -> CaptureBatchV1 {
            let mut batch = setup_batch(&self.work_id, self.generation, SESSION);
            batch.operation_id = operation.into();
            batch.session.capture_mode = CaptureModeV1::SharedConversation;
            batch.events[0].setup = None;
            batch.events[0].artifact = Some(ArtifactRefV1 {
                digest: self.sha.clone(),
                size_bytes: self.body.len() as u64,
                media: ArtifactMediaV1::Json,
                retention: RetentionClassV1::SharedApproved,
                availability: ContentAvailabilityV1::Local,
            });
            batch
        }

        async fn start() -> Self {
            Self::start_for(30).await
        }

        async fn start_for(retention_days: i64) -> Self {
            let fixture = Self::start_undeclared_for(retention_days).await;
            let token = fixture.owner_token();
            fixture
                .store
                .change_ingest(&token, &fixture.batch("shared-declare"), unix_now())
                .unwrap();
            fixture
        }

        fn url(&self, sha: &str) -> String {
            format!(
                "{}/v1/hub/repos/{REPOSITORY_PATH}/change-records/{}/conversation/{sha}",
                self.base, self.work_id
            )
        }

        async fn put(&self, secret: &str, sha: &str, body: Vec<u8>) -> StatusCode {
            self.client
                .put(self.url(sha))
                .bearer_auth(secret)
                .body(body)
                .send()
                .await
                .unwrap()
                .status()
        }

        async fn delete(&self, secret: &str, sha: &str) -> StatusCode {
            self.client
                .delete(self.url(sha))
                .bearer_auth(secret)
                .send()
                .await
                .unwrap()
                .status()
        }

        /// Declares one more shared body through an accepted event and returns its digest.
        fn declare(&self, session: &str, sequence: u64, body: &[u8]) -> String {
            self.declare_all(session, sequence, &[body])[0].clone()
        }

        /// Declares consecutive bodies in one batch; sequences start at `first`.
        fn declare_all(&self, session: &str, first: u64, bodies: &[&[u8]]) -> Vec<String> {
            let mut batch = self.batch(&format!("declare-{session}-{first}"));
            batch.session.session_id = session.into();
            batch.events = bodies
                .iter()
                .enumerate()
                .map(|(offset, body)| {
                    let sequence = first + offset as u64;
                    let mut event = self.batch("unused").events.remove(0);
                    event.event_id = format!("{session}-event-{sequence}");
                    event.sequence = sequence;
                    event.kind = CapturedEventKindV1::AssistantMessage;
                    event.observed_at = 200 + sequence as i64;
                    event.artifact = Some(ArtifactRefV1 {
                        digest: digest(body),
                        size_bytes: body.len() as u64,
                        media: ArtifactMediaV1::Json,
                        retention: RetentionClassV1::SharedApproved,
                        availability: ContentAvailabilityV1::Local,
                    });
                    event
                })
                .collect();
            let digests = bodies.iter().map(|body| digest(body)).collect();
            self.store
                .change_ingest(&self.owner_token(), &batch, unix_now())
                .unwrap();
            digests
        }

        fn list_url(&self) -> String {
            format!(
                "{}/v1/hub/repos/{REPOSITORY_PATH}/change-records/{}/conversation",
                self.base, self.work_id
            )
        }

        async fn list(&self, secret: &str, query: &str) -> (StatusCode, Value) {
            let response = self
                .client
                .get(format!("{}{query}", self.list_url()))
                .bearer_auth(secret)
                .send()
                .await
                .unwrap();
            let status = response.status();
            (status, json_of(response).await)
        }

        async fn read(&self, secret: &str, sha: &str) -> reqwest::Response {
            self.client
                .get(self.url(sha))
                .bearer_auth(secret)
                .send()
                .await
                .unwrap()
        }

        /// The `agentscm_shared_conversation` result for these arguments.
        async fn mcp(&self, secret: &str, arguments: Value) -> Value {
            let reply = self
                .client
                .post(format!("{}/mcp", self.base))
                .bearer_auth(secret)
                .header("content-type", "application/json")
                .body(
                    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": "agentscm_shared_conversation", "arguments": arguments}})
                    .to_string(),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(reply.status(), StatusCode::OK);
            json_of(reply).await["result"].clone()
        }

        fn key(&self) -> ObjectKey {
            ObjectKey::new(
                "hub-conversation",
                &self.work_id,
                SOURCE_PROFILE_V1,
                &self.sha,
            )
            .unwrap()
        }

        fn object(&self) -> Option<Vec<u8>> {
            self.backend.get_verified(&self.key(), "v1").ok()
        }

        fn row(&self) -> agentscm::hub::change_record::SharedBodyRow {
            let token = self.owner_token();
            self.store
                .shared_body_lookup(&token, &self.work_id, &self.sha, unix_now())
                .unwrap()
                .unwrap()
                .row
        }

        fn owner_token(&self) -> String {
            let digest = agentscm::hub::store::agent_token_digest(&self.owner_secret).unwrap();
            self.store
                .authenticate(&digest, unix_now())
                .unwrap()
                .unwrap()
                .token_id
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_stores_the_exact_body_and_replays_idempotently() {
        let f = Fixture::start().await;
        assert_eq!(f.row().state, SharedBodyState::Declared);
        assert!(f.object().is_none());

        let before = unix_now();
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(f.object().as_deref(), Some(f.body.as_slice()));
        let row = f.row();
        assert_eq!(row.state, SharedBodyState::Available);
        let expires = row.expires_at.unwrap();
        assert!(expires >= before + 30 * DAY && expires <= unix_now() + 30 * DAY);

        // A replay, including from another token of the same human, changes nothing.
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            f.put(&f.sibling_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(f.row().expires_at, Some(expires));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_refuses_mismatches_unknown_digests_and_other_callers() {
        let f = Fixture::start().await;
        let mut other = f.body.clone();
        other[10] ^= 1;
        let truncated = f.body[..f.body.len() - 1].to_vec();
        let undeclared = br#"{"other":true}"#.to_vec();
        let undeclared_sha = digest(&undeclared);

        assert_eq!(
            f.put(&f.owner_secret, &f.sha, other).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, truncated).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            f.put(&f.owner_secret, &undeclared_sha, undeclared).await,
            StatusCode::NOT_FOUND
        );
        for bad in [f.sha.to_uppercase(), "abc".to_owned(), "g".repeat(64)] {
            assert_eq!(
                f.put(&f.owner_secret, &bad, f.body.clone()).await,
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
        assert_eq!(
            f.put(&f.member_secret, &f.sha, f.body.clone()).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            f.put(&f.read_only_secret, &f.sha, f.body.clone()).await,
            StatusCode::FORBIDDEN
        );
        let anonymous = f
            .client
            .put(f.url(&f.sha))
            .body(f.body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        let oversized = vec![b' '; 1024 * 1024 + 1];
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, oversized).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );

        assert_eq!(f.row().state, SharedBodyState::Declared);
        assert!(f.object().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_is_for_the_work_owner_or_a_hub_admin_and_blocks_later_uploads() {
        let f = Fixture::start().await;
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            f.delete(&f.member_secret, &f.sha).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            f.delete(&f.read_only_secret, &f.sha).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(f.object().as_deref(), Some(f.body.as_slice()));
        assert_eq!(
            f.delete(&f.owner_secret, &"1".repeat(64)).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            f.delete(&f.owner_secret, "NOT-HEX").await,
            StatusCode::BAD_REQUEST
        );

        assert_eq!(
            f.delete(&f.sibling_secret, &f.sha).await,
            StatusCode::NO_CONTENT
        );
        let row = f.row();
        assert_eq!(row.state, SharedBodyState::Deleted);
        assert_eq!(row.deleted_by.as_deref(), Some(f.owner.as_str()));
        assert!(row.physical_deleted_at.is_some());
        assert!(f.object().is_none());

        // Deleting again is a no-op, and the body cannot come back.
        assert_eq!(
            f.delete(&f.owner_secret, &f.sha).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::GONE
        );
        assert!(f.object().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hub_admin_may_delete_before_any_upload() {
        let f = Fixture::start().await;
        assert_eq!(
            f.delete(&f.admin_secret, &f.sha).await,
            StatusCode::NO_CONTENT
        );
        let row = f.row();
        assert_eq!(row.state, SharedBodyState::Deleted);
        assert_ne!(row.deleted_by.as_deref(), Some(f.owner.as_str()));
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::GONE
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sweep_expires_then_removes_objects_and_expired_bodies_stay_gone() {
        let f = Fixture::start().await;
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        let expires = f.row().expires_at.unwrap();

        let early = sweep_shared_bodies(&f.state, expires - 1).await.unwrap();
        assert_eq!(early, SweepReport::default());
        assert!(f.object().is_some());

        // Readers see the expiry at the deadline even before the sweep records it.
        assert_eq!(f.row().state_at(expires), SharedBodyState::Expired);
        assert_eq!(f.row().state, SharedBodyState::Available);

        let report = sweep_shared_bodies(&f.state, expires).await.unwrap();
        assert_eq!(
            report,
            SweepReport {
                expired: 1,
                deleted: 1,
                failed: 0
            }
        );
        assert!(f.object().is_none());
        assert_eq!(f.row().state, SharedBodyState::Expired);
        assert_eq!(
            sweep_shared_bodies(&f.state, expires + 1).await.unwrap(),
            SweepReport::default()
        );
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::GONE
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_removal_stays_pending_until_a_later_sweep() {
        let f = Fixture::start().await;
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        f.backend.fail_delete.store(true, Ordering::SeqCst);
        // The tombstone is the decision: the request succeeds though the object is still there.
        assert_eq!(
            f.delete(&f.owner_secret, &f.sha).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(f.row().state, SharedBodyState::Deleted);
        assert!(f.row().physical_deleted_at.is_none());
        assert!(f.object().is_some());

        let report = sweep_shared_bodies(&f.state, unix_now()).await.unwrap();
        assert_eq!(
            report,
            SweepReport {
                expired: 0,
                deleted: 1,
                failed: 0
            }
        );
        assert!(f.object().is_none());
        assert!(f.row().physical_deleted_at.is_some());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conversation_reads_are_for_the_owner_their_tokens_and_hub_admins() {
        let f = Fixture::start().await;
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        // The owner's writer, sibling and read-only tokens and the admin all read.
        for secret in [
            &f.owner_secret,
            &f.sibling_secret,
            &f.read_only_secret,
            &f.admin_secret,
        ] {
            let (status, page) = f.list(secret, "").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["entries"].as_array().unwrap().len(), 1);
            assert_eq!(page["entries"][0]["digest"], f.sha.as_str());
            let response = f.read(secret, &f.sha).await;
            assert_eq!(response.status(), StatusCode::OK);
            let headers = response.headers().clone();
            assert_eq!(headers["content-type"], "application/json");
            assert_eq!(headers["x-content-type-options"], "nosniff");
            assert!(
                headers["content-disposition"]
                    .to_str()
                    .unwrap()
                    .starts_with("attachment")
            );
            assert!(
                headers["content-security-policy"]
                    .to_str()
                    .unwrap()
                    .contains("sandbox")
            );
            assert_eq!(response.bytes().await.unwrap().as_ref(), f.body.as_slice());
        }
        // Another member of the repository gets neither the listing nor the body.
        let (status, error) = f.list(&f.member_secret, "").await;
        assert_eq!(
            (status, error),
            (StatusCode::FORBIDDEN, json!({"error": "forbidden"}))
        );
        let response = f.read(&f.member_secret, &f.sha).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!response.text().await.unwrap().contains("synthetic"));
        // No token, an unknown digest, a malformed digest and an unknown Work.
        let anonymous = f.client.get(f.list_url()).send().await.unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        let anonymous = f.client.get(f.url(&f.sha)).send().await.unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            f.read(&f.owner_secret, &"1".repeat(64)).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            f.read(&f.owner_secret, "NOT-HEX").await.status(),
            StatusCode::BAD_REQUEST
        );
        let unknown = format!("wrk_{}", "f".repeat(32));
        let response = f
            .client
            .get(f.list_url().replace(&f.work_id, &unknown))
            .bearer_auth(&f.owner_secret)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listing_maps_availability_through_upload_expiry_and_deletion() {
        // One day of retention, so a three-day token is still valid at the deadline.
        let f = Fixture::start_for(1).await;
        let entry = |page: &Value| page["entries"][0].clone();

        let (_, page) = f.list(&f.owner_secret, "").await;
        assert_eq!(
            entry(&page),
            json!({
                "session_id": SESSION, "sequence": 1, "event_id": "http-setup-event",
                "kind": "session_started", "observed_at": 100, "digest": f.sha,
                "size_bytes": f.body.len(), "availability": "local", "expires_at": null
            })
        );
        // Declared but never uploaded: nothing to read.
        assert_eq!(
            f.read(&f.owner_secret, &f.sha).await.status(),
            StatusCode::NOT_FOUND
        );

        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        let (_, page) = f.list(&f.owner_secret, "").await;
        assert_eq!(entry(&page)["availability"], "shared");
        let expires = entry(&page)["expires_at"].as_i64().unwrap();
        assert_eq!(expires, f.row().expires_at.unwrap());

        // At the deadline a reader sees the expiry although the sweep has not recorded it.
        let (long_lived, _) = {
            let (id, secret, _) =
                mint_bound_agent_token(&*f.store, &f.owner, "long", REPOSITORY, 77, 3 * DAY, false)
                    .unwrap();
            (id, secret)
        };
        let at_deadline = f
            .store
            .shared_conversation_page(&long_lived, &f.work_id, None, expires)
            .unwrap()
            .unwrap();
        assert_eq!(
            at_deadline.entries[0].availability,
            ContentAvailabilityV1::Expired
        );
        let before_deadline = f
            .store
            .shared_conversation_page(&long_lived, &f.work_id, None, expires - 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            before_deadline.entries[0].availability,
            ContentAvailabilityV1::Shared
        );
        assert_eq!(f.row().state, SharedBodyState::Available);

        sweep_shared_bodies(&f.state, expires).await.unwrap();
        let (_, page) = f.list(&f.owner_secret, "").await;
        assert_eq!(entry(&page)["availability"], "expired");
        assert_eq!(
            f.read(&f.owner_secret, &f.sha).await.status(),
            StatusCode::GONE
        );

        let g = Fixture::start().await;
        assert_eq!(
            g.put(&g.owner_secret, &g.sha, g.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            g.delete(&g.admin_secret, &g.sha).await,
            StatusCode::NO_CONTENT
        );
        let (_, page) = g.list(&g.owner_secret, "").await;
        assert_eq!(entry(&page)["availability"], "deleted");
        assert_eq!(
            g.read(&g.owner_secret, &g.sha).await.status(),
            StatusCode::GONE
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_object_is_an_integrity_failure_never_a_partial_body() {
        let f = Fixture::start().await;
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        f.backend.delete_incarnation(&f.key(), "v1").unwrap();
        let response = f.read(&f.owner_secret, &f.sha).await;
        assert_eq!(response.status(), StatusCode::FAILED_DEPENDENCY);
        assert_eq!(
            json_of(response).await,
            json!({"error": "integrity_failure"})
        );
        // The listing only knows the row, so it still says shared.
        let (_, page) = f.list(&f.owner_secret, "").await;
        assert_eq!(page["entries"][0]["availability"], "shared");
        // A caller without access is refused before the backend is consulted.
        assert_eq!(
            f.read(&f.member_secret, &f.sha).await.status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listing_pages_are_ordered_bounded_and_reject_tampered_cursors() {
        let f = Fixture::start().await;
        let bodies: Vec<String> = (2..=105).map(|n| format!(r#"{{"n":{n}}}"#)).collect();
        let bodies: Vec<&[u8]> = bodies.iter().map(|body| body.as_bytes()).collect();
        for (index, chunk) in bodies.chunks(50).enumerate() {
            f.declare_all(SESSION, 2 + 50 * index as u64, chunk);
        }

        let (status, page_one) = f.list(&f.read_only_secret, "").await;
        assert_eq!(status, StatusCode::OK);
        let entries = page_one["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 100);
        let cursor = page_one["next_cursor"].as_str().unwrap().to_owned();
        let (status, page_two) = f
            .list(&f.read_only_secret, &format!("?cursor={cursor}"))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page_two["next_cursor"], Value::Null);
        let all: Vec<&Value> = entries
            .iter()
            .chain(page_two["entries"].as_array().unwrap())
            .collect();
        assert_eq!(all.len(), 105);
        let keys: Vec<(&str, u64)> = all
            .iter()
            .map(|entry| {
                (
                    entry["session_id"].as_str().unwrap(),
                    entry["sequence"].as_u64().unwrap(),
                )
            })
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(keys, sorted);
        assert_eq!(keys[0], (SESSION, 1));
        assert_eq!(keys[104], (SESSION, 105));
        let encoded = serde_json::to_vec(&page_one).unwrap();
        assert!(encoded.len() <= 48 * 1024);

        // A cursor that is not one of ours, or has extra or odd parts, is refused.
        let digest = "a".repeat(64);
        for bad in [
            "garbage".to_owned(),
            "0:".to_owned() + &digest + ":s",
            format!("1:{}:s", digest.to_uppercase()),
            format!("1:{digest}:../x"),
            format!("1:{digest}"),
            "%00".to_owned(),
        ] {
            let (status, _) = f.list(&f.owner_secret, &format!("?cursor={bad}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let (status, _) = f.list(&f.owner_secret, "?other=1").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // A valid cursor gives another member no access.
        let (status, _) = f.list(&f.member_secret, &format!("?cursor={cursor}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_mcp_tool_applies_the_same_access_and_bounds() {
        let f = Fixture::start().await;
        // 49 KiB of JSON: readable over HTTP, too large for the tool.
        let large = format!(r#"{{"text":"{}"}}"#, "x".repeat(49 * 1024));
        let large_sha = f.declare(SESSION, 2, large.as_bytes());
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            f.put(&f.owner_secret, &large_sha, large.clone().into_bytes())
                .await,
            StatusCode::NO_CONTENT
        );

        // The owner's read-only token and an admin read through the tool.
        for secret in [&f.read_only_secret, &f.admin_secret] {
            let listing = f.mcp(secret, json!({"work_id": f.work_id})).await;
            assert_eq!(listing["isError"], false);
            assert_eq!(
                listing["structuredContent"]["entries"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            let body = f
                .mcp(secret, json!({"work_id": f.work_id, "digest": f.sha}))
                .await;
            assert_eq!(body["isError"], false);
            assert_eq!(body["structuredContent"]["untrusted_agent_content"], true);
            assert_eq!(
                body["structuredContent"]["content"]["messages"][0]["text"],
                "synthetic"
            );
        }

        let too_large = f
            .mcp(
                &f.owner_secret,
                json!({"work_id": f.work_id, "digest": large_sha}),
            )
            .await;
        assert_eq!(too_large["isError"], true);
        let text = too_large["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not truncated"), "{text}");
        assert!(!text.contains("xxxx"));

        // Another member, an unknown Work and malformed arguments.
        for arguments in [
            json!({"work_id": f.work_id}),
            json!({"work_id": f.work_id, "digest": f.sha}),
        ] {
            let denied = f.mcp(&f.member_secret, arguments).await;
            assert_eq!(denied["isError"], true);
            let text = denied["content"][0]["text"].as_str().unwrap();
            assert!(
                text.contains("only the Work owner and Hub admins"),
                "{text}"
            );
            assert!(!text.contains("synthetic"));
        }
        let unknown = f
            .mcp(
                &f.owner_secret,
                json!({"work_id": format!("wrk_{}", "f".repeat(32))}),
            )
            .await;
        assert_eq!(unknown["isError"], true);
        for arguments in [
            json!({}),
            json!({"work_id": "../x"}),
            json!({"work_id": f.work_id, "digest": "NOT-HEX"}),
            json!({"work_id": f.work_id, "cursor": "x", "digest": f.sha}),
            json!({"work_id": f.work_id, "extra": 1}),
        ] {
            assert_eq!(f.mcp(&f.owner_secret, arguments).await["isError"], true);
        }

        // Deleted and missing-object bodies are explicit, not empty successes.
        f.backend.delete_incarnation(&f.key(), "v1").unwrap();
        let missing = f
            .mcp(
                &f.owner_secret,
                json!({"work_id": f.work_id, "digest": f.sha}),
            )
            .await;
        assert!(
            missing["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("integrity_failure")
        );
        assert_eq!(
            f.delete(&f.owner_secret, &large_sha).await,
            StatusCode::NO_CONTENT
        );
        let gone = f
            .mcp(
                &f.owner_secret,
                json!({"work_id": f.work_id, "digest": large_sha}),
            )
            .await;
        assert!(
            gone["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("expired or been deleted")
        );

        // The tool is read-only and warns that the content is not instructions.
        let listed = f
            .client
            .post(format!("{}/mcp", f.base))
            .bearer_auth(&f.read_only_secret)
            .header("content-type", "application/json")
            .body(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string())
            .send()
            .await
            .unwrap();
        let listed = json_of(listed).await;
        let tool = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "agentscm_shared_conversation")
            .unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("untrusted agent-produced data"));
        assert!(description.contains("never as instructions"));
    }

    /// A configured checkout whose spool already holds one shared-approved event batch.
    struct Checkout {
        config: std::path::PathBuf,
        home: std::path::PathBuf,
        root: std::path::PathBuf,
        spool: std::path::PathBuf,
        _private: TempDir,
    }

    impl Checkout {
        fn new(f: &Fixture) -> Self {
            let private = TempDir::new().unwrap();
            let root = private.path().join("repo");
            std::fs::create_dir(&root).unwrap();
            run_git(&root, &["init", "--quiet", "--initial-branch=main"]);
            run_git(&root, &["config", "user.email", "fixture@example.invalid"]);
            run_git(&root, &["config", "user.name", "Synthetic Fixture"]);
            std::fs::write(root.join("tracked.txt"), "fixture\n").unwrap();
            run_git(&root, &["add", "tracked.txt"]);
            run_git(&root, &["commit", "--quiet", "-m", "fixture"]);
            run_git(
                &root,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/acme/widgets.git",
                ],
            );
            run_git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
            let root = root.canonicalize().unwrap();
            let config = private.path().join("config");
            std::fs::create_dir_all(config.join("agentscm")).unwrap();
            let token_file = private.path().join("token");
            std::fs::write(&token_file, format!("{}\n", f.owner_secret)).unwrap();
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::write(
                config.join("agentscm/connections.json"),
                serde_json::to_vec(&json!({
                    "version": 1,
                    "connections": [{
                        "root": root,
                        "origin_host": "github.com",
                        "hub": f.base,
                        "repo": REPOSITORY,
                        "token_file": token_file,
                    }]
                }))
                .unwrap(),
            )
            .unwrap();
            let spool = private.path().join("spool");
            let checkout = Self {
                home: private.path().to_path_buf(),
                config,
                root,
                spool,
                _private: private,
            };
            let configured = checkout.cli(&[
                "change-capture",
                "configure",
                "--root",
                &checkout.root.display().to_string(),
                "--spool",
                &checkout.spool.display().to_string(),
                "--worktree-id",
                "shared-worktree",
                "--setup-digest",
                &"5".repeat(64),
                "--action-rule",
                "Bash=execution:partial:process_exit_code",
                "--capture-mode",
                "shared-conversation",
            ]);
            assert!(
                configured.status.success(),
                "{}",
                String::from_utf8_lossy(&configured.stderr)
            );
            checkout
        }

        fn cli(&self, args: &[&str]) -> std::process::Output {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            run_capture_cli(&self.config, &self.home, &args, None)
        }

        /// Retains the fixture body and queues the event batch that declares it.
        fn queue(&self, f: &Fixture) {
            let spool = agentscm::change_capture::LocalSpool::open(&self.spool).unwrap();
            let reference = spool
                .retain_artifact(
                    &f.body,
                    ArtifactMediaV1::Json,
                    RetentionClassV1::SharedApproved,
                )
                .unwrap();
            assert_eq!(reference.digest, f.sha);
            spool
                .append(&agentscm::change_record::CaptureEnvelopeV1::EventBatch(
                    f.batch("queued-batch"),
                ))
                .unwrap();
        }

        /// A hook for a session with no Work: it captures nothing of its own but delivers.
        fn deliver(&self) {
            let payload = json!({
                "hook_event_name": "SessionStart",
                "session_id": "unrelated-native-session",
                "source": "startup",
                "timestamp": "2026-10-24T10:00:00Z",
                "cwd": self.root,
            });
            let output = run_capture_cli(
                &self.config,
                &self.home,
                &["change-capture".into(), "hook-claude".into()],
                Some(&serde_json::to_vec(&payload).unwrap()),
            );
            assert!(output.status.success());
        }

        fn pending(&self) -> u64 {
            let output = self.cli(&[
                "change-capture",
                "pending",
                "--spool",
                &self.spool.display().to_string(),
            ]);
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            report["count"]["present"].as_u64().unwrap()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delivery_uploads_bodies_in_shared_mode_and_acknowledges_only_afterwards() {
        let f = Fixture::start_undeclared().await;
        let checkout = Checkout::new(&f);
        checkout.queue(&f);
        assert_eq!(checkout.pending(), 1);

        // The Hub accepts the events but cannot store the body: the envelope must stay pending.
        f.backend.fail_put.store(true, Ordering::SeqCst);
        checkout.deliver();
        assert_eq!(checkout.pending(), 1);
        assert_eq!(f.row().state, SharedBodyState::Declared);
        assert!(f.object().is_none());

        f.backend.fail_put.store(false, Ordering::SeqCst);
        checkout.deliver();
        assert_eq!(checkout.pending(), 0);
        assert_eq!(f.row().state, SharedBodyState::Available);
        assert_eq!(f.object().as_deref(), Some(f.body.as_slice()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delivery_acknowledges_a_body_the_owner_already_deleted() {
        let f = Fixture::start().await;
        assert_eq!(
            f.delete(&f.owner_secret, &f.sha).await,
            StatusCode::NO_CONTENT
        );
        let checkout = Checkout::new(&f);
        checkout.queue(&f);
        checkout.deliver();
        assert_eq!(checkout.pending(), 0);
        assert_eq!(f.row().state, SharedBodyState::Deleted);
        assert!(f.object().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delivery_keeps_bodies_local_once_sharing_is_switched_off_or_the_body_is_missing() {
        let f = Fixture::start_undeclared().await;
        let off = Checkout::new(&f);
        off.queue(&f);
        let switched = off.cli(&[
            "change-capture",
            "set-mode",
            "--root",
            &off.root.display().to_string(),
            "--mode",
            "metadata-only",
        ]);
        assert!(switched.status.success());
        off.deliver();
        assert_eq!(off.pending(), 0);
        assert_eq!(f.row().state, SharedBodyState::Declared);
        assert!(f.object().is_none());

        // A body that is no longer in the spool is skipped and stays declared on the Hub.
        let missing = Checkout::new(&f);
        missing.queue(&f);
        std::fs::remove_file(missing.spool.join("artifacts").join(&f.sha)).unwrap();
        missing.deliver();
        assert_eq!(missing.pending(), 0);
        assert_eq!(f.row().state, SharedBodyState::Declared);
        assert!(f.object().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_conversation_command_lists_reads_and_deletes_with_the_connection_token() {
        let f = Fixture::start().await;
        assert_eq!(
            f.put(&f.owner_secret, &f.sha, f.body.clone()).await,
            StatusCode::NO_CONTENT
        );
        let checkout = Checkout::new(&f);
        let root = checkout.root.display().to_string();
        let run = |extra: &[&str]| {
            let mut args = vec![
                "change-capture",
                "conversation",
                "--root",
                &root,
                "--work-id",
                &f.work_id,
            ];
            args.extend_from_slice(extra);
            checkout.cli(&args)
        };

        let listed = run(&[]);
        assert!(listed.status.success());
        let page: Value = serde_json::from_slice(&listed.stdout).unwrap();
        assert_eq!(page["entries"][0]["digest"], f.sha.as_str());
        assert_eq!(page["entries"][0]["availability"], "shared");
        assert_eq!(page["next_cursor"], Value::Null);

        let body = run(&["--digest", &f.sha]);
        assert!(body.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&body.stdout).unwrap(),
            serde_json::from_slice::<Value>(&f.body).unwrap()
        );

        // Flags that contradict each other, and a digest that is not one, never reach the Hub.
        assert!(
            !run(&["--digest", &f.sha, "--delete", &f.sha])
                .status
                .success()
        );
        assert!(!run(&["--digest", "NOT-HEX"]).status.success());
        assert!(!run(&["--cursor", "x", "--digest", &f.sha]).status.success());

        let deleted = run(&["--delete", &f.sha]);
        assert!(deleted.status.success());
        assert_eq!(f.row().state, SharedBodyState::Deleted);
        let after = run(&["--digest", &f.sha]);
        assert!(!after.status.success());
        assert!(
            String::from_utf8_lossy(&after.stderr).contains("expired or been deleted"),
            "{}",
            String::from_utf8_lossy(&after.stderr)
        );
        let relisted: Value = serde_json::from_slice(&run(&[]).stdout).unwrap();
        assert_eq!(relisted["entries"][0]["availability"], "deleted");
    }
}
