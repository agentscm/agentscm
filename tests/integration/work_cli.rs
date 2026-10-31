//! The shared-work CLI preserves structured claim conflicts while returning a failing status.
use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::catchup::CatchupPreviewQuery;
use agentscm::hub::publish::{Create, NewPublication, Update, derive_id};
use agentscm::hub::store::{
    ForgeIdentity, Store, mint_agent_token, mint_bound_agent_token, unix_now,
};
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

    fn bound_client(&self, user_id: i64, login: &str, repository_id: i64) -> Client {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id,
        };
        let owner = self.store.upsert_user(&identity, login).unwrap();
        let (token_id, secret, _) = mint_bound_agent_token(
            &*self.store,
            &owner.id,
            &format!("agent-{user_id}"),
            REPOSITORY,
            repository_id,
            3600,
            false,
        )
        .unwrap();
        Client::new(&self.url, token_id, secret.expose())
    }

    fn sqlite(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.database.strip_prefix("sqlite://").unwrap()).unwrap()
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

    fn repair_merge(
        &self,
        database: Option<&OsStr>,
        repository: &str,
        publication: &str,
        merge_commit: &str,
        apply: bool,
    ) -> Output {
        const DATABASE_ENV: &str = "AGENTSCM_TEST_WORK_DATABASE";
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command
            .args([
                "work",
                "repair-merge",
                "--database-env",
                DATABASE_ENV,
                "--repository",
                repository,
                "--publication",
                publication,
                "--merge-commit",
                merge_commit,
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

fn publication(branch: &str) -> NewPublication {
    let commit = "b".repeat(40);
    NewPublication {
        id: derive_id(REPOSITORY, 3, &commit, branch),
        manifest: serde_json::from_value(serde_json::json!({
            "revision": "a".repeat(64), "event_seq": 3, "commit": commit,
            "tree": "c".repeat(40), "parent": "d".repeat(40), "branch": branch,
            "base": "main", "title": "Repair projection", "body": "", "message": "message\n",
            "author": {"name":"A","email":"a@example.invalid","date":"2026-10-25T00:00:00Z"},
            "committer": {"name":"A","email":"a@example.invalid","date":"2026-10-25T00:00:00Z"},
            "changes": [{"path":"src/a.rs","mode":"100644","blob":"e".repeat(40),
                "sha256":"f".repeat(64),"size":5}]
        }))
        .unwrap(),
    }
}

fn create_pull_publication(
    hub: &Hub,
    client: &Client,
    branch: &str,
    number: i64,
    now: i64,
) -> String {
    let publication = publication(branch);
    assert!(matches!(
        hub.store
            .create_publication(&client.token_id, &publication, now)
            .unwrap(),
        Create::Done(_)
    ));
    hub.store
        .update_publication(&publication.id, &Update::BranchAttempt, now)
        .unwrap();
    hub.store
        .update_publication(&publication.id, &Update::BranchCreated, now)
        .unwrap();
    hub.store
        .update_publication(
            &publication.id,
            &Update::PullRequest {
                number,
                node_id: format!("PR_{number}"),
                html_url: format!("https://github.example/acme/widgets/pull/{number}"),
            },
            now,
        )
        .unwrap();
    publication.id
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

fn cli_error(output: &Output) -> serde_json::Value {
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    serde_json::from_slice(&output.stderr).unwrap()
}

#[test]
fn repair_merge_previews_and_applies_only_the_guarded_retained_failure() {
    let hub = Hub::start();
    let client = hub.bound_client(91, "repairer", 991);
    let now = unix_now();
    let publication = create_pull_publication(&hub, &client, "repair-merge", 91, now);
    let invalid_commit = format!("legacy\n{}", "x".repeat(4096));
    hub.sqlite()
        .execute(
            "UPDATE hub_publications SET merge_commit=?2,updated_at=?3 WHERE id=?1",
            rusqlite::params![publication, invalid_commit, now + 1],
        )
        .unwrap();
    hub.store
        .work_catchup_preview(
            &client.token_id,
            &CatchupPreviewQuery {
                subscriber: "agent:repair-cli".into(),
            },
            now + 2,
        )
        .unwrap();
    assert_eq!(
        hub.sqlite()
            .query_row(
                "SELECT COUNT(*) FROM hub_catchup_publication_failures WHERE publication=?1",
                [&publication],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );

    let repaired_commit = "9".repeat(40);
    let preview = client.repair_merge(
        Some(OsStr::new(&hub.database)),
        REPOSITORY,
        &publication,
        &repaired_commit,
        false,
    );
    assert!(preview.status.success(), "{preview:?}");
    let preview_json: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(
        preview_json["previous_merge_commit_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&preview.stdout).unwrap(),
        serde_json::json!({
            "publication": publication,
            "repository": REPOSITORY,
            "previous_merge_commit_sha256": store::lower_hex_of(invalid_commit.as_bytes()),
            "proposed_merge_commit": repaired_commit,
            "applied": false
        })
    );
    assert_eq!(
        hub.store
            .publication(REPOSITORY, &publication)
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some(invalid_commit.as_str())
    );

    let wrong_repository = client.repair_merge(
        Some(OsStr::new(&hub.database)),
        "acme/other",
        &publication,
        &repaired_commit,
        false,
    );
    assert_eq!(
        cli_error(&wrong_repository)["error"],
        "invalid: unknown publication in repository"
    );
    let unknown = client.repair_merge(
        Some(OsStr::new(&hub.database)),
        REPOSITORY,
        "pub_00000000000000000000000000000000",
        &repaired_commit,
        false,
    );
    assert_eq!(
        cli_error(&unknown)["error"],
        "invalid: unknown publication in repository"
    );
    for malformed in ["9".repeat(39), "A".repeat(40), "z".repeat(40)] {
        let output = client.repair_merge(
            Some(OsStr::new(&hub.database)),
            REPOSITORY,
            &publication,
            &malformed,
            false,
        );
        assert_eq!(
            cli_error(&output)["error"],
            "invalid: merge commit must be 40 or 64 lowercase hexadecimal characters"
        );
    }

    let applied = client.repair_merge(
        Some(OsStr::new(&hub.database)),
        REPOSITORY,
        &publication,
        &repaired_commit,
        true,
    );
    assert!(applied.status.success(), "{applied:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&applied.stdout).unwrap(),
        serde_json::json!({
            "publication": publication,
            "repository": REPOSITORY,
            "previous_merge_commit_sha256": store::lower_hex_of(invalid_commit.as_bytes()),
            "proposed_merge_commit": repaired_commit,
            "applied": true
        })
    );
    assert_eq!(
        hub.store
            .publication(REPOSITORY, &publication)
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some(repaired_commit.as_str())
    );
    assert_eq!(
        hub.sqlite()
            .query_row(
                "SELECT COUNT(*) FROM hub_catchup_publication_failures WHERE publication=?1",
                [&publication],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );

    let valid_publication = create_pull_publication(&hub, &client, "valid-merge-fact", 92, now + 4);
    let valid_commit = "8".repeat(40);
    hub.store
        .update_publication(
            &valid_publication,
            &Update::Merged {
                merge_commit: valid_commit.clone(),
            },
            now + 5,
        )
        .unwrap();
    for apply in [false, true] {
        let refused = client.repair_merge(
            Some(OsStr::new(&hub.database)),
            REPOSITORY,
            &valid_publication,
            &valid_commit,
            apply,
        );
        assert_eq!(
            cli_error(&refused)["error"],
            "invalid: no retained catch-up projection failure for publication"
        );
    }
    let refused = client.repair_merge(
        Some(OsStr::new(&hub.database)),
        REPOSITORY,
        &valid_publication,
        &"7".repeat(40),
        true,
    );
    assert_eq!(
        cli_error(&refused)["error"],
        "invalid: no retained catch-up projection failure for publication"
    );
    assert_eq!(
        hub.store
            .publication(REPOSITORY, &valid_publication)
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some(valid_commit.as_str())
    );
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

#[test]
fn catchup_read_does_not_acknowledge_until_the_exact_delivery_is_marked_reviewed() {
    use agentscm::hub::catchup::{CatchupAck, CatchupAckResult, CatchupAckStatus, CatchupPage};
    let hub = Hub::start();
    let client = hub.client(17, "reader");
    let first = client.request("catchup", "{}");
    assert!(first.status.success(), "{first:?}");
    let page: CatchupPage = serde_json::from_slice(&first.stdout).unwrap();
    assert!(page.complete);
    let repeated = client.request("catchup", "{}");
    assert!(repeated.status.success(), "{repeated:?}");
    let repeated: CatchupPage = serde_json::from_slice(&repeated.stdout).unwrap();
    assert_eq!(repeated.event_from, page.event_from);
    assert_eq!(repeated.merge_from, page.merge_from);
    let ack = CatchupAck {
        delivery: page.delivery,
        expected_event_from: page.event_from,
        expected_merge_from: page.merge_from,
    };
    let marked = client.request("catchup-ack", &serde_json::to_string(&ack).unwrap());
    assert!(marked.status.success(), "{marked:?}");
    let result: CatchupAckResult = serde_json::from_slice(&marked.stdout).unwrap();
    assert_eq!(result.status, CatchupAckStatus::Advanced);
    let retry = client.request("catchup-ack", &serde_json::to_string(&ack).unwrap());
    assert!(retry.status.success(), "{retry:?}");
    let retry: CatchupAckResult = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(retry.status, CatchupAckStatus::AlreadyAcknowledged);
}

#[test]
fn preflight_reports_missing_source_without_creating_work() {
    use agentscm::hub::work_preflight::{
        WorkPreflightReason, WorkPreflightResult, WorkPreflightStatus,
    };
    let hub = Hub::start();
    let client = hub.client(18, "reader");
    let result = client.request("preflight", r#"{"request_key":"inspect-only","title":"Inspect source","session":"agent","scope":["src/a.rs"]}"#);
    assert!(result.status.success(), "{result:?}");
    let result: WorkPreflightResult = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result.status, WorkPreflightStatus::Unknown);
    assert!(result.reasons.contains(&WorkPreflightReason::MissingSource));
    assert!(result.subject_match.is_none());
    assert!(
        hub.store
            .work_search(&client.token_id, None, None, 100, unix_now())
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
fn explicit_pr_link_cli_uses_work_service_and_stale_fence() {
    let hub = Hub::start();
    let client = hub.bound_client(1, "alice", 42);
    let started = client
        .start(r#"{"request_key":"linked-task","title":"A linked task","session":"link-session"}"#);
    assert!(started.status.success(), "{started:?}");
    let start: WorkStartResult = serde_json::from_slice(&started.stdout).unwrap();
    let request = serde_json::json!({"work_id":start.item.id,"generation":start.item.generation,"session":"link-session","operation_id":"link-1","expected_updated_seq":start.item.updated_seq,"link":{"repository_id":42,"number":7,"head_sha":"a".repeat(40)}});
    let set = client.request("pr-link-set", &request.to_string());
    assert!(set.status.success(), "{set:?}");
    let result: serde_json::Value = serde_json::from_slice(&set.stdout).unwrap();
    assert_eq!(result["view"]["link"]["target"]["number"], 7);
    let get = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args([
            "work",
            "pr-link",
            &start.item.id,
            "--expected-updated-seq",
            &start.item.updated_seq.to_string(),
        ])
        .env_clear()
        .env("HOME", client.config.path())
        .env("XDG_CONFIG_HOME", client.config.path())
        .env("AGENTSCM_HUB_URL", &client.url)
        .env("AGENTSCM_HUB_TOKEN_FILE", &client.token_file)
        .output()
        .unwrap();
    assert!(get.status.success(), "{get:?}");
    let view: serde_json::Value = serde_json::from_slice(&get.stdout).unwrap();
    assert_eq!(view["snapshot_status"], "work_changed");
    let mut stale = request.clone();
    stale["operation_id"] = serde_json::json!("remove-stale");
    stale["link"] = serde_json::Value::Null;
    assert!(
        !client
            .request("pr-link-set", &stale.to_string())
            .status
            .success()
    );
    let mut missing = request.clone();
    missing["operation_id"] = serde_json::json!("missing-link");
    missing["expected_updated_seq"] = result["view"]["updated_seq"].clone();
    missing.as_object_mut().unwrap().remove("link");
    assert!(
        !client
            .request("pr-link-set", &missing.to_string())
            .status
            .success()
    );
}
