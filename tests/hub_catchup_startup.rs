//! Durable catch-up startup behavior on the real HTTP, MCP and native-hook surfaces.

use std::path::PathBuf;
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::publish::{Create, NewPublication, Update, derive_id};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::{HubState, router};
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPOSITORY: &str = "acme/widgets";

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    server: tokio::task::JoinHandle<()>,
    database: PathBuf,
    _directory: TempDir,
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Hub {
    async fn start() -> Self {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("hub.sqlite");
        let url = format!("sqlite://{}", database.display());
        let store = store::open(&url).unwrap();
        let objects = LocalBackend::open(directory.path().join("objects")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            store,
            client: reqwest::Client::new(),
            server,
            database,
            _directory: directory,
        }
    }

    fn token(&self, user_id: i64, name: &str, repository: &str) -> (String, String) {
        let owner = self
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id,
                },
                &format!("user{user_id}"),
            )
            .unwrap();
        let (token, secret) =
            mint_agent_token(&*self.store, &owner.id, name, repository, 3600).unwrap();
        (token, secret.expose().to_owned())
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        secret: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        if let Some(secret) = secret {
            request = request.bearer_auth(secret);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"text": text}));
        (status, value)
    }

    async fn mcp(&self, secret: &str, name: &str, arguments: Value) -> Value {
        let (_, response) = self
            .call(
                Method::POST,
                "/mcp",
                Some(secret),
                Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":name,"arguments":arguments}})),
            )
            .await;
        response["result"].clone()
    }
}

fn merged_publication(store: &dyn Store, token: &str, now: i64) -> String {
    let commit = "b".repeat(40);
    let publication = NewPublication {
        id: derive_id(REPOSITORY, 3, &commit, "feat/startup-preview"),
        manifest: serde_json::from_value(json!({
            "revision": "a".repeat(64), "event_seq": 3, "commit": commit,
            "tree": "c".repeat(40), "parent": "d".repeat(40),
            "branch": "feat/startup-preview", "base": "main",
            "title": "Startup preview merge", "body": "", "message": "message\n",
            "author": {"name":"A","email":"a@example.invalid","date":"2026-10-25T00:00:00Z"},
            "committer": {"name":"A","email":"a@example.invalid","date":"2026-10-25T00:00:00Z"},
            "changes": [{"path":"src/a.rs","mode":"100644","blob":"e".repeat(40),
                "sha256":"f".repeat(64),"size":5}]
        }))
        .unwrap(),
    };
    let id = publication.id.clone();
    assert!(matches!(
        store.create_publication(token, &publication, now).unwrap(),
        Create::Done(_)
    ));
    for update in [Update::BranchAttempt, Update::BranchCreated] {
        store.update_publication(&id, &update, now).unwrap();
    }
    store
        .update_publication(
            &id,
            &Update::PullRequest {
                number: 17,
                node_id: "PR_17".into(),
                html_url: "https://github.example/acme/widgets/pull/17".into(),
            },
            now,
        )
        .unwrap();
    store
        .update_publication(
            &id,
            &Update::Merged {
                merge_commit: format!("{:040x}", 17),
            },
            now,
        )
        .unwrap();
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tracked_publication_merge_is_in_delivery_free_startup_preview() {
    let hub = Hub::start().await;
    let (token, secret) = hub.token(1, "writer", REPOSITORY);
    let merged_id = merged_publication(&*hub.store, &token, unix_now());
    let event_count = hub.store.events_after(0, 10_000).unwrap().len();

    assert_eq!(
        hub.call(Method::GET, "/v1/hub/standup/overview", None, None)
            .await
            .0,
        401
    );
    let (status, overview) = hub
        .call(Method::GET, "/v1/hub/standup/overview", Some(&secret), None)
        .await;
    assert_eq!(status, 200, "{overview}");
    assert_eq!(
        hub.store.events_after(0, 10_000).unwrap().len(),
        event_count,
        "overview read wrote a Hub event"
    );

    let startup = hub
        .mcp(
            &secret,
            "agentscm_start",
            json!({"task":"Begin after merge","request_key":"after-merge",
                "session":"after-merge-session"}),
        )
        .await;
    assert_eq!(startup["isError"], false, "{startup}");
    assert_eq!(
        startup["structuredContent"]["catchup"]["merges"][0]["source_id"],
        merged_id
    );
    assert!(startup["structuredContent"]["briefing"].is_object());
    assert!(startup["structuredContent"]["briefing"]["delivery"].is_null());
    assert_eq!(startup["structuredContent"]["acknowledged"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn optional_catchup_projection_failure_does_not_fail_overview_or_claim() {
    let hub = Hub::start().await;
    let (_, secret) = hub.token(1, "writer", REPOSITORY);
    rusqlite::Connection::open(&hub.database)
        .unwrap()
        .execute("DROP TABLE hub_publications", [])
        .unwrap();

    let (status, overview) = hub
        .call(Method::GET, "/v1/hub/standup/overview", Some(&secret), None)
        .await;
    assert_eq!(status, 200, "{overview}");
    assert!(overview["current"].is_array());

    let startup = hub
        .mcp(
            &secret,
            "agentscm_start",
            json!({"task":"Claim despite missing projection","request_key":"projection-unavailable",
                "session":"unavailable-session"}),
        )
        .await;
    assert_eq!(startup["isError"], false, "{startup}");
    assert_eq!(startup["structuredContent"]["work"]["created"], true);
    let coverage = startup["structuredContent"]["catchup"]["source_coverage"]
        .as_array()
        .unwrap();
    assert!(
        coverage
            .iter()
            .any(|source| source["source"] == "tracked_publication"
                && source["state"] == "unavailable"),
        "{startup}"
    );
    let warning = startup["content"][0]["text"].as_str().unwrap();
    assert!(warning.contains("Some catch-up sources are incomplete or unavailable"));
    assert!(warning.contains("affected coverage is unknown"));
    assert!(warning.contains("Do not infer that no recent merges were recorded"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_catchup_warns_before_forge_coverage_exists() {
    let hub = Hub::start().await;
    let (_, secret) = hub.token(1, "writer", REPOSITORY);

    let (status, hook) = hub
        .call(
            Method::POST,
            "/v1/hub/hook/claude",
            Some(&secret),
            Some(json!({
                "session_id": "11111111-2222-3333-4444-555555555555",
                "transcript_path": "/work/.claude/projects/-work-acme-widgets/session.jsonl",
                "cwd": "/work/acme-widgets",
                "hook_event_name": "SessionStart",
                "source": "startup",
                "model": "synthetic-model"
            })),
        )
        .await;
    assert_eq!(status, 200, "{hook}");
    let hook_text = hook
        .pointer("/hookSpecificOutput/additionalContext")
        .and_then(Value::as_str)
        .unwrap();
    assert!(
        hook_text.contains("Some catch-up sources are incomplete or unavailable"),
        "{hook}"
    );
    assert!(hook_text.contains("affected coverage is unknown"), "{hook}");
    assert!(
        hook_text.contains("Do not infer that no recent merges were recorded"),
        "{hook}"
    );

    let startup = hub
        .mcp(
            &secret,
            "agentscm_start",
            json!({"task":"Begin before source sync","request_key":"empty-coverage",
                "session":"empty-coverage-session"}),
        )
        .await;
    assert_eq!(startup["isError"], false, "{startup}");
    let coverage = startup["structuredContent"]["catchup"]["source_coverage"]
        .as_array()
        .unwrap();
    assert_eq!(coverage.len(), 2, "{startup}");
    assert!(
        coverage
            .iter()
            .any(|source| source["source"] == "forge_pull" && source["state"] == "unavailable"),
        "{startup}"
    );
    assert!(
        coverage
            .iter()
            .any(|source| source["source"] == "tracked_publication"
                && source["state"] == "observed_only"),
        "{startup}"
    );
    let mcp_text = startup["content"][0]["text"].as_str().unwrap();
    assert!(
        mcp_text.contains("Some catch-up sources are incomplete or unavailable"),
        "{startup}"
    );
    assert!(
        mcp_text.contains("affected coverage is unknown"),
        "{startup}"
    );
    assert!(
        mcp_text.contains("Do not infer that no recent merges were recorded"),
        "{startup}"
    );
}
