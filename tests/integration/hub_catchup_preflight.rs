//! HTTP and MCP delivery of durable return-to-work catch-up.

use crate::hub_pg;

use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::HubError;
use agentscm::hub::catchup::{
    CatchupCoverageState, CatchupMergeSource, CatchupRepositoryScope, CatchupSourceStatus,
    NewCatchupMerge,
};
use agentscm::hub::coordination::CoordinationInboxQuery;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token};
use agentscm::hub::work::{CheckpointKind, WorkCheckpoint, WorkRole, WorkStart, WorkState};
use agentscm::hub::work_preflight::{
    WorkPreflightReason, WorkPreflightRequest, WorkPreflightStatus,
};
use agentscm::hub::{HubState, router};
use hub_pg::PgSchema;
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    _dir: TempDir,
}

const REPOSITORY: &str = "acme/widgets";
const REPOSITORY_ID: i64 = 101;

struct StoreFixture {
    store: Arc<dyn Store>,
    _directory: Option<TempDir>,
    _pg: Option<PgSchema>,
}

impl StoreFixture {
    fn sqlite() -> Self {
        let directory = TempDir::new().unwrap();
        let database = format!("sqlite://{}", directory.path().join("hub.sqlite").display());
        Self {
            store: store::open(&database).unwrap(),
            _directory: Some(directory),
            _pg: None,
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            store: store::open(&pg.url).unwrap(),
            _directory: None,
            _pg: Some(pg),
        })
    }
}

fn direct_token(store: &dyn Store, user_id: i64, name: &str) -> String {
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id,
            },
            name,
        )
        .unwrap();
    mint_bound_agent_token(
        store,
        &owner.id,
        name,
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap()
    .0
}

fn repository_scope() -> CatchupRepositoryScope {
    CatchupRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPOSITORY.into(),
        repository_id: Some(REPOSITORY_ID),
    }
}

fn observe_complete_sources(store: &dyn Store, now: i64) {
    for source in [
        CatchupMergeSource::TrackedPublication,
        CatchupMergeSource::ForgePull,
    ] {
        store
            .record_catchup_source_status(
                &repository_scope(),
                &CatchupSourceStatus {
                    source,
                    state: CatchupCoverageState::ObservedOnly,
                    checked_at: now,
                    unread_pages: 0,
                    deferred: 0,
                    window_from: None,
                },
            )
            .unwrap();
    }
}

fn direct_start(
    request_key: &str,
    title: &str,
    session: &str,
    scope: &[&str],
    preflight: Option<WorkPreflightRequest>,
) -> WorkStart {
    WorkStart {
        work_id: None,
        source: None,
        request_key: Some(request_key.into()),
        title: title.into(),
        session: session.into(),
        role: WorkRole::Lead,
        scope: scope.iter().map(|path| (*path).into()).collect(),
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: Vec::new(),
        related_work: Vec::new(),
        takeover_generation: None,
        preflight,
    }
}

fn checked_window(source_revision: &str, relation_checked: bool) -> WorkPreflightRequest {
    WorkPreflightRequest {
        source_revision: source_revision.into(),
        merge_after: 0,
        merge_through: 0,
        source_relation_checked: relation_checked,
        checked_merges: Vec::new(),
    }
}

impl Hub {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let database = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        let store = tokio::task::spawn_blocking(move || store::open(&database))
            .await
            .unwrap()
            .unwrap();
        let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            store,
            client: reqwest::Client::new(),
            _dir: dir,
        }
    }

    async fn token(&self, read_only: bool) -> String {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let owner = store
                .upsert_user(
                    &ForgeIdentity {
                        provider: "github".into(),
                        host: "github.com".into(),
                        user_id: 71,
                    },
                    "catchup-user",
                )
                .unwrap();
            let (_, secret, _) = mint_bound_agent_token(
                &*store,
                &owner.id,
                if read_only { "reader" } else { "writer" },
                "acme/widgets",
                101,
                3600,
                read_only,
            )
            .unwrap();
            secret.expose().to_owned()
        })
        .await
        .unwrap()
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        secret: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(secret);
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let body = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        (status, body)
    }

    async fn mcp(&self, secret: &str, id: i64, method: &str, params: Value) -> Value {
        self.call(
            Method::POST,
            "/mcp",
            secret,
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            })),
        )
        .await
        .1["result"]
            .clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catchup_http_and_mcp_keep_human_and_agent_readership_separate() {
    let hub = Hub::start().await;
    let writer = hub.token(false).await;
    let reader = hub.token(true).await;
    let (status, created) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &writer,
            Some(json!({
                "request_key": "catchup-http",
                "title": "Inspect exact catch-up",
                "session": "writer-session",
                "scope": ["src/hub/work_http.rs"]
            })),
        )
        .await;
    assert_eq!(status, 200, "{created}");

    let (status, first) = hub
        .call(Method::GET, "/v1/hub/standup/catchup", &reader, None)
        .await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["subscriber"], "human");
    assert_eq!(first["complete"], true);
    let source_coverage = first["source_coverage"].as_array().unwrap();
    assert_eq!(source_coverage.len(), 2);
    assert!(source_coverage.iter().all(|source| {
        source["source"].is_string()
            && matches!(
                source["state"].as_str(),
                Some("observed_only" | "incomplete" | "unavailable")
            )
    }));
    assert!(!first["work_events"].as_array().unwrap().is_empty());

    let (status, replay_before_ack) = hub
        .call(Method::GET, "/v1/hub/standup/catchup", &reader, None)
        .await;
    assert_eq!(status, 200, "{replay_before_ack}");
    assert_eq!(replay_before_ack["event_from"], first["event_from"]);
    assert_eq!(replay_before_ack["merge_from"], first["merge_from"]);

    let ack = json!({
        "delivery": first["delivery"],
        "expected_event_from": first["event_from"],
        "expected_merge_from": first["merge_from"],
    });
    let (status, acknowledged) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/catchup/ack",
            &reader,
            Some(ack),
        )
        .await;
    assert_eq!(status, 200, "{acknowledged}");
    assert_eq!(acknowledged["status"], "advanced");

    let (status, next_human) = hub
        .call(Method::GET, "/v1/hub/standup/catchup", &writer, None)
        .await;
    assert_eq!(status, 200, "{next_human}");
    assert_eq!(next_human["event_from"], first["event_through"]);
    assert_eq!(next_human["merge_from"], first["merge_through"]);

    let (status, named_human) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &writer,
            Some(json!({
                "request_key": "session-named-human",
                "title": "Keep agent cursor isolated",
                "session": "human",
                "scope": ["src/hub/hook.rs"]
            })),
        )
        .await;
    assert_eq!(status, 200, "{named_human}");

    let start = hub
        .mcp(
            &writer,
            1,
            "tools/call",
            json!({
                "name": "agentscm_start",
                "arguments": {
                    "task": "Keep agent cursor isolated",
                    "paths": ["src/hub/hook.rs"],
                    "session": "human",
                    "request_key": "session-named-human"
                }
            }),
        )
        .await;
    assert_eq!(
        start["structuredContent"]["catchup"]["subscriber"], "agent:human",
        "{start}"
    );

    let agent = hub
        .mcp(
            &writer,
            2,
            "tools/call",
            json!({
                "name": "agentscm_work_catchup",
                "arguments": {}
            }),
        )
        .await;
    assert_eq!(agent["isError"], false, "{agent}");
    assert_eq!(agent["structuredContent"]["subscriber"], "agent:default");
    assert_eq!(agent["structuredContent"]["event_from"], 0);

    let tools = hub.mcp(&reader, 3, "tools/list", json!({})).await;
    let names: Vec<_> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"agentscm_work_catchup"));
    assert!(names.contains(&"agentscm_work_catchup_ack"));
    assert!(names.contains(&"agentscm_work_preflight"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catchup_continuation_requires_both_fixed_highwaters_and_cursors() {
    let hub = Hub::start().await;
    let reader = hub.token(true).await;
    let (status, body) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/catchup?delivery=cup_00000000000000000000000000000000&event_after=0",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 400, "{body}");

    let result = hub
        .mcp(
            &reader,
            4,
            "tools/call",
            json!({
                "name": "agentscm_work_catchup",
                "arguments": {
                    "delivery": "cup_00000000000000000000000000000000",
                    "event_after": 0
                }
            }),
        )
        .await;
    assert_eq!(result["isError"], true, "{result}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_is_read_only_and_missing_evidence_is_unknown() {
    let hub = Hub::start().await;
    let reader = hub.token(true).await;
    let request = json!({
        "request_key": "read-only-preflight",
        "title": "Inspect admission without claiming",
        "session": "preflight-http",
        "scope": ["src/hub/work.rs"]
    });
    let (status, result) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight",
            &reader,
            Some(request),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["status"], "unknown");
    assert_eq!(result["complete"], false);
    assert_eq!(result["subject_match"], Value::Null);
    assert!(
        result["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "missing_source")
    );

    let (status, search) = hub
        .call(Method::GET, "/v1/hub/standup/work", &reader, None)
        .await;
    assert_eq!(status, 200, "{search}");
    assert!(search["items"].as_array().unwrap().is_empty());

    let result = hub
        .mcp(
            &reader,
            5,
            "tools/call",
            json!({
                "name": "agentscm_work_preflight",
                "arguments": {
                    "task": "Inspect admission without claiming",
                    "paths": ["src/hub/mcp.rs"],
                    "request_key": "read-only-mcp-preflight"
                }
            }),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["structuredContent"]["status"], "unknown");
    assert_eq!(result["structuredContent"]["complete"], false);

    let (status, search) = hub
        .call(Method::GET, "/v1/hub/standup/work", &reader, None)
        .await;
    assert_eq!(status, 200, "{search}");
    assert!(search["items"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_merge_highwater_refuses_new_work_without_claiming_it() {
    let hub = Hub::start().await;
    let writer = hub.token(false).await;
    let reader = hub.token(true).await;
    let scope = CatchupRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: "acme/widgets".into(),
        repository_id: Some(101),
    };
    let store = hub.store.clone();
    let status_scope = scope.clone();
    tokio::task::spawn_blocking(move || {
        for source in [
            CatchupMergeSource::TrackedPublication,
            CatchupMergeSource::ForgePull,
        ] {
            store
                .record_catchup_source_status(
                    &status_scope,
                    &CatchupSourceStatus {
                        source,
                        state: CatchupCoverageState::ObservedOnly,
                        checked_at: 10,
                        unread_pages: 0,
                        deferred: 0,
                        window_from: None,
                    },
                )
                .unwrap();
        }
    })
    .await
    .unwrap();
    let request = json!({
        "request_key": "atomic-preflight",
        "title": "Start only from inspected source",
        "session": "atomic-session",
        "scope": ["src/hub/work.rs"],
        "preflight": {
            "source_revision": "cccccccccccccccccccccccccccccccccccccccc",
            "merge_after": 0,
            "merge_through": 0,
            "source_relation_checked": true,
            "checked_merges": []
        }
    });
    let (status, preview) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight",
            &reader,
            Some(request.clone()),
        )
        .await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(
        preview["status"], "clear_within_checked_window",
        "{preview}"
    );
    assert_eq!(preview["complete"], true, "{preview}");

    let store = hub.store.clone();
    tokio::task::spawn_blocking(move || {
        store
            .record_catchup_merge(
                &scope,
                &NewCatchupMerge {
                    source: CatchupMergeSource::ForgePull,
                    source_id: "forge-pr-17".into(),
                    occurred_at: Some(9),
                    title: Some("Merged exact source".into()),
                    branch: "feature/merged".into(),
                    base: "main".into(),
                    head_commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    merge_commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                    pull_number: 17,
                    paths: vec!["src/hub/work.rs".into()],
                    paths_complete: true,
                },
                10,
            )
            .unwrap();
    })
    .await
    .unwrap();

    let (status, refused) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &writer,
            Some(request),
        )
        .await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(refused["error"], "work_preflight_required");
    assert_eq!(refused["preflight"]["current_merge_through"], 1);
    assert!(
        refused["preflight"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "merge_highwater_changed")
    );

    let exact_old_merge = json!({
        "request_key": "exact-old-merge",
        "title": "Detect an already merged exact source",
        "session": "exact-source-session",
        "scope": ["src/hub/work.rs"],
        "preflight": {
            "source_revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "merge_after": 1,
            "merge_through": 1,
            "source_relation_checked": true,
            "checked_merges": [{
                "source": "forge_pull",
                "source_id": "forge-pr-17",
                "pull_number": 17,
                "merge_commit": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "in_source": false
            }]
        }
    });
    let (status, duplicate) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight",
            &reader,
            Some(exact_old_merge),
        )
        .await;
    assert_eq!(status, 200, "{duplicate}");
    assert_eq!(duplicate["status"], "review_required", "{duplicate}");
    assert!(duplicate["merges"].as_array().unwrap().is_empty());
    assert_eq!(
        duplicate["checked_observations"].as_array().unwrap().len(),
        1
    );
    assert!(
        duplicate["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "source_head_already_merged")
    );
    assert!(
        duplicate["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "source_merge_missing")
    );

    let (status, search) = hub
        .call(Method::GET, "/v1/hub/standup/work", &reader, None)
        .await;
    assert_eq!(status, 200, "{search}");
    assert!(search["items"].as_array().unwrap().is_empty(), "{search}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_explicit_work_id_can_retry_already_owned_work_with_unknown_preflight() {
    let hub = Hub::start().await;
    let writer = hub.token(false).await;
    let (status, created) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &writer,
            Some(json!({
                "request_key": "owned-retry",
                "title": "Resume exact owned work",
                "session": "owned-session",
                "scope": ["src/hub/work.rs"]
            })),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    let work_id = created["item"]["id"].as_str().unwrap().to_owned();

    let store = hub.store.clone();
    tokio::task::spawn_blocking(move || {
        store
            .record_catchup_merge(
                &CatchupRepositoryScope {
                    provider: "github".into(),
                    host: "github.com".into(),
                    repository: "acme/widgets".into(),
                    repository_id: Some(101),
                },
                &NewCatchupMerge {
                    source: CatchupMergeSource::ForgePull,
                    source_id: "forge-pr-22".into(),
                    occurred_at: Some(20),
                    title: None,
                    branch: "feature/other".into(),
                    base: "main".into(),
                    head_commit: "dddddddddddddddddddddddddddddddddddddddd".into(),
                    merge_commit: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
                    pull_number: 22,
                    paths: vec!["src/hub/work.rs".into()],
                    paths_complete: true,
                },
                20,
            )
            .unwrap();
    })
    .await
    .unwrap();

    let preflight = json!({
        "source_revision": "ffffffffffffffffffffffffffffffffffffffff",
        "merge_after": 0,
        "merge_through": 0,
        "source_relation_checked": false,
        "checked_merges": []
    });
    let (status, request_key_retry) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &writer,
            Some(json!({
                "request_key": "owned-retry",
                "title": "Resume exact owned work",
                "session": "owned-session",
                "scope": ["src/hub/work.rs"],
                "preflight": preflight
            })),
        )
        .await;
    assert_eq!(status, 409, "{request_key_retry}");

    let (status, explicit_retry) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &writer,
            Some(json!({
                "work_id": work_id,
                "title": "Resume exact owned work",
                "session": "owned-session",
                "scope": ["src/hub/work.rs"],
                "preflight": preflight
            })),
        )
        .await;
    assert_eq!(status, 200, "{explicit_retry}");
    assert_eq!(explicit_retry["created"], false);
    assert_eq!(explicit_retry["preflight"]["status"], "unknown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_mcp_claim_succeeds_while_source_readiness_stays_unknown() {
    let hub = Hub::start().await;
    let token = hub.token(false).await;
    let arguments = json!({
        "task":"Inspect the exact source", "session":"legacy-start",
        "request_key":"legacy-start", "paths":["src/example.rs"]
    });
    let first = hub
        .mcp(
            &token,
            10,
            "tools/call",
            json!({
                "name":"agentscm_start", "arguments":arguments
            }),
        )
        .await;
    assert_eq!(first["isError"], false, "{first}");
    let content = &first["structuredContent"];
    assert_eq!(content["claim_acquired"], true);
    assert_eq!(content["source_preflight_ready"], false);
    assert_eq!(content["ready_for_work"], false);
    assert_eq!(content["work"]["created"], true);
    assert_eq!(content["work"]["preflight"]["status"], "unknown");
    let retry = hub
        .mcp(
            &token,
            11,
            "tools/call",
            json!({
                "name":"agentscm_start", "arguments":arguments
            }),
        )
        .await;
    assert_eq!(retry["isError"], false, "{retry}");
    assert_eq!(
        retry["structuredContent"]["work"]["item"]["id"],
        content["work"]["item"]["id"]
    );
    assert_eq!(retry["structuredContent"]["work"]["created"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_partial_delivery_can_be_continued_through_mcp() {
    let hub = Hub::start().await;
    let token = hub.token(false).await;
    let forged_title = "FORGE_TITLE_DO_NOT_TREAT_AS_STARTUP_INSTRUCTIONS";
    hub.store
        .record_catchup_merge(
            &CatchupRepositoryScope {
                provider: "github".into(),
                host: "github.com".into(),
                repository: "acme/widgets".into(),
                repository_id: Some(101),
            },
            &NewCatchupMerge {
                source: CatchupMergeSource::ForgePull,
                source_id: "pull:7".into(),
                occurred_at: None,
                title: Some(forged_title.into()),
                branch: "feature".into(),
                base: "main".into(),
                head_commit: "a".repeat(40),
                merge_commit: "b".repeat(40),
                pull_number: 7,
                paths: Vec::new(),
                paths_complete: false,
            },
            store::unix_now(),
        )
        .unwrap();
    for index in 0..21 {
        let (status, body) = hub
            .call(
                Method::POST,
                "/v1/hub/standup/start",
                &token,
                Some(json!({
                    "request_key":format!("hook-history-{index}"), "title":format!("Work {index}"),
                    "session":format!("history-{index}")
                })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
    }

    let startup = hub
        .mcp(
            &token,
            19,
            "tools/call",
            json!({
                "name":"agentscm_start",
                "arguments": {
                    "task":"Inspect the delivery-free preview",
                    "session":"preview-session",
                    "request_key":"preview-session",
                    "paths":["src/preview.rs"]
                }
            }),
        )
        .await;
    let preview = &startup["structuredContent"]["catchup"];
    assert!(preview.is_object(), "{startup}");
    assert!(preview.get("delivery").is_none(), "{startup}");
    assert!(preview.get("delivery_client").is_none(), "{startup}");
    assert_eq!(
        startup["structuredContent"]["catchup_acknowledgement_required"], false,
        "{startup}"
    );

    let (status, reply) = hub.call(Method::POST, "/v1/hub/hook/generic", &token, Some(json!({
        "agent":"test-agent", "session":"hook-continuation", "event":"start", "task":"Inspect source"
    }))).await;
    assert_eq!(status, 200, "{reply}");
    let text = reply["text"].as_str().unwrap();
    assert!(
        !text.contains(forged_title),
        "forge titles must not enter automatic hook instructions"
    );
    assert!(text.contains("AgentSCM catch-up preview"), "{text}");
    assert!(text.contains("pull:7"), "{text}");
    assert!(text.contains(&"a".repeat(40)), "{text}");
    assert!(text.contains(&"b".repeat(40)), "{text}");
    assert!(
        text.contains("automatic preview creates no delivery or acknowledgement"),
        "{text}"
    );
    let instruction = text
        .lines()
        .find(|line| line.contains("this exact JSON"))
        .expect(text);
    let arguments: Value =
        serde_json::from_str(&instruction[instruction.find('{').unwrap()..]).unwrap();
    assert_eq!(arguments["subscriber"], "agent:hook-continuation");
    assert!(
        arguments["delivery_client"]
            .as_str()
            .unwrap()
            .contains("hook")
    );
    let first = hub
        .mcp(
            &token,
            20,
            "tools/call",
            json!({
                "name":"agentscm_work_catchup", "arguments":arguments
            }),
        )
        .await;
    assert_eq!(first["isError"], false, "{first}");
    let first_page = &first["structuredContent"];
    assert_eq!(first_page["complete"], false, "{first}");
    assert_eq!(first_page["subscriber"], arguments["subscriber"]);
    assert_eq!(first_page["delivery_client"], arguments["delivery_client"]);
    assert!(first_page["delivery"].is_string(), "{first}");

    let continuation = json!({
        "subscriber": first_page["subscriber"],
        "delivery_client": first_page["delivery_client"],
        "delivery": first_page["delivery"],
        "event_after": first_page["next_event_after"],
        "event_through": first_page["event_through"],
        "merge_after": first_page["next_merge_after"],
        "merge_through": first_page["merge_through"],
        "limit": 20
    });
    let continued = hub
        .mcp(
            &token,
            21,
            "tools/call",
            json!({
                "name":"agentscm_work_catchup", "arguments":continuation
            }),
        )
        .await;
    assert_eq!(continued["isError"], false, "{continued}");
    let complete_page = &continued["structuredContent"];
    assert_eq!(
        complete_page["delivery"], first_page["delivery"],
        "{continued}"
    );
    assert_eq!(complete_page["complete"], true, "{continued}");

    let acknowledged = hub
        .mcp(
            &token,
            22,
            "tools/call",
            json!({
                "name":"agentscm_work_catchup_ack",
                "arguments": {
                    "delivery": complete_page["delivery"],
                    "expected_event_from": complete_page["event_from"],
                    "expected_merge_from": complete_page["merge_from"]
                }
            }),
        )
        .await;
    assert_eq!(acknowledged["isError"], false, "{acknowledged}");
    assert_eq!(
        acknowledged["structuredContent"]["status"], "advanced",
        "{acknowledged}"
    );
}

fn explicit_preflight_overlap_coordinates_without_claiming(fixture: &StoreFixture) {
    let incumbent = direct_token(&*fixture.store, 801, "preflight-incumbent");
    let requester = direct_token(&*fixture.store, 802, "preflight-requester");
    let now = store::unix_now();
    observe_complete_sources(&*fixture.store, now);

    let incumbent_work = fixture
        .store
        .work_start(
            &incumbent,
            &direct_start(
                "preflight-incumbent",
                "Own the shared preflight path",
                "incumbent-session",
                &["src/shared-preflight.rs"],
                None,
            ),
            now,
        )
        .unwrap()
        .item;
    let proposed = direct_start(
        "preflight-overlap",
        "Coordinate the shared preflight path",
        "requester-session",
        &["src/shared-preflight.rs", "src/requester-only.rs"],
        Some(checked_window(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            true,
        )),
    );

    let first = fixture
        .store
        .work_start(&requester, &proposed, now + 1)
        .unwrap();
    assert!(!first.created);
    assert!(!first.joined);
    assert!(!first.took_over);
    assert_eq!(first.item.id, incumbent_work.id);
    assert_eq!(
        first.conflict.as_ref().unwrap().kind,
        agentscm::hub::work::WorkConflictKind::ScopeOverlap
    );
    let preflight = first.preflight.as_ref().unwrap();
    assert_eq!(preflight.status, WorkPreflightStatus::ReviewRequired);
    assert!(
        preflight
            .reasons
            .contains(&WorkPreflightReason::ActiveScopeOverlap)
    );
    let coordination = first.coordination_request.as_ref().unwrap();
    assert_eq!(coordination.targets.len(), 1);
    assert_eq!(coordination.targets[0].work_id, incumbent_work.id);

    let replay = fixture
        .store
        .work_start(&requester, &proposed, now + 2)
        .unwrap();
    assert_eq!(
        replay.coordination_request.as_ref().unwrap().id,
        coordination.id
    );
    let inbox = fixture
        .store
        .coordination_inbox(
            &incumbent,
            &CoordinationInboxQuery {
                session: Some("incumbent-session".into()),
                ..CoordinationInboxQuery::default()
            },
            now + 3,
        )
        .unwrap();
    assert_eq!(inbox.requests.len(), 1);
    assert_eq!(inbox.requests[0].id, coordination.id);

    let work = fixture
        .store
        .work_search(&requester, None, None, 50, now + 3)
        .unwrap();
    assert_eq!(work.items.len(), 1);
    assert_eq!(work.items[0].id, incumbent_work.id);
    assert!(
        fixture
            .store
            .live_intents(REPOSITORY, now + 3, 100)
            .unwrap()
            .is_empty()
    );

    // This reaches the defensive postcondition: the overlap preflight selects the coordination
    // path, while start_in's existing same-session rule returns SessionGoalChanged first. Only an
    // actual scope-overlap coordination result may escape the transaction.
    let same_session_overlap = direct_start(
        "preflight-own-session-divergence",
        "Do not replace own active work through overlap coordination",
        "incumbent-session",
        &["src/shared-preflight.rs"],
        Some(checked_window(
            "1111111111111111111111111111111111111111",
            false,
        )),
    );
    assert!(matches!(
        fixture
            .store
            .work_start(&incumbent, &same_session_overlap, now + 4),
        Err(HubError::WorkPreflight(result))
            if result.status == WorkPreflightStatus::Unknown
                && result.subject_match.is_none()
                && !result.scope_overlaps.is_empty()
    ));
    let after_same_session = fixture
        .store
        .work_get(&incumbent, &incumbent_work.id, now + 4)
        .unwrap()
        .unwrap();
    assert_eq!(after_same_session.generation, incumbent_work.generation);
    assert_eq!(after_same_session.lead_token, incumbent_work.lead_token);
    assert_eq!(after_same_session.state, WorkState::Active);

    let paused_incumbent = fixture
        .store
        .work_checkpoint(
            &incumbent,
            &WorkCheckpoint {
                work_id: incumbent_work.id.clone(),
                operation_id: "pause-preflight-incumbent".into(),
                generation: incumbent_work.generation,
                session: "incumbent-session".into(),
                expected_updated_seq: Some(after_same_session.updated_seq),
                kind: CheckpointKind::Pause,
                summary: "Pause while another proposed subject overlaps the same scope".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: Vec::new(),
                depends_on: Vec::new(),
                related_work: Vec::new(),
            },
            now + 5,
        )
        .unwrap()
        .item;
    let paused_overlap = direct_start(
        "preflight-paused-overlap",
        "Coordinate before claiming paused overlapping scope",
        "paused-overlap-session",
        &["src/shared-preflight.rs"],
        Some(checked_window(
            "2222222222222222222222222222222222222222",
            false,
        )),
    );
    let paused_coordination = fixture
        .store
        .work_start(&incumbent, &paused_overlap, now + 6)
        .unwrap();
    assert!(!paused_coordination.created);
    assert!(!paused_coordination.joined);
    assert!(!paused_coordination.took_over);
    assert_eq!(
        paused_coordination.conflict.as_ref().unwrap().kind,
        agentscm::hub::work::WorkConflictKind::ScopeOverlap
    );
    assert!(paused_coordination.coordination_request.is_some());

    // Exact-subject helper and foreign takeover proposals use the generic preflight gate,
    // rather than the new-subject overlap coordination path exercised above.
    let mut helper_join = direct_start(
        "unused-helper-id",
        "A non-clear preflight cannot join paused Work",
        "helper-overlap-session",
        &["src/shared-preflight.rs"],
        Some(checked_window(
            "3333333333333333333333333333333333333333",
            false,
        )),
    );
    helper_join.request_key = None;
    helper_join.work_id = Some(paused_incumbent.id.clone());
    helper_join.role = WorkRole::Helper;
    helper_join.takeover_generation = Some(paused_incumbent.generation);
    assert!(matches!(
        fixture
            .store
            .work_start(&incumbent, &helper_join, now + 7),
        Err(HubError::WorkPreflight(result)) if result.status == WorkPreflightStatus::Unknown
            && result.subject_match.as_ref().is_some_and(|item| item.id == paused_incumbent.id)
    ));

    let mut foreign_takeover = helper_join.clone();
    foreign_takeover.role = WorkRole::Lead;
    foreign_takeover.session = "foreign-generation-session".into();
    assert!(matches!(
        fixture
            .store
            .work_start(&requester, &foreign_takeover, now + 8),
        Err(HubError::WorkPreflight(result)) if result.status == WorkPreflightStatus::Unknown
            && result.subject_match.as_ref().is_some_and(|item| item.id == paused_incumbent.id)
    ));
    let still_paused = fixture
        .store
        .work_get(&incumbent, &paused_incumbent.id, now + 8)
        .unwrap()
        .unwrap();
    assert_eq!(still_paused.state, WorkState::Paused);
    assert_eq!(still_paused.generation, paused_incumbent.generation);
    assert_eq!(still_paused.lead_token, paused_incumbent.lead_token);
    assert_eq!(still_paused.participants, paused_incumbent.participants);

    let unrelated_unknown = direct_start(
        "preflight-unrelated-unknown",
        "Do not claim unknown unrelated work",
        "unknown-session",
        &["docs/unrelated-preflight.md"],
        Some(checked_window(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            false,
        )),
    );
    assert!(matches!(
        fixture
            .store
            .work_start(&requester, &unrelated_unknown, now + 9),
        Err(HubError::WorkPreflight(result))
            if result.status == WorkPreflightStatus::Unknown
                && result.scope_overlaps.is_empty()
    ));

    let owned = fixture
        .store
        .work_start(
            &incumbent,
            &direct_start(
                "owned-generation-retry",
                "Resume generation-checked owned work",
                "owned-generation-session",
                &["docs/owned-generation.md"],
                None,
            ),
            now + 10,
        )
        .unwrap()
        .item;
    let paused = fixture
        .store
        .work_checkpoint(
            &incumbent,
            &WorkCheckpoint {
                work_id: owned.id.clone(),
                operation_id: "pause-owned-generation".into(),
                generation: owned.generation,
                session: "owned-generation-session".into(),
                expected_updated_seq: Some(owned.updated_seq),
                kind: CheckpointKind::Pause,
                summary: "Pause before an explicit generation-checked resume".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: Vec::new(),
                depends_on: Vec::new(),
                related_work: Vec::new(),
            },
            now + 11,
        )
        .unwrap()
        .item;
    let mut exact_retry = direct_start(
        "unused-explicit-id",
        "Resume generation-checked owned work",
        "owned-generation-resume",
        &["docs/owned-generation.md"],
        Some(checked_window(
            "cccccccccccccccccccccccccccccccccccccccc",
            false,
        )),
    );
    exact_retry.request_key = None;
    exact_retry.work_id = Some(paused.id.clone());
    exact_retry.takeover_generation = Some(paused.generation);
    let resumed = fixture
        .store
        .work_start(&incumbent, &exact_retry, now + 12)
        .unwrap();
    assert_eq!(resumed.item.id, paused.id);
    assert!(!resumed.created);
    assert!(resumed.took_over);
    assert_eq!(
        resumed.preflight.as_ref().unwrap().status,
        WorkPreflightStatus::Unknown
    );
}

fn empty_window_still_detects_exact_retained_source_head(fixture: &StoreFixture) {
    let token = direct_token(&*fixture.store, 803, "preflight-source-head");
    let now = store::unix_now();
    observe_complete_sources(&*fixture.store, now);
    let head = "dddddddddddddddddddddddddddddddddddddddd";
    fixture
        .store
        .record_catchup_merge(
            &repository_scope(),
            &NewCatchupMerge {
                source: CatchupMergeSource::ForgePull,
                source_id: "source-head-pr-31".into(),
                occurred_at: Some(now - 1),
                title: Some("Merged retained source head".into()),
                branch: "feature/source-head".into(),
                base: "main".into(),
                head_commit: head.into(),
                merge_commit: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
                pull_number: 31,
                paths: vec!["src/source-head.rs".into()],
                paths_complete: true,
            },
            now,
        )
        .unwrap();

    let mut duplicate_request = direct_start(
        "retained-source-head",
        "Check retained source head",
        "source-head-session",
        &["src/source-head.rs"],
        Some(WorkPreflightRequest {
            source_revision: head.into(),
            merge_after: 1,
            merge_through: 1,
            source_relation_checked: true,
            checked_merges: Vec::new(),
        }),
    );
    let duplicate = fixture
        .store
        .work_preflight(&token, &duplicate_request, now + 1)
        .unwrap();
    assert_eq!(duplicate.status, WorkPreflightStatus::ReviewRequired);
    assert!(duplicate.merges.is_empty());
    assert!(duplicate.checked_observations.is_empty());
    assert!(
        duplicate
            .reasons
            .contains(&WorkPreflightReason::SourceHeadAlreadyMerged)
    );

    duplicate_request.request_key = Some("unrelated-source-head".into());
    duplicate_request
        .preflight
        .as_mut()
        .unwrap()
        .source_revision = "ffffffffffffffffffffffffffffffffffffffff".into();
    let unrelated = fixture
        .store
        .work_preflight(&token, &duplicate_request, now + 2)
        .unwrap();
    assert_eq!(
        unrelated.status,
        WorkPreflightStatus::ClearWithinCheckedWindow
    );
    assert!(unrelated.complete);
    assert!(
        !unrelated
            .reasons
            .contains(&WorkPreflightReason::SourceHeadAlreadyMerged)
    );
    assert!(
        fixture
            .store
            .work_search(&token, None, None, 50, now + 2)
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
fn sqlite_explicit_preflight_overlap_coordinates_without_claiming() {
    explicit_preflight_overlap_coordinates_without_claiming(&StoreFixture::sqlite());
}

#[test]
fn postgres_explicit_preflight_overlap_coordinates_without_claiming() {
    let Some(fixture) = StoreFixture::postgres() else {
        return;
    };
    explicit_preflight_overlap_coordinates_without_claiming(&fixture);
}

#[test]
fn sqlite_empty_window_still_detects_exact_retained_source_head() {
    empty_window_still_detects_exact_retained_source_head(&StoreFixture::sqlite());
}

#[test]
fn postgres_empty_window_still_detects_exact_retained_source_head() {
    let Some(fixture) = StoreFixture::postgres() else {
        return;
    };
    empty_window_still_detects_exact_retained_source_head(&fixture);
}

#[test]
fn checked_merge_identity_rejects_hidden_characters_before_lookup() {
    use agentscm::hub::work_preflight::CheckedMerge;

    let fixture = StoreFixture::sqlite();
    let now = agentscm::hub::store::unix_now();
    let token = direct_token(&*fixture.store, 91, "identity-reader");
    let mut evidence = checked_window(&"a".repeat(40), true);
    evidence.checked_merges.push(CheckedMerge {
        source: CatchupMergeSource::ForgePull,
        source_id: "pull:7".into(),
        pull_number: 7,
        merge_commit: "b".repeat(40),
        in_source: true,
    });
    let mut request = direct_start(
        "checked-identity",
        "Inspect checked merge",
        "identity-session",
        &["src/identity.rs"],
        Some(evidence),
    );
    let ordinary = fixture.store.work_preflight(&token, &request, now).unwrap();
    assert!(
        ordinary
            .reasons
            .contains(&WorkPreflightReason::CheckedMergeUnknown)
    );

    for source_id in ["pull:\u{200d}7", "pull:\u{2066}7", "pull:\n7"] {
        request.preflight.as_mut().unwrap().checked_merges[0].source_id = source_id.into();
        assert!(matches!(
            fixture.store.work_preflight(&token, &request, now),
            Err(HubError::Invalid(_))
        ));
    }
    assert!(
        fixture
            .store
            .work_search(&token, None, None, 50, now)
            .unwrap()
            .items
            .is_empty()
    );
}

fn included_merge_does_not_require_complete_paths(fixture: &StoreFixture) {
    use agentscm::hub::work_preflight::CheckedMerge;

    let token = direct_token(&*fixture.store, 804, "large-merge-reader");
    let now = store::unix_now();
    observe_complete_sources(&*fixture.store, now);
    let merge = NewCatchupMerge {
        source: CatchupMergeSource::ForgePull,
        source_id: "pull:32".into(),
        occurred_at: Some(now - 1),
        title: Some("Large source move".into()),
        branch: "feature/large-move".into(),
        base: "main".into(),
        head_commit: "d".repeat(40),
        merge_commit: "e".repeat(40),
        pull_number: 32,
        paths: vec!["src/visible.rs".into()],
        paths_complete: false,
    };
    fixture
        .store
        .record_catchup_merge(&repository_scope(), &merge, now)
        .unwrap();
    let mut request = direct_start(
        "large-merge",
        "Continue after a large merge",
        "large-merge-session",
        &["src/omitted.rs"],
        Some(WorkPreflightRequest {
            source_revision: "f".repeat(40),
            merge_after: 0,
            merge_through: 1,
            source_relation_checked: true,
            checked_merges: vec![CheckedMerge {
                source: merge.source,
                source_id: merge.source_id.clone(),
                pull_number: merge.pull_number,
                merge_commit: merge.merge_commit.clone(),
                in_source: true,
            }],
        }),
    );
    let result = fixture.store.work_preflight(&token, &request, now).unwrap();
    assert_eq!(result.status, WorkPreflightStatus::ClearWithinCheckedWindow);
    assert!(!result.merges[0].paths_complete);
    // Neither an unchecked relation nor a missing merge can excuse incomplete paths.
    for (relation_checked, in_source) in [(false, true), (true, false)] {
        let evidence = request.preflight.as_mut().unwrap();
        evidence.source_relation_checked = relation_checked;
        evidence.checked_merges[0].in_source = in_source;
        let result = fixture.store.work_preflight(&token, &request, now).unwrap();
        assert!(
            result
                .reasons
                .contains(&WorkPreflightReason::MergePathsIncomplete)
        );
        assert!(matches!(
            fixture.store.work_start(&token, &request, now),
            Err(HubError::WorkPreflight(_))
        ));
    }
    let evidence = request.preflight.as_mut().unwrap();
    evidence.source_revision = merge.merge_commit.clone();
    evidence.source_relation_checked = false;
    let result = fixture.store.work_preflight(&token, &request, now).unwrap();
    assert_eq!(result.status, WorkPreflightStatus::ClearWithinCheckedWindow);
    // Exact merged heads must still be flagged, even with a claimed inclusion.
    let evidence = request.preflight.as_mut().unwrap();
    evidence.source_revision = merge.head_commit.clone();
    evidence.source_relation_checked = true;
    evidence.checked_merges[0].in_source = true;
    let result = fixture.store.work_preflight(&token, &request, now).unwrap();
    assert!(
        result
            .reasons
            .contains(&WorkPreflightReason::SourceHeadAlreadyMerged)
    );
    request.preflight.as_mut().unwrap().source_revision = "f".repeat(40);
    let started = fixture.store.work_start(&token, &request, now).unwrap();
    assert!(started.created);
}

#[test]
fn sqlite_included_merge_does_not_require_complete_paths() {
    included_merge_does_not_require_complete_paths(&StoreFixture::sqlite());
}

#[test]
fn postgres_included_merge_does_not_require_complete_paths() {
    let Some(fixture) = StoreFixture::postgres() else {
        return;
    };
    included_merge_does_not_require_complete_paths(&fixture);
}

/// The checkout's MCP server lists `agent_tools()` before it is connected; a connected agent must
/// see the same definitions, less the skill tools a repository that adopted no skills does not list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_unconnected_tool_list_matches_what_the_hub_serves_an_agent() {
    let hub = Hub::start().await;
    let writer = hub.token(false).await;
    let served = hub.mcp(&writer, 1, "tools/list", json!({})).await["tools"].clone();
    let compiled = agentscm::hub::mcp::agent_tools();
    let (served, compiled) = (served.as_array().unwrap(), compiled.as_array().unwrap());
    for tool in served {
        assert!(compiled.contains(tool), "compiled list drifted: {tool}");
    }
    for tool in compiled.iter().filter(|tool| !served.contains(tool)) {
        let name = tool["name"].as_str().unwrap();
        assert!(name.starts_with("agentscm_skill_"), "{name} is not served");
    }
    assert!(
        compiled
            .iter()
            .any(|tool| tool["name"] == "agentscm_setup_status")
    );
}
