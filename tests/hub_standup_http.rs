//! Exercise the shared stand-up HTTP surface with independent synthetic identities.
#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token};
use agentscm::hub::{HubState, router};
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    _dir: TempDir,
}

impl Hub {
    async fn start(database: Option<String>) -> Self {
        let dir = TempDir::new().unwrap();
        let database = database
            .unwrap_or_else(|| format!("sqlite://{}", dir.path().join("hub.sqlite").display()));
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

    async fn token(&self, id: i64, repository: &str, read_only: bool) -> (String, String) {
        let store = self.store.clone();
        let repository = repository.to_owned();
        tokio::task::spawn_blocking(move || {
            let owner = store
                .upsert_user(
                    &ForgeIdentity {
                        provider: "github".into(),
                        host: "github.com".into(),
                        user_id: id,
                    },
                    &format!("contributor{id}"),
                )
                .unwrap();
            let (token, secret, _) = mint_bound_agent_token(
                &*store,
                &owner.id,
                "test-agent",
                &repository,
                101,
                3600,
                read_only,
            )
            .unwrap();
            (token, secret.expose().to_owned())
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
        (
            status,
            serde_json::from_str(&response.text().await.unwrap()).unwrap(),
        )
    }
}

fn start(key: &str) -> Value {
    json!({"request_key": key, "title": "Improve request validation", "session": "session-a",
        "scope": ["src/request.rs"], "branch": "feat/validation", "worktree": "validation-checkout",
        "next_action": "Add boundary tests"})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_roundtrip_and_authorization() {
    roundtrip(Hub::start(None).await).await;
}

async fn roundtrip(hub: Hub) {
    let (_, alice) = hub.token(1, "acme/widgets", false).await;
    let (_, reader) = hub.token(2, "acme/widgets", true).await;
    let (_, other_repo) = hub.token(3, "acme/private", false).await;
    let (token_id, revoked) = hub.token(4, "acme/widgets", false).await;
    let (status, created) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &alice,
            Some(start("validation-goal")),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    let id = created["item"]["id"].as_str().unwrap();
    assert_eq!(created["created"], true);
    let (status, replay) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &alice,
            Some(start("validation-goal")),
        )
        .await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["item"]["id"], id);
    assert_eq!(replay["created"], false);

    let work_path = format!("/v1/hub/standup/work/{id}");
    let (status, item) = hub.call(Method::GET, &work_path, &reader, None).await;
    assert_eq!(status, 200);
    assert_eq!(item["id"], id);
    assert_eq!(item["state"], "active");
    let (status, _) = hub.call(Method::GET, &work_path, &other_repo, None).await;
    assert_eq!(status, 404);
    let (status, briefing) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/briefing?limit=1",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 200, "{briefing}");
    assert_eq!(briefing["items"][0]["id"], id);
    assert!(briefing["delivery"].is_null());

    for route in ["start", "checkpoint", "ack"] {
        let (status, body) = hub
            .call(
                Method::POST,
                &format!("/v1/hub/standup/{route}"),
                &reader,
                Some(json!({})),
            )
            .await;
        assert_eq!(status, 403, "{route}: {body}");
    }
    let mut invalid = start("another-goal");
    invalid["extra"] = json!("not accepted");
    assert_eq!(
        hub.call(Method::POST, "/v1/hub/standup/start", &alice, Some(invalid))
            .await
            .0,
        400
    );
    assert_eq!(
        hub.call(
            Method::GET,
            "/v1/hub/standup/briefing?limit=101",
            &reader,
            None
        )
        .await
        .0,
        400
    );
    assert_eq!(
        hub.call(
            Method::GET,
            "/v1/hub/standup/briefing?repository=acme/private",
            &reader,
            None
        )
        .await
        .0,
        400
    );

    let store = hub.store.clone();
    tokio::task::spawn_blocking(move || store.revoke_agent_token(&token_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        hub.call(Method::GET, &work_path, &revoked, None).await.0,
        401
    );
    assert_eq!(
        hub.call(
            Method::POST,
            "/v1/hub/standup/start",
            &revoked,
            Some(start("revoked"))
        )
        .await
        .0,
        401
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paged_delivery_is_retry_safe_and_acknowledges_only_its_watermark() {
    paged_delivery(Hub::start(None).await).await;
}

async fn paged_delivery(hub: Hub) {
    let (_, alice) = hub.token(11, "acme/widgets", false).await;
    let (_, created) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &alice,
            Some(start("paged-goal")),
        )
        .await;
    let id = created["item"]["id"].as_str().unwrap().to_owned();
    let mut expected_updated_seq = created["item"]["updated_seq"].as_i64().unwrap();
    let checkpoint = |index: usize, expected_updated_seq: i64| {
        json!({
            "work_id": id, "generation": 1, "session": "session-a", "operation_id": format!("progress-{index}"),
            "expected_updated_seq": expected_updated_seq,
            "kind": "progress", "summary": format!("Completed boundary case {index}"),
            "next_action": "Continue request validation"
        })
    };
    for index in 0..125 {
        let (status, body) = hub
            .call(
                Method::POST,
                "/v1/hub/standup/checkpoint",
                &alice,
                Some(checkpoint(index, expected_updated_seq)),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["item"]["current"]["kind"], "progress");
        assert_eq!(body["receipt"]["operation_id"], format!("progress-{index}"));
        assert_eq!(body["receipt"]["generation"], 1);
        assert_eq!(body["receipt"]["replayed"], false);
        assert!(body["receipt"]["event_seq"].as_i64().unwrap() > 0);
        expected_updated_seq = body["item"]["updated_seq"].as_i64().unwrap();
    }
    let first_path = "/v1/hub/standup/briefing?limit=17";
    let (status, mut page) = hub.call(Method::GET, first_path, &alice, None).await;
    assert_eq!(status, 200, "{page}");
    let through = page["through"].as_i64().unwrap();
    let delivery = page["delivery"].as_str().unwrap().to_owned();
    assert_eq!(page["complete"], false);
    let (status, _) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/ack",
            &alice,
            Some(json!({"delivery": delivery})),
        )
        .await;
    assert_eq!(status, 400);
    // New changes must be delivered in the next briefing, not inserted into this one.
    assert_eq!(
        hub.call(
            Method::POST,
            "/v1/hub/standup/checkpoint",
            &alice,
            Some(checkpoint(999, expected_updated_seq))
        )
        .await
        .0,
        200
    );
    let mut seen = Vec::new();
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages < 30, "pagination did not terminate");
        seen.extend(
            page["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| event["seq"].as_i64().unwrap()),
        );
        assert_eq!(page["retention_gap"], false);
        if page["complete"] == true {
            break;
        }
        let mut url =
            reqwest::Url::parse(&format!("{}/v1/hub/standup/briefing", hub.base)).unwrap();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("delivery", &delivery);
            query.append_pair("limit", "17");
            query.append_pair("through", &through.to_string());
            query.append_pair("after", &page["next_after"].to_string());
            query.append_pair("work_done", &page["next_work_done"].to_string());
            if let Some(work_after) = page["next_work_after"].as_str() {
                query.append_pair("work_after", work_after);
            }
        }
        let path = url.as_str().strip_prefix(&hub.base).unwrap();
        let (status, next) = hub.call(Method::GET, path, &alice, None).await;
        assert_eq!(status, 200, "{next}");
        let (status, replay) = hub.call(Method::GET, path, &alice, None).await;
        assert_eq!(status, 200, "{replay}");
        assert_eq!(replay, next, "retry skipped or changed a delivery page");
        page = next;
    }
    assert!(seen.len() > 100);
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(seen.iter().all(|seq| *seq <= through));
    assert_eq!(
        hub.call(
            Method::POST,
            "/v1/hub/standup/ack",
            &alice,
            Some(json!({"delivery": delivery}))
        )
        .await
        .0,
        200
    );
    let (status, next) = hub
        .call(Method::GET, "/v1/hub/standup/briefing", &alice, None)
        .await;
    assert_eq!(status, 200, "{next}");
    assert_eq!(next["from"], through);
    assert_eq!(
        next["items"][0]["id"], id,
        "old unfinished work disappeared after acknowledgement"
    );
    assert_eq!(next["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        next["events"][0]["item"]["current"]["summary"],
        "Completed boundary case 999"
    );
}

#[test]
fn postgres_work_roundtrip_and_authorization() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async { roundtrip(Hub::start(Some(pg.url.clone())).await).await });
}

#[test]
fn postgres_paged_delivery_is_retry_safe() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async { paged_delivery(Hub::start(Some(pg.url.clone())).await).await });
}
