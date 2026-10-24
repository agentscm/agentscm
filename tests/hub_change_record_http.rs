//! Authenticated HTTP projection for one concise change record.
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::change_capture::MIN_ACTION_CAPTURE_RECORDS;
use agentscm::change_record::{
    ActionFinishV1, ActionKindV1, ActionOutcomeV1, ActionStartV1, ArtifactMediaV1, ArtifactRefV1,
    AssuranceBasisV1, CheckResultV1, ContentAvailabilityV1, CoverageV1, DriftV1,
    RecipeProvenanceV1, RetentionClassV1, SourceBindingV1,
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
