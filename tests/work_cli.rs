//! The shared-work CLI preserves structured claim conflicts while returning a failing status.
use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::work::{
    WorkCheckpointResult, WorkCompaction, WorkConflictKind, WorkStartResult,
};
use agentscm::hub::{HubState, router, store};
use tempfile::TempDir;

const REPOSITORY: &str = "acme/widgets";

struct Hub {
    database: String,
    url: String,
    store: Arc<dyn Store>,
    _runtime: tokio::runtime::Runtime,
    _directory: TempDir,
}

impl Hub {
    fn start() -> Self {
        let directory = TempDir::new().unwrap();
        let database = format!("sqlite://{}", directory.path().join("hub.sqlite").display());
        let store = store::open(&database).unwrap();
        let objects = LocalBackend::open(directory.path().join("objects")).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        let address = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await });
            address
        });
        Self {
            database,
            url: format!("http://{address}"),
            store,
            _runtime: runtime,
            _directory: directory,
        }
    }

    fn client(&self, user_id: i64, login: &str) -> Client {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id,
        };
        let owner = self.store.upsert_user(&identity, login).unwrap();
        let (token_id, secret) = mint_agent_token(
            &*self.store,
            &owner.id,
            &format!("agent-{user_id}"),
            REPOSITORY,
            3600,
        )
        .unwrap();
        Client::new(&self.url, token_id, secret.expose())
    }
}

struct Client {
    config: TempDir,
    url: String,
    token_id: String,
    token_file: PathBuf,
}

impl Client {
    fn new(url: &str, token_id: String, secret: &str) -> Self {
        let config = TempDir::new().unwrap();
        let token_file = config.path().join("agent.token");
        std::fs::write(&token_file, format!("{secret}\n")).unwrap();
        Self {
            config,
            url: url.into(),
            token_id,
            token_file,
        }
    }

    fn request(&self, operation: &str, request: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["work", operation, "--request", "-"])
            .env_clear()
            .env("HOME", self.config.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("AGENTSCM_HUB_URL", &self.url)
            .env("AGENTSCM_HUB_TOKEN_FILE", &self.token_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn start(&self, request: &str) -> Output {
        self.request("start", request)
    }

    fn checkpoint(&self, request: &str) -> Output {
        self.request("checkpoint", request)
    }

    fn compact(&self, database: Option<&OsStr>, before: i64, apply: bool) -> Output {
        let before = before.to_string();
        const DATABASE_ENV: &str = "AGENTSCM_TEST_WORK_DATABASE";
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command
            .args([
                "work",
                "compact",
                "--database-env",
                DATABASE_ENV,
                "--provider",
                "github",
                "--host",
                "github.com",
                "--repository",
                REPOSITORY,
                "--before",
                &before,
            ])
            .env_clear()
            .env("HOME", self.config.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(database) = database {
            command.env(DATABASE_ENV, database);
        }
        if apply {
            command.arg("--apply");
        }
        command.output().unwrap()
    }
}

fn conflict_result(output: &Output, expected: WorkConflictKind) -> WorkStartResult {
    assert!(!output.status.success(), "{output:?}");
    let result: WorkStartResult = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result.conflict.as_ref().unwrap().kind, expected);
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(
        error,
        serde_json::json!({
            "ok": false,
            "error": "conflict: durable work claim was not acquired; inspect the JSON result",
            "error_kind": "conflict"
        })
    );
    result
}

#[test]
fn start_claim_conflicts_keep_json_and_exit_nonzero() {
    let hub = Hub::start();
    let first_client = hub.client(1, "alice");
    let first = first_client.start(
        r#"{"work_id":null,"source":null,"request_key":"shared-goal","title":"implement shared work","session":"alice-session","scope":["src/shared.rs"],"branch":null,"worktree":null,"next_action":null,"depends_on":[],"related_work":[],"takeover_generation":null}"#,
    );
    assert!(first.status.success(), "{first:?}");
    let first_result: WorkStartResult = serde_json::from_slice(&first.stdout).unwrap();
    let checkpoint = first_client.checkpoint(
        &serde_json::json!({
            "work_id": first_result.item.id.clone(),
            "operation_id": "cli-progress-1",
            "generation": first_result.item.generation,
            "expected_updated_seq": first_result.item.updated_seq,
            "session": "alice-session",
            "kind": "progress",
            "summary": "CLI returns the typed receipt"
        })
        .to_string(),
    );
    assert!(checkpoint.status.success(), "{checkpoint:?}");
    let checkpoint_result: WorkCheckpointResult =
        serde_json::from_slice(&checkpoint.stdout).unwrap();
    assert_eq!(checkpoint_result.receipt.operation_id, "cli-progress-1");
    assert_eq!(checkpoint_result.receipt.work_id, first_result.item.id);
    assert!(!checkpoint_result.receipt.replayed);
    assert_eq!(checkpoint_result.item.id, first_result.item.id);

    let before_compaction = checkpoint_result.item.updated_seq;
    let cutoff = unix_now() - 24 * 60 * 60 - 1;
    let dry_run = first_client.compact(Some(OsStr::new(&hub.database)), cutoff, false);
    assert!(dry_run.status.success(), "{dry_run:?}");
    let dry_run: WorkCompaction = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert!(!dry_run.applied);
    assert_eq!(dry_run.deleted_operations, 0);
    assert_eq!(dry_run.deleted_versions, 0);
    assert_eq!(
        hub.store
            .work_get(&first_client.token_id, &first_result.item.id, unix_now())
            .unwrap()
            .unwrap()
            .updated_seq,
        before_compaction
    );
    let applied = first_client.compact(Some(OsStr::new(&hub.database)), cutoff, true);
    assert!(applied.status.success(), "{applied:?}");
    let applied: WorkCompaction = serde_json::from_slice(&applied.stdout).unwrap();
    assert!(applied.applied);
    assert_eq!(applied.retention_floor, dry_run.retention_floor);

    let missing = first_client.compact(None, cutoff, false);
    assert!(!missing.status.success(), "{missing:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&missing.stderr).unwrap(),
        serde_json::json!({
            "ok": false,
            "error": "invalid: database environment variable is not set",
            "error_kind": "invalid"
        })
    );
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let non_unicode = OsString::from_vec(vec![0xff]);
        let invalid = first_client.compact(Some(&non_unicode), cutoff, false);
        assert!(!invalid.status.success(), "{invalid:?}");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&invalid.stderr).unwrap(),
            serde_json::json!({
                "ok": false,
                "error": "invalid: database environment variable is not valid Unicode",
                "error_kind": "invalid"
            })
        );
    }

    let second_client = hub.client(2, "bob");
    let mut request = serde_json::json!({
        "work_id": first_result.item.id,
        "source": null,
        "request_key": null,
        "title": "implement shared work",
        "session": "bob-session",
        "scope": ["src/shared.rs"],
        "branch": null,
        "worktree": null,
        "next_action": null,
        "depends_on": [],
        "related_work": [],
        "takeover_generation": null
    });
    conflict_result(
        &second_client.start(&request.to_string()),
        WorkConflictKind::ClaimHeld,
    );

    assert!(
        hub.store
            .revoke_agent_token(&first_client.token_id)
            .unwrap()
    );
    let stale = conflict_result(
        &second_client.start(&request.to_string()),
        WorkConflictKind::StaleTakeoverRequired,
    );
    request["takeover_generation"] = serde_json::json!(stale.item.generation + 1);
    conflict_result(
        &second_client.start(&request.to_string()),
        WorkConflictKind::GenerationChanged,
    );
}
