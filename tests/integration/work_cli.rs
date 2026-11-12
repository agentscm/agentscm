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

    fn why(&self, directory: &std::path::Path, target: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["why", target])
            .current_dir(directory)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.config.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("AGENTSCM_HUB_URL", &self.url)
            .env("AGENTSCM_HUB_TOKEN_FILE", &self.token_file)
            .output()
            .unwrap()
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

#[test]
fn named_overlap_cli_roundtrip_and_dependency_projection() {
    let hub = Hub::start();
    let a = hub.bound_client(901, "first", 101);
    let b = hub.bound_client(902, "second", 101);
    let first=a.start(r#"{"request_key":"first","title":"First behavior","session":"first","scope":["src/shared.rs"]}"#);
    assert!(first.status.success(), "{first:?}");
    let target: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let second = serde_json::json!({"request_key":"second","title":"Second behavior","session":"second","scope":["src/shared.rs"]});
    let refused = b.start(&second.to_string());
    assert!(!refused.status.success());
    let refused: serde_json::Value = serde_json::from_slice(&refused.stdout).unwrap();
    let request = &refused["coordination_request"];
    let grant=a.request("overlap-consent",&serde_json::json!({"request_id":request["id"],"target_work_id":target["item"]["id"],"operation_id":"grant","session":"first","expected_updated_seq":request["updated_seq"],"target_generation":target["item"]["generation"],"target_updated_seq":target["item"]["updated_seq"],"paths":["src/shared.rs"]}).to_string());
    assert!(grant.status.success(), "{grant:?}");
    let grant: serde_json::Value = serde_json::from_slice(&grant.stdout).unwrap();
    let envelope = serde_json::json!({"start":second,"consent_ids":[grant["consent"]["id"]]});
    let inspected = b.request("overlap-preflight", &envelope.to_string());
    assert!(inspected.status.success(), "{inspected:?}");
    let accepted = b.request("overlap-start", &envelope.to_string());
    assert!(accepted.status.success(), "{accepted:?}");
    let accepted: serde_json::Value = serde_json::from_slice(&accepted.stdout).unwrap();
    assert_eq!(accepted["created"], true);
    let dependent=b.start(&serde_json::json!({"request_key":"dependent","title":"Use first behavior","session":"dependent","scope":["docs/dependent.md"],"depends_on":[target["item"]["id"]]}).to_string());
    assert!(dependent.status.success(), "{dependent:?}");
    let dependent: serde_json::Value = serde_json::from_slice(&dependent.stdout).unwrap();
    assert_eq!(
        dependent["preflight"]["dependencies"]["conditions"][0]["disposition"]["state"],
        "unknown"
    );
    let notices = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args([
            "work",
            "dependency-notices",
            "--session",
            "first",
            "--after",
            "0",
            "--limit",
            "20",
        ])
        .env_clear()
        .env("HOME", a.config.path())
        .env("XDG_CONFIG_HOME", a.config.path())
        .env("AGENTSCM_HUB_URL", &a.url)
        .env("AGENTSCM_HUB_TOKEN_FILE", &a.token_file)
        .output()
        .unwrap();
    assert!(notices.status.success(), "{notices:?}");
    let notices: serde_json::Value = serde_json::from_slice(&notices.stdout).unwrap();
    assert_eq!(notices["notices"].as_array().unwrap().len(), 1);
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
            .publication(
                &crate::repository_scope::github(REPOSITORY, Some(991)),
                &publication
            )
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
            .publication(
                &crate::repository_scope::github(REPOSITORY, Some(991)),
                &publication
            )
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
            .publication(
                &crate::repository_scope::github(REPOSITORY, Some(991)),
                &valid_publication
            )
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some(valid_commit.as_str())
    );
}

#[test]
fn start_accepts_task_alias_and_omitted_optional_fields() {
    let hub = Hub::start();
    let client = hub.client(1, "alice");
    let output = client.start(
        r#"{"request_key":"alias-goal","task":"implement via task alias","session":"alice-session","scope":["src/alias.rs"]}"#,
    );
    assert!(output.status.success(), "{output:?}");
    let result: WorkStartResult = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result.item.title, "implement via task alias");
}

#[test]
fn request_decode_errors_name_the_field() {
    let hub = Hub::start();
    let client = hub.client(1, "alice");
    let unknown =
        client.start(r#"{"title":"t","session":"s","scope":[],"surprise":"secret-value"}"#);
    let missing = client.start(r#"{"title":"t","scope":[]}"#);
    let broken = client.start(r#"{"title":"#);
    for (output, expected) in [
        (&unknown, "work request field `surprise` is unknown"),
        (&missing, "work request field `session` is missing"),
        (
            &broken,
            "work request is not valid strict JSON at line 1 column",
        ),
    ] {
        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        assert!(!stderr.contains("secret-value"), "{stderr}");
    }
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
    // A revoked lead token no longer renews, but another owner waits for the lease to lapse.
    conflict_result(
        &second_client.start(&request.to_string()),
        WorkConflictKind::ClaimHeld,
    );
    hub.sqlite()
        .execute(
            "UPDATE hub_work_items SET reservation_expires_at = ?2 WHERE id = ?1",
            rusqlite::params![first_result.item.id, unix_now() - 1],
        )
        .unwrap();
    hub.store.observe_lease_clock(unix_now()).unwrap();
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

/// `agentscm why` turns a line and a path into commits with local Git, asks the Hub about each
/// and about the path from the repository root, and refuses a target that is neither.
#[test]
fn why_resolves_a_line_and_a_path_through_git_then_asks_the_hub() {
    let hub = Hub::start();
    let client = hub.client(19, "why");
    let started = client.start(r#"{"request_key":"why-cli","title":"Explain why.rs","session":"why-cli","scope":["src/why.rs"]}"#);
    assert!(started.status.success(), "{started:?}");
    let repository = TempDir::new().unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .current_dir(repository.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-q"]);
    std::fs::create_dir(repository.path().join("src")).unwrap();
    std::fs::write(repository.path().join("src/why.rs"), "fn why() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "why"]);
    let commit = git(&["rev-parse", "HEAD"]).trim().to_owned();

    let line = client.why(&repository.path().join("src"), "why.rs:1");
    assert!(line.status.success(), "{line:?}");
    let line: serde_json::Value = serde_json::from_slice(&line.stdout).unwrap();
    assert_eq!(line["commits"], serde_json::json!([commit]));
    assert_eq!(line["path"], "src/why.rs");
    assert_eq!(line["answers"][0]["known"], false, "{line}");
    assert_eq!(
        line["answers"][1]["work"][0]["title"], "Explain why.rs",
        "{line}"
    );

    let root = client.why(repository.path(), ".");
    assert!(root.status.success(), "{root:?}");
    let root: serde_json::Value = serde_json::from_slice(&root.stdout).unwrap();
    assert_eq!(root["path"], serde_json::Value::Null);
    assert_eq!(root["commits"], serde_json::json!([commit]));

    // An absolute path names the same file.
    let absolute = repository.path().join("src/why.rs");
    let absolute = client.why(repository.path(), &format!("{}:1", absolute.display()));
    assert!(absolute.status.success(), "{absolute:?}");
    let absolute: serde_json::Value = serde_json::from_slice(&absolute.stdout).unwrap();
    assert_eq!(absolute["path"], "src/why.rs");
    assert_eq!(absolute["commits"], serde_json::json!([commit]));

    // A line from a merged branch also names the merge that brought it in, not a merge of the
    // main line into the branch.
    let main = git(&["symbolic-ref", "--short", "HEAD"]).trim().to_owned();
    git(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(
        repository.path().join("src/why.rs"),
        "fn why() {}\nfn because() {}\n",
    )
    .unwrap();
    git(&["commit", "-q", "-am", "feature"]);
    let feature = git(&["rev-parse", "HEAD"]).trim().to_owned();
    git(&["checkout", "-q", &main]);
    std::fs::write(repository.path().join("other.txt"), "x\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "main moves"]);
    git(&["checkout", "-q", "feature"]);
    git(&["merge", "-q", "--no-ff", "-m", "sync main", &main]);
    git(&["checkout", "-q", &main]);
    git(&["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]);
    let merge = git(&["rev-parse", "HEAD"]).trim().to_owned();
    let merged = client.why(repository.path(), "src/why.rs:2");
    assert!(merged.status.success(), "{merged:?}");
    let merged: serde_json::Value = serde_json::from_slice(&merged.stdout).unwrap();
    assert_eq!(
        merged["commits"],
        serde_json::json!([feature, merge]),
        "{merged}"
    );

    // A squash merge leaves one new commit on the main line and no merge: the line names it.
    git(&["checkout", "-q", "-b", "squashed"]);
    std::fs::write(
        repository.path().join("src/why.rs"),
        "fn why() {}\nfn because() {}\nfn so() {}\n",
    )
    .unwrap();
    git(&["commit", "-q", "-am", "branch work"]);
    git(&["checkout", "-q", &main]);
    git(&["merge", "-q", "--squash", "squashed"]);
    git(&["commit", "-q", "-m", "squash merge"]);
    let squash = git(&["rev-parse", "HEAD"]).trim().to_owned();
    let squashed = client.why(repository.path(), "src/why.rs:3");
    assert!(squashed.status.success(), "{squashed:?}");
    let squashed: serde_json::Value = serde_json::from_slice(&squashed.stdout).unwrap();
    assert_eq!(
        squashed["commits"],
        serde_json::json!([squash]),
        "{squashed}"
    );

    let unknown = client.why(repository.path(), "no-such-target");
    assert!(!unknown.status.success(), "{unknown:?}");
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

#[test]
fn reconcile_cli_prints_the_report_and_fails_for_unknown_work() {
    let hub = Hub::start();
    let client = hub.bound_client(1, "alice", 42);
    let started =
        client.start(r#"{"request_key":"reconcile-task","title":"A task","session":"s"}"#);
    assert!(started.status.success(), "{started:?}");
    let start: WorkStartResult = serde_json::from_slice(&started.stdout).unwrap();
    let run = |id: &str| {
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["work", "reconcile", id])
            .env_clear()
            .env("HOME", client.config.path())
            .env("XDG_CONFIG_HOME", client.config.path())
            .env("AGENTSCM_HUB_URL", &client.url)
            .env("AGENTSCM_HUB_TOKEN_FILE", &client.token_file)
            .output()
            .unwrap()
    };
    let ok = run(&start.item.id);
    assert!(ok.status.success(), "{ok:?}");
    let report: serde_json::Value = serde_json::from_slice(&ok.stdout).unwrap();
    assert_eq!(report["work_id"], start.item.id);
    assert_eq!(report["provider"]["status"], "not_linked");
    assert_eq!(report["atomic_snapshot"], false);

    let missing = run("wrk_00000000000000000000000000000001");
    let error = cli_error(&missing);
    assert_eq!(
        (error["code"].as_str(), error["error_kind"].as_str()),
        (Some("not_found"), Some("nonexistent")),
        "{error}"
    );
}

#[test]
fn shared_status_cli_has_text_and_json_without_generated_prose() {
    let hub = Hub::start();
    let client = hub.bound_client(901, "synthetic-status-owner", 101);
    let started = client.start(r#"{"request_key":"status-cli","title":"Synthetic status task","session":"status-cli","scope":["src/status.rs"]}"#);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let run = |json: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command.args(["work", "status"]);
        if json {
            command.arg("--json");
        }
        command
            .env_clear()
            .env("HOME", client.config.path())
            .env("XDG_CONFIG_HOME", client.config.path())
            .env("AGENTSCM_HUB_URL", &client.url)
            .env("AGENTSCM_HUB_TOKEN_FILE", &client.token_file)
            .output()
            .unwrap()
    };
    let text = run(false);
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("1 active"));
    assert!(text.contains("Synthetic status task"));
    assert!(text.contains("Coverage incomplete"));
    let json = run(true);
    assert!(json.status.success());
    let status: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(status["schema_version"], 1);
    assert_eq!(status["counts"]["active"], 1);
    assert_eq!(status["work"][0]["state"], "active");
}

#[test]
fn recovery_cli_inspects_previews_and_applies_a_batch_with_per_item_outcomes() {
    let hub = Hub::start();
    let client = hub.bound_client(902, "synthetic-recovery-owner", 101);
    let started = client.start(r#"{"request_key":"recover-cli","title":"Obsolete task","session":"old-session","scope":["src/old.rs"]}"#);
    assert!(started.status.success(), "{started:?}");
    let start: WorkStartResult = serde_json::from_slice(&started.stdout).unwrap();
    let id = start.item.id.clone();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(args)
            .env_clear()
            .env("HOME", client.config.path())
            .env("XDG_CONFIG_HOME", client.config.path())
            .env("AGENTSCM_HUB_URL", &client.url)
            .env("AGENTSCM_HUB_TOKEN_FILE", &client.token_file)
            .output()
            .unwrap()
    };
    // Text and JSON inspection, with the live hold over the path listed exactly.
    let text = run(&[
        "work",
        "recovery",
        "--session",
        "new-session",
        "--path",
        "src/old.rs",
    ]);
    assert!(text.status.success(), "{text:?}");
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.starts_with("AgentSCM recovery: 1 active"), "{text}");
    assert!(
        text.contains(&format!("conflict {id} on src/old.rs")),
        "{text}"
    );
    let json = run(&["work", "recovery", "--json", "--session", "new-session"]);
    assert!(json.status.success(), "{json:?}");
    let page: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(page["work"][0]["work_id"], id.as_str());
    assert_eq!(page["work"][0]["standing"], "owner");

    let preview = run(&[
        "work",
        "recovery-preview",
        "--work-id",
        &id,
        "--remedy",
        "release",
        "--session",
        "new-session",
    ]);
    assert!(preview.status.success(), "{preview:?}");
    let preview: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview["permitted"], true);
    assert_eq!(preview["effects"]["follow_up"]["unfinished"], true);
    let unknown = run(&[
        "work",
        "recovery-preview",
        "--work-id",
        &id,
        "--remedy",
        "steal",
    ]);
    assert!(!unknown.status.success());

    // One item applies; a second with a stale fence is refused and the command exits non-zero.
    let batch = serde_json::json!({"session": "new-session", "items": [
        {"work_id": id, "operation_id": "cli-release", "generation": preview["generation"],
         "expected_updated_seq": preview["expected_updated_seq"], "transition": "release",
         "reason": "its session is gone"},
        {"work_id": id, "operation_id": "cli-abandon", "generation": preview["generation"],
         "expected_updated_seq": preview["expected_updated_seq"], "transition": "abandon",
         "reason": "stale read"}
    ]});
    let applied = client.request("transitions", &batch.to_string());
    assert!(!applied.status.success());
    let result: serde_json::Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(result["items"][0]["outcome"], "applied");
    assert_eq!(result["items"][1]["outcome"], "refused");
    assert_eq!(result["items"][1]["code"], "generation_changed");
    assert_eq!(
        (result["applied"].as_u64(), result["not_applied"].as_u64()),
        (Some(1), Some(1))
    );
    let stderr = String::from_utf8_lossy(&applied.stderr);
    assert!(
        stderr.contains("1 of 2 transitions were not applied"),
        "{stderr}"
    );
}

#[test]
fn issue_link_set_preserves_native_identity() {
    let hub = Hub::start();
    let client = hub.bound_client(91, "fixture", 101);
    let created = client.start(
        r#"{"request_key":"issue-cli","title":"Synthetic issue task","session":"issue-cli"}"#,
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let started: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let item = &started["item"];
    let request = serde_json::json!({"work_id":item["id"],"operation_id":"cli-link","generation":item["generation"],"session":"issue-cli","expected_updated_seq":item["updated_seq"],"link":{"provider":"github","host":"github.com","repository_id":101,"number":7}});
    let updated = client.request("issue-link-set", &request.to_string());
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&updated.stdout).unwrap();
    assert_eq!(result["item"]["id"], item["id"]);
    assert_eq!(result["item"]["source"], item["source"]);
    assert_eq!(result["item"]["issue_link"]["number"], 7);
}

#[test]
fn recorded_intent_read_and_errors() {
    let hub = Hub::start();
    let client = hub.bound_client(92, "fixture", 101);
    let created = client.start(r#"{"request_key":"intent-cli","title":"Synthetic intent task","session":"intent-cli","next_action":"Check the fixture"}"#);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let started: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let id = started["item"]["id"].as_str().unwrap();
    let read = |id: &str| {
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["work", "intent", id])
            .env_clear()
            .env("HOME", client.config.path())
            .env("XDG_CONFIG_HOME", client.config.path())
            .env("AGENTSCM_HUB_URL", &client.url)
            .env("AGENTSCM_HUB_TOKEN_FILE", &client.token_file)
            .output()
            .unwrap()
    };
    let before = hub.store.latest_event_seq().unwrap();
    let output = read(id);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["work_id"], id);
    assert_eq!(report["next_action"], "Check the fixture");
    assert!(report["completeness"].is_object());
    assert_eq!(hub.store.latest_event_seq().unwrap(), before);
    assert!(
        !read("wrk_00000000000000000000000000000000")
            .status
            .success()
    );
    // Query/path delimiters stay inside the ID segment and cannot redirect this read.
    assert!(
        !read(&format!("{id}/../overview?operational=1"))
            .status
            .success()
    );
}
