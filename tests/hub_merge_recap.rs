//! Bounded tracked-publication merge recaps on the real HTTP/MCP surfaces.

#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::path::PathBuf;
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::publish::{Create, NewPublication, Update, derive_id};
use agentscm::hub::store::{
    self, ForgeIdentity, Store, mint_agent_token, mint_bound_agent_token, unix_now,
};
use agentscm::hub::{HubState, router};
use hub_pg::PgSchema;
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

fn publication(repository: &str, branch: &str, title: &str) -> NewPublication {
    let commit = "b".repeat(40);
    NewPublication {
        id: derive_id(repository, 3, &commit, branch),
        manifest: serde_json::from_value(json!({
            "revision": "a".repeat(64), "event_seq": 3, "commit": commit,
            "tree": "c".repeat(40), "parent": "d".repeat(40), "branch": branch,
            "base": "main", "title": title, "body": "", "message": "message\n",
            "author": {"name":"A","email":"a@example.invalid","date":"2026-10-25T00:00:00Z"},
            "committer": {"name":"A","email":"a@example.invalid","date":"2026-10-25T00:00:00Z"},
            "changes": [{"path":"src/a.rs","mode":"100644","blob":"e".repeat(40),
                "sha256":"f".repeat(64),"size":5}]
        }))
        .unwrap(),
    }
}

fn create_publication(
    store: &dyn Store,
    token: &str,
    repository: &str,
    branch: &str,
    title: &str,
    created_at: i64,
) -> String {
    let publication = publication(repository, branch, title);
    assert!(matches!(
        store
            .create_publication(token, &publication, created_at)
            .unwrap(),
        Create::Done(_)
    ));
    publication.id
}

fn merge_publication(store: &dyn Store, id: &str, number: i64, updated_at: i64) {
    store
        .update_publication(id, &Update::BranchAttempt, updated_at)
        .unwrap();
    store
        .update_publication(id, &Update::BranchCreated, updated_at)
        .unwrap();
    store
        .update_publication(
            id,
            &Update::PullRequest {
                number,
                node_id: format!("PR_{number}"),
                html_url: format!("https://github.example/acme/widgets/pull/{number}"),
            },
            updated_at,
        )
        .unwrap();
    store
        .update_publication(
            id,
            &Update::Merged {
                merge_commit: format!("{number:040x}"),
            },
            updated_at,
        )
        .unwrap();
}

fn scoped_token(
    store: &dyn Store,
    provider: &str,
    host: &str,
    user_id: i64,
    name: &str,
    repository_id: Option<i64>,
    read_only: bool,
) -> String {
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: provider.into(),
                host: host.into(),
                user_id,
            },
            &format!("user-{provider}-{user_id}"),
        )
        .unwrap();
    match repository_id {
        Some(repository_id) => {
            mint_bound_agent_token(
                store,
                &owner.id,
                name,
                REPOSITORY,
                repository_id,
                3600,
                read_only,
            )
            .unwrap()
            .0
        }
        None => {
            assert!(!read_only, "unbound test tokens are writable");
            mint_agent_token(store, &owner.id, name, REPOSITORY, 3600)
                .unwrap()
                .0
        }
    }
}

fn projection_scope_contract(store: &dyn Store) {
    let now = unix_now();
    let writer = scoped_token(store, "github", "github.com", 1, "writer", Some(100), false);
    let reader = scoped_token(store, "github", "github.com", 1, "reader", Some(100), true);
    let wrong_host = scoped_token(store, "github", "ghe.example", 2, "host", Some(100), false);
    let wrong_provider = scoped_token(
        store,
        "gitlab",
        "gitlab.com",
        3,
        "provider",
        Some(100),
        false,
    );
    let wrong_id = scoped_token(
        store,
        "github",
        "github.com",
        4,
        "stable-id",
        Some(101),
        false,
    );
    let null_viewer = scoped_token(
        store,
        "github",
        "github.com",
        5,
        "unbound-viewer",
        None,
        false,
    );
    let null_publisher = scoped_token(
        store,
        "github",
        "github.com",
        6,
        "unbound-publisher",
        None,
        false,
    );

    let cases = [
        (&writer, "feat/visible", "Visible", 1),
        (&wrong_host, "feat/wrong-host", "Wrong host", 2),
        (&wrong_provider, "feat/wrong-provider", "Wrong provider", 3),
        (&wrong_id, "feat/wrong-id", "Wrong stable id", 4),
    ];
    let mut visible = String::new();
    for (token, branch, title, number) in cases {
        let id = create_publication(store, token, REPOSITORY, branch, title, now - 100);
        merge_publication(store, &id, number, now - 10);
        if token == &writer {
            visible = id;
        }
    }
    let null_id = create_publication(
        store,
        &null_publisher,
        REPOSITORY,
        "feat/unbound-visible",
        "Visible in the unbound namespace",
        now - 100,
    );
    merge_publication(store, &null_id, 6, now - 10);
    let recap = store.work_merge_recap(&writer, now - 20, now, now).unwrap();
    assert_eq!(recap.merges.len(), 1);
    assert_eq!(recap.merges[0].id, visible);
    assert_eq!(recap.scanned_count, 1);
    assert!(recap.complete);
    assert_eq!(
        store.work_merge_recap(&reader, now - 20, now, now).unwrap(),
        recap,
        "a read-only token must receive the same repository projection"
    );
    let null_recap = store
        .work_merge_recap(&null_viewer, now - 20, now, now)
        .unwrap();
    assert_eq!(null_recap.merges.len(), 1);
    assert_eq!(null_recap.merges[0].id, null_id);
    assert!(null_recap.merges.iter().all(|merge| merge.id != visible));
    assert!(recap.merges.iter().all(|merge| merge.id != null_id));

    let large = publication(REPOSITORY, "feat/large-manifest", "Large manifest");
    let mut large = serde_json::to_value(large.manifest).unwrap();
    large["body"] = json!("z".repeat(64 * 1024));
    large["message"] = json!(format!("{}\n", "m".repeat(64 * 1024 - 1)));
    large["changes"] = json!(
        (0..1_000)
            .map(|index| json!({
                "path": format!("private-manifest-marker-{index:04}-{}", "x".repeat(950)),
                "mode": "100644", "blob": "e".repeat(40),
                "sha256": "f".repeat(64), "size": 5
            }))
            .collect::<Vec<_>>()
    );
    let large = NewPublication {
        id: derive_id(REPOSITORY, 3, &"b".repeat(40), "feat/large-manifest"),
        manifest: serde_json::from_value(large).unwrap(),
    };
    let large_id = large.id.clone();
    assert!(matches!(
        store.create_publication(&writer, &large, now - 90).unwrap(),
        Create::Done(_)
    ));
    merge_publication(store, &large_id, 5, now - 9);
    let bounded = store.work_merge_recap(&writer, now - 20, now, now).unwrap();
    let encoded = serde_json::to_string(&bounded).unwrap();
    assert!(encoded.len() < 8_192);
    assert!(!encoded.contains("private-manifest-marker"));

    for (branch, merge_commit, updated_at) in [
        ("feat/malformed-merge", "not-hex".to_owned(), now + 1),
        ("feat/oversized-merge", "a".repeat(4_096), now + 2),
    ] {
        let id = create_publication(store, &writer, REPOSITORY, branch, branch, now - 80);
        store
            .update_publication(&id, &Update::BranchAttempt, updated_at)
            .unwrap();
        store
            .update_publication(&id, &Update::BranchCreated, updated_at)
            .unwrap();
        store
            .update_publication(
                &id,
                &Update::PullRequest {
                    number: updated_at,
                    node_id: format!("PR_{updated_at}"),
                    html_url: format!("https://github.example/acme/widgets/pull/{updated_at}"),
                },
                updated_at,
            )
            .unwrap();
        store
            .update_publication(&id, &Update::Merged { merge_commit }, updated_at)
            .unwrap();
        assert!(
            store
                .work_merge_recap(&writer, updated_at, updated_at, now + 3)
                .is_err(),
            "invalid retained merge identity must make the recap unavailable"
        );
    }
}

#[test]
fn sqlite_projection_enforces_complete_repository_identity_and_bounds() {
    let directory = TempDir::new().unwrap();
    let store = store::open(&format!(
        "sqlite://{}",
        directory.path().join("projection.sqlite").display()
    ))
    .unwrap();
    projection_scope_contract(&*store);
}

#[test]
fn postgres_projection_enforces_complete_repository_identity_and_bounds() {
    let Some(database) = PgSchema::create() else {
        return;
    };
    let store = store::open(&database.url).unwrap();
    projection_scope_contract(&*store);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overview_and_start_recap_are_scoped_read_only_and_fact_labeled() {
    let hub = Hub::start().await;
    let (writer_id, writer) = hub.token(1, "writer", REPOSITORY);
    let (other_id, other) = hub.token(2, "other", "acme/other");
    let now = unix_now();
    let merged_id = create_publication(
        &*hub.store,
        &writer_id,
        REPOSITORY,
        "feat/old",
        "Old creation, recent merge",
        now - 10_000,
    );
    merge_publication(&*hub.store, &merged_id, 17, now - 5);
    let unmerged_id = create_publication(
        &*hub.store,
        &writer_id,
        REPOSITORY,
        "feat/open",
        "Still open",
        now - 4,
    );
    let foreign_id = create_publication(
        &*hub.store,
        &other_id,
        "acme/other",
        "feat/foreign",
        "Foreign merge",
        now - 3,
    );
    merge_publication(&*hub.store, &foreign_id, 18, now - 2);

    let completed = hub
        .mcp(
            &writer,
            "agentscm_start",
            json!({"task":"Reported completion only","request_key":"reported-only",
                "session":"reported-session"}),
        )
        .await;
    let item = &completed["structuredContent"]["work"]["item"];
    let finished = hub
        .mcp(
            &writer,
            "agentscm_work_checkpoint",
            json!({"work_id":item["id"],"operation_id":"finish-reported","generation":item["generation"],
                "expected_updated_seq":item["updated_seq"],"session":"reported-session",
                "kind":"finish","summary":"Agent reported completion"}),
        )
        .await;
    assert_eq!(finished["isError"], false, "{finished}");

    let event_count = hub.store.events_after(0, 10_000).unwrap().len();
    assert_eq!(
        hub.call(
            Method::GET,
            &format!("/v1/hub/standup/overview?recent_since={}", now - 100),
            None,
            None,
        )
        .await
        .0,
        401
    );
    let (status, overview) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/overview?recent_since={}", now - 100),
            Some(&writer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{overview}");
    let recap = &overview["merge_recap"];
    assert_eq!(recap["since"], now - 100);
    assert!(recap["until"].as_i64().is_some_and(|until| until >= now));
    assert_eq!(recap["time_basis"], "last_recorded_update");
    assert_eq!(recap["scope"], "tracked_publications");
    assert_eq!(recap["complete"], true);
    assert_eq!(recap["scanned_count"], 1);
    assert_eq!(recap["merges"].as_array().unwrap().len(), 1);
    let merge = &recap["merges"][0];
    assert_eq!(merge["id"], merged_id);
    assert_eq!(merge["title"], "Old creation, recent merge");
    assert_eq!(merge["branch"], "feat/old");
    assert_eq!(merge["base"], "main");
    assert_eq!(merge["head_commit"], "b".repeat(40));
    assert_eq!(merge["merge_commit"], format!("{:040x}", 17));
    assert_eq!(merge["pull_number"], 17);
    assert_eq!(merge["updated_at"], now - 5);
    assert!(!overview.to_string().contains(&unmerged_id));
    assert!(!overview.to_string().contains(&foreign_id));
    assert!(!recap.to_string().contains("Reported completion only"));

    let mcp = hub
        .mcp(
            &writer,
            "agentscm_work_overview",
            json!({"recent_since":now - 100}),
        )
        .await;
    assert_eq!(mcp["isError"], false, "{mcp}");
    assert_eq!(
        mcp["structuredContent"]["merge_recap"]["merges"],
        recap["merges"]
    );
    assert_eq!(
        mcp["structuredContent"]["merge_recap"]["since"],
        recap["since"]
    );
    assert_eq!(
        hub.store.events_after(0, 10_000).unwrap().len(),
        event_count,
        "overview recap reads wrote a Hub event"
    );

    let startup = hub
        .mcp(
            &writer,
            "agentscm_start",
            json!({"task":"Begin after recap","request_key":"after-recap","session":"next-session"}),
        )
        .await;
    assert_eq!(startup["isError"], false, "{startup}");
    assert_eq!(
        startup["structuredContent"]["merge_recap"]["merges"][0]["id"],
        merged_id
    );
    assert!(startup["structuredContent"]["briefing"]["delivery"].is_null());
    assert_eq!(startup["structuredContent"]["acknowledged"], false);

    let (_, foreign) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/overview?recent_since={}", now - 100),
            Some(&other),
            None,
        )
        .await;
    assert_eq!(foreign["merge_recap"]["merges"][0]["id"], foreign_id);
    assert!(!foreign.to_string().contains(&merged_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recap_reports_match_truncation_and_ignores_creation_order() {
    let hub = Hub::start().await;
    let (token, secret) = hub.token(1, "writer", REPOSITORY);
    let now = unix_now();
    let mut tied_ids = Vec::new();
    for number in 1..=21 {
        let id = create_publication(
            &*hub.store,
            &token,
            REPOSITORY,
            &format!("feat/recent-{number}"),
            &format!("Recent {number}"),
            now - 100,
        );
        let updated_at = if number <= 2 { now - 1 } else { now - number };
        merge_publication(&*hub.store, &id, number, updated_at);
        if number <= 2 {
            tied_ids.push(id);
        }
    }
    let (_, first) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/overview?recent_since={}", now - 200),
            Some(&secret),
            None,
        )
        .await;
    assert_eq!(first["merge_recap"]["merges"].as_array().unwrap().len(), 20);
    assert_eq!(first["merge_recap"]["complete"], false);
    assert_eq!(first["merge_recap"]["scanned_count"], 21);
    let updates = first["merge_recap"]["merges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["updated_at"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert!(updates.windows(2).all(|pair| pair[0] >= pair[1]));
    tied_ids.sort_by(|left, right| right.cmp(left));
    assert_eq!(first["merge_recap"]["merges"][0]["id"], tied_ids[0]);
    assert_eq!(first["merge_recap"]["merges"][1]["id"], tied_ids[1]);

    let outside_id = create_publication(
        &*hub.store,
        &token,
        REPOSITORY,
        "feat/outside",
        "Old creation at scan boundary",
        now - 30_000,
    );
    merge_publication(&*hub.store, &outside_id, 999, now);
    for number in 22..=199 {
        let id = create_publication(
            &*hub.store,
            &token,
            REPOSITORY,
            &format!("feat/old-{number}"),
            &format!("Old {number}"),
            now - 50,
        );
        merge_publication(&*hub.store, &id, number, now - 40);
    }
    let (_, boundary) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/overview?recent_since={}", now - 30),
            Some(&secret),
            None,
        )
        .await;
    assert_eq!(boundary["merge_recap"]["scanned_count"], 21);
    assert_eq!(boundary["merge_recap"]["complete"], false);
    assert!(boundary["merge_recap"].to_string().contains(&outside_id));

    let newest = create_publication(
        &*hub.store,
        &token,
        REPOSITORY,
        "feat/newest-200",
        "The 201st retained row",
        now - 10,
    );
    merge_publication(&*hub.store, &newest, 200, now - 5);
    let (_, truncated) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/overview?recent_since={}", now - 30),
            Some(&secret),
            None,
        )
        .await;
    assert_eq!(truncated["merge_recap"]["scanned_count"], 21);
    assert_eq!(truncated["merge_recap"]["complete"], false);
    assert!(truncated["merge_recap"].to_string().contains(&outside_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recap_failure_does_not_fail_overview_or_claim() {
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
    assert!(overview["merge_recap"].is_null());
    assert!(overview["current"].is_array());

    let startup = hub
        .mcp(
            &secret,
            "agentscm_start",
            json!({"task":"Claim despite missing recap","request_key":"recap-unavailable",
                "session":"unavailable-session"}),
        )
        .await;
    assert_eq!(startup["isError"], false, "{startup}");
    assert!(startup["structuredContent"]["merge_recap"].is_null());
    assert_eq!(startup["structuredContent"]["work"]["created"], true);
    assert!(
        startup["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Do not infer that no recent merges were recorded")
    );
}
