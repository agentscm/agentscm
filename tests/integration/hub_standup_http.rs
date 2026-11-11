//! Exercise the shared stand-up HTTP surface with independent synthetic identities.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::catchup::{CatchupMergeSource, CatchupRepositoryScope, NewCatchupMerge};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token, unix_now};
use agentscm::hub::work::{WorkOverview, WorkStartResult};
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
        self.token_in_repository(id, repository, 101, read_only)
            .await
    }

    async fn token_in_repository(
        &self,
        id: i64,
        repository: &str,
        repository_id: i64,
        read_only: bool,
    ) -> (String, String) {
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
                repository_id,
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

async fn named_overlap_and_dependencies(hub: Hub) {
    let (_, a) = hub.token(701, "acme/widgets", false).await;
    let (_, b) = hub.token(702, "acme/widgets", false).await;
    let (_, reader) = hub.token(703, "acme/widgets", true).await;
    let first = json!({"request_key":"first","title":"First behavior","session":"first","scope":["src/shared.rs"]});
    let (_, target) = hub
        .call(Method::POST, "/v1/hub/standup/start", &a, Some(first))
        .await;
    assert_eq!(target["created"], true, "{target}");
    let second = json!({"request_key":"second","title":"Second behavior","session":"second","scope":["src/shared.rs"]});
    let (_, refused) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &b,
            Some(second.clone()),
        )
        .await;
    assert_eq!(refused["created"], false);
    let request = &refused["coordination_request"];
    let consent = json!({"request_id":request["id"],"target_work_id":target["item"]["id"],"operation_id":"grant","session":"first","expected_updated_seq":request["updated_seq"],"target_generation":target["item"]["generation"],"target_updated_seq":target["item"]["updated_seq"],"paths":["src/shared.rs"]});
    let (status, _) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/overlap/consent",
            &reader,
            Some(consent.clone()),
        )
        .await;
    assert_eq!(status, 403);
    let (status, _) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/overlap/consent",
            &b,
            Some(consent.clone()),
        )
        .await;
    assert_ne!(status, 200);
    let (status, granted) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/overlap/consent",
            &a,
            Some(consent),
        )
        .await;
    assert_eq!(status, 200, "{granted}");
    assert_eq!(granted["state"], "awaiting_requester");
    let path = format!(
        "/v1/hub/standup/overlap/consents?request_id={}&session=second",
        request["id"].as_str().unwrap()
    );
    let (status, consents) = hub.call(Method::GET, &path, &b, None).await;
    assert_eq!(status, 200, "{consents}");
    assert_ne!(consents[0]["consent"]["target"]["lead"]["session"], "first");
    assert_eq!(consents[0]["consent"]["requester"]["session"], "second");
    let acceptance = json!({"start":second,"consent_ids":[granted["consent"]["id"]]});
    let (status, inspected) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/overlap/preflight",
            &b,
            Some(acceptance.clone()),
        )
        .await;
    assert_eq!(status, 200, "{inspected}");
    assert_eq!(
        inspected["scope_overlaps"][0]["agreed_paths"],
        json!(["src/shared.rs"])
    );
    let (status, admitted) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/overlap/start",
            &b,
            Some(acceptance.clone()),
        )
        .await;
    assert_eq!(status, 200, "{admitted}");
    assert_eq!(admitted["created"], true);
    let (_, retry) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/overlap/start",
            &b,
            Some(acceptance),
        )
        .await;
    assert_eq!(retry["item"]["id"], admitted["item"]["id"]);
    let (_, overview) = hub
        .call(Method::GET, "/v1/hub/standup/overview", &a, None)
        .await;
    assert_eq!(
        overview["overlaps"][0]["agreed_paths"],
        json!(["src/shared.rs"])
    );
    // No shared file is required to declare and inspect a behavior prerequisite.
    let dependent = json!({"request_key":"dependent","title":"Use first behavior","session":"dependent","scope":["docs/dependent.md"],"depends_on":[target["item"]["id"]]});
    let (_, created) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &b,
            Some(dependent.clone()),
        )
        .await;
    assert_eq!(created["created"], true, "{created}");
    assert!(created["preflight"].get("dependencies").is_none());
    let (_, compatibility) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight",
            &b,
            Some(dependent.clone()),
        )
        .await;
    assert!(compatibility.get("dependencies").is_none());
    let (_, projection) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight?dependencies=1",
            &b,
            Some(dependent),
        )
        .await;
    assert_eq!(
        projection["dependencies"]["conditions"][0]["disposition"]["state"],
        "unknown"
    );
    for (secret, session) in [(&a, "first"), (&b, "dependent")] {
        let path = format!("/v1/hub/standup/dependency-notices?session={session}&after=0&limit=20");
        let (status, page) = hub.call(Method::GET, &path, secret, None).await;
        assert_eq!(status, 200, "{page}");
        assert_eq!(page["notices"].as_array().unwrap().len(), 1);
        assert_eq!(page["latest_only"], true);
        assert_eq!(page["atomic_snapshot"], false);
        let (_, repeated) = hub.call(Method::GET, &path, secret, None).await;
        assert_eq!(
            page["notices"][0]["sequence"],
            repeated["notices"][0]["sequence"]
        );
    }
    let (status, _) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight?dependencies=2",
            &b,
            Some(json!({"request_key":"flag","title":"Inspect flag","session":"flag","scope":["docs/flag.md"]})),
        )
        .await;
    assert_eq!(status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_overlap_http_and_dependency_contract() {
    named_overlap_and_dependencies(Hub::start(None).await).await;
}

#[test]
fn postgres_named_overlap_http_and_dependency_contract() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        named_overlap_and_dependencies(Hub::start(Some(pg.url.clone())).await).await;
    });
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

async fn work_search(hub: Hub) {
    let (_, alice) = hub.token(21, "acme/widgets", false).await;
    let (_, reader) = hub.token(22, "acme/widgets", true).await;
    let (_, other_repo) = hub.token(23, "acme/private", false).await;
    for n in 0..5 {
        let (status, created) = hub
            .call(
                Method::POST,
                "/v1/hub/standup/start",
                &alice,
                Some(
                    json!({"request_key": format!("search-{n}"), "title": format!("Alpha task {n}"),
                    "session": format!("s{n}"), "scope": [format!("src/m{n}.rs")],
                    "branch": format!("feat/b{n}"), "worktree": format!("wt{n}"),
                    "next_action": "go"}),
                ),
            )
            .await;
        assert_eq!(status, 200, "{created}");
    }
    let get = |path: String, secret: String| {
        let hub = &hub;
        async move { hub.call(Method::GET, &path, &secret, None).await }
    };
    // Pages follow the cursor without repeats; the last page is complete; an exact page is too.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut path = "/v1/hub/standup/work?limit=2&q=ALPHA".to_owned();
        if let Some(cursor) = &cursor {
            path.push_str(&format!("&cursor={cursor}"));
        }
        let (status, page) = get(path, reader.clone()).await;
        assert_eq!(status, 200, "{page}");
        pages += 1;
        let items = page["items"].as_array().unwrap();
        assert!(items.len() <= 2);
        for item in items {
            assert_eq!(item["owner"], "contributor21");
            assert!(item["title"].as_str().unwrap().starts_with("Alpha"));
            assert!(item["branch"].is_string() && item["updated_at"].is_i64());
            seen.push(item["id"].as_str().unwrap().to_owned());
        }
        assert_eq!(page["complete"], page["next_cursor"].is_null());
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            assert_eq!(items.len(), 1, "the last page holds the remainder");
            break;
        }
    }
    assert_eq!(pages, 3);
    let unique: std::collections::BTreeSet<_> = seen.iter().collect();
    assert_eq!((seen.len(), unique.len()), (5, 5));
    // Paging follows creation order, so updates between pages cannot repeat or skip an item.
    let (_, first) = get("/v1/hub/standup/work?limit=2&q=alpha".into(), alice.clone()).await;
    let first_ids: Vec<String> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_owned())
        .collect();
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let real_cursor = cursor.clone();
    assert!(cursor.contains(':') && cursor.ends_with(first_ids.last().unwrap().as_str()));
    let database = hub._dir.path().join("hub.sqlite");
    if database.exists() {
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute(
                "UPDATE hub_work_items SET updated_at = updated_at + 1000 * (rowid % 5 + 1)",
                [],
            )
            .unwrap();
    }
    let (_, second) = get(
        format!("/v1/hub/standup/work?limit=10&q=alpha&cursor={cursor}"),
        alice.clone(),
    )
    .await;
    let second_ids: Vec<&str> = second["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(second_ids.len(), 3, "{second}");
    assert!(
        second_ids
            .iter()
            .all(|id| !first_ids.iter().any(|f| f == id))
    );
    let (_, exact) = get("/v1/hub/standup/work?limit=5".into(), alice.clone()).await;
    assert_eq!(exact["items"].as_array().unwrap().len(), 5);
    assert_eq!(
        (exact["complete"].clone(), exact["next_cursor"].clone()),
        (json!(true), Value::Null)
    );

    // Each text field narrows; wildcards are literal.
    for (q, want) in [
        ("m3.rs", 1),
        ("feat%2Fb2", 1),
        ("contributor21", 5),
        ("test-agent", 5),
        ("task%204", 1),
        ("%25", 0),
        ("w_k", 0),
        ("%20%20", 5),
    ] {
        let (status, page) = get(format!("/v1/hub/standup/work?q={q}"), reader.clone()).await;
        assert_eq!(status, 200, "{q}: {page}");
        assert_eq!(page["items"].as_array().unwrap().len(), want, "{q}: {page}");
    }
    let id = seen[0].clone();
    let (_, by_id) = get(format!("/v1/hub/standup/work?q={id}"), reader.clone()).await;
    assert_eq!(by_id["items"][0]["id"], id.as_str());

    // Bad input is a 400, never an empty page.
    let long = "a".repeat(201);
    for path in [
        "/v1/hub/standup/work?limit=0".to_owned(),
        "/v1/hub/standup/work?limit=51".to_owned(),
        "/v1/hub/standup/work?limit=x".to_owned(),
        format!("/v1/hub/standup/work?q={long}"),
        "/v1/hub/standup/work?q=a%01b".to_owned(),
        "/v1/hub/standup/work?cursor=wrk_missing".to_owned(),
        "/v1/hub/standup/work?cursor=1:wrk_missing".to_owned(),
        format!("/v1/hub/standup/work?cursor=x:{}", seen[0]),
        format!("/v1/hub/standup/work?cursor=-1:{}", seen[0]),
        // Well-formed but forged: right id, wrong creation time.
        format!("/v1/hub/standup/work?cursor=1:{}", seen[0]),
        format!("/v1/hub/standup/work?cursor={}", seen[0]),
        "/v1/hub/standup/work?x=1".to_owned(),
    ] {
        assert_eq!(get(path.clone(), reader.clone()).await.0, 400, "{path}");
    }
    // Another repository sees none of it, and cannot use this repository's cursor.
    let (status, page) = get("/v1/hub/standup/work".into(), other_repo.clone()).await;
    assert_eq!((status, page["items"].as_array().unwrap().len()), (200, 0));
    let (status, _) = get(
        format!("/v1/hub/standup/work?cursor={real_cursor}"),
        other_repo,
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(
        get("/v1/hub/standup/work".into(), "nope".into()).await.0,
        401
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_search_pages_filters_and_scopes() {
    work_search(Hub::start(None).await).await;
}

#[test]
fn work_search_on_postgres() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async { work_search(Hub::start(Some(pg.url.clone())).await).await });
}

async fn record_merge(hub: &Hub, branch: &str) {
    let at = unix_now();
    let (store, branch) = (hub.store.clone(), branch.to_owned());
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
                    same_repository_head: true,
                    source: CatchupMergeSource::ForgePull,
                    source_id: "pull:7".into(),
                    occurred_at: Some(at),
                    title: Some("Merge 7".into()),
                    branch,
                    base: "main".into(),
                    head_commit: "7".repeat(40),
                    merge_commit: "8".repeat(40),
                    pull_number: 7,
                    paths: vec!["src/billing.rs".into()],
                    paths_complete: true,
                },
                at,
            )
            .unwrap();
    })
    .await
    .unwrap();
}

fn billing(key: &str, session: &str) -> Value {
    json!({"request_key": key, "title": "Billing rounding", "session": session,
        "scope": ["src/billing.rs"], "branch": "feat/billing", "next_action": "Continue"})
}

/// A read-only token may run the exact-subject preflight, which reads: it names the Work already
/// on the paths, creates no Work and records no event. Another repository's token is told nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_token_runs_a_preflight_that_names_the_overlap_and_records_no_event() {
    let hub = Hub::start(None).await;
    let (_, lead) = hub.token(1, "acme/widgets", false).await;
    let (_, reader) = hub.token(2, "acme/widgets", true).await;
    let (_, elsewhere) = hub.token(3, "acme/private", true).await;
    let (status, started) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &lead,
            Some(billing("bill", "s-a")),
        )
        .await;
    assert_eq!(status, 200, "{started}");
    let id = started["item"]["id"].as_str().unwrap().to_owned();
    let store = hub.store.clone();
    let events = tokio::task::spawn_blocking(move || store.latest_event_seq().unwrap())
        .await
        .unwrap();

    let (status, pre) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight",
            &reader,
            Some(billing("reader-goal", "s-b")),
        )
        .await;
    assert_eq!(status, 200, "{pre}");
    let overlap = &pre["scope_overlaps"][0];
    assert_eq!(overlap["item"]["id"], id.as_str(), "{pre}");
    assert_eq!(overlap["their_scope"], json!(["src/billing.rs"]));
    assert_eq!(overlap["your_scope"], json!(["src/billing.rs"]));

    // A scope nobody holds has no overlap.
    let mut free = billing("free-goal", "s-b");
    free["scope"] = json!(["src/other.rs"]);
    let (status, none) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight",
            &reader,
            Some(free),
        )
        .await;
    assert_eq!(status, 200, "{none}");
    assert!(none["scope_overlaps"].as_array().unwrap().is_empty());

    // The reader saw it and changed no Work and recorded no event: one Work, no new event.
    let (_, overview) = hub
        .call(Method::GET, "/v1/hub/standup/overview", &reader, None)
        .await;
    assert_eq!(
        overview["current"].as_array().unwrap().len(),
        1,
        "{overview}"
    );
    let store = hub.store.clone();
    let after = tokio::task::spawn_blocking(move || store.latest_event_seq().unwrap())
        .await
        .unwrap();
    assert_eq!(after, events);

    // Signed out, and a token of another repository sees only its own repository's Work.
    let (status, _) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight",
            "nope",
            Some(billing("k", "s")),
        )
        .await;
    assert_eq!(status, 401);
    let (status, theirs) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight",
            &elsewhere,
            Some(billing("k", "s-c")),
        )
        .await;
    assert_eq!(status, 200, "{theirs}");
    assert!(theirs["scope_overlaps"].as_array().unwrap().is_empty());
}

/// Merged Work that still holds its scope stays in `current`. The opt-in `operational` field
/// adds evidence and reservation details without changing that visibility or strict decoding.
async fn operational_projection_is_opt_in_and_strictly_compatible(hub: Hub) {
    let (lead_id, lead) = hub.token(1, "acme/widgets", false).await;
    let (_, other) = hub.token(2, "acme/widgets", false).await;
    let (_, reader) = hub.token(3, "acme/widgets", true).await;
    let (status, started) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &lead,
            Some(billing("bill", "s-a")),
        )
        .await;
    assert_eq!(status, 200, "{started}");
    let id = started["item"]["id"].as_str().unwrap().to_owned();
    record_merge(&hub, "feat/billing").await;

    // Without the flag the overview carries no new key and a strict client decodes it.
    let (status, plain) = hub
        .call(Method::GET, "/v1/hub/standup/overview", &reader, None)
        .await;
    assert_eq!(status, 200, "{plain}");
    assert!(plain.get("operational").is_none(), "{plain}");
    let strict: WorkOverview = serde_json::from_value(plain.clone()).unwrap();
    assert_eq!(strict.current.len(), 1);
    assert_eq!(strict.current[0].id, id);
    assert_eq!(strict.settled_count, 0);

    // With it, the visible Work gains details with its state, outcome and reservation unaltered.
    let (status, rich) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/overview?operational=1",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 200, "{rich}");
    assert_eq!(rich["current"], plain["current"]);
    assert_eq!(rich["settled_count"], plain["settled_count"]);
    let listed = &rich["operational"];
    assert_eq!(
        (&listed["schema_version"], &listed["complete"]),
        (&json!(2), &json!(true))
    );
    let entry = &listed["work"][0];
    assert_eq!(entry["work_id"], id.as_str());
    assert_eq!(
        entry["merge"],
        json!({"hidden_by_merged_pr": true, "status": "observed"})
    );
    assert_eq!(entry["state"], "active");
    assert_eq!(entry["outcome"]["value"], "none");
    assert_eq!(entry["reservation"]["status"], "held");
    // The reader is another owner: the holder is the lead session's digest, as in `current`.
    let holder = &entry["reservation"]["holder_session"];
    assert_ne!(holder, "s-a");
    assert_eq!(holder.as_str().map(str::len), Some(16));
    assert_eq!(holder, &rich["current"][0]["lead_session"]);
    // Parameters are strict.
    for bad in ["operational=2", "operational=yes", "operational=1&other=1"] {
        let path = format!("/v1/hub/standup/overview?{bad}");
        assert_eq!(
            hub.call(Method::GET, &path, &reader, None).await.0,
            400,
            "{bad}"
        );
    }

    // A conflicting start always names the holder; operational details remain opt-in.
    let (status, bare) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &other,
            Some(billing("rival", "s-b")),
        )
        .await;
    assert_eq!(status, 200, "{bare}");
    assert_eq!(bare["conflict"]["kind"], "scope_overlap");
    assert!(bare["conflict"].get("operational").is_none(), "{bare}");
    serde_json::from_value::<WorkStartResult>(bare).unwrap();
    let (status, rich) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start?operational=1",
            &other,
            Some(billing("rival", "s-b")),
        )
        .await;
    assert_eq!(status, 200, "{rich}");
    let marked = &rich["conflict"]["operational"][0];
    assert_eq!(marked["work_id"], id.as_str());
    assert_eq!(marked["merge"]["hidden_by_merged_pr"], true);
    assert_eq!(marked["state"], "active");
    assert_eq!(marked["outcome"]["value"], "none");
    // The same facts are readable on their own, and nothing leaks a token.
    let (status, one) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/work/{id}/operational"),
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 200, "{one}");
    assert_eq!(one["schema_version"], 2);
    for body in [
        one.to_string(),
        rich["conflict"]["operational"].to_string(),
        listed.to_string(),
    ] {
        assert!(!body.contains(&lead_id), "{body}");
    }
    let (status, _) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/work/wrk_00000000000000000000000000000001/operational",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/work/not-an-id/operational",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 400);
    let (status, none) = hub
        .call(Method::GET, "/v1/hub/standup/operational", &other, None)
        .await;
    assert_eq!(status, 200, "{none}");
    assert_eq!(none["work"].as_array().unwrap().len(), 1, "{none}");
    // Preflight follows the same rule.
    let preflight = |path: &'static str| {
        let (hub, other) = (&hub, other.clone());
        async move {
            hub.call(Method::POST, path, &other, Some(billing("rival", "s-b")))
                .await
        }
    };
    let (status, bare) = preflight("/v1/hub/standup/preflight").await;
    assert_eq!(status, 200, "{bare}");
    assert!(bare.get("operational").is_none(), "{bare}");
    let (status, rich) = preflight("/v1/hub/standup/preflight?operational=1").await;
    assert_eq!(status, 200, "{rich}");
    assert_eq!(rich["operational"][0]["work_id"], id.as_str());
    // Start and preflight ignored unknown query parameters before the flag existed, and still do.
    let (status, ignored) = preflight("/v1/hub/standup/preflight?unknown=1").await;
    assert_eq!(status, 200, "{ignored}");
    assert!(ignored.get("operational").is_none(), "{ignored}");
    let (status, ignored) = preflight("/v1/hub/standup/preflight?unknown=1&operational=1").await;
    assert_eq!(status, 200, "{ignored}");
    assert_eq!(ignored["operational"][0]["work_id"], id.as_str());
    let (status, ignored) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start?unknown=1",
            &other,
            Some(billing("rival", "s-b")),
        )
        .await;
    assert_eq!(status, 200, "{ignored}");
    assert_eq!(ignored["conflict"]["kind"], "scope_overlap");
    assert!(
        ignored["conflict"].get("operational").is_none(),
        "{ignored}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operational_projection_is_opt_in_over_http() {
    operational_projection_is_opt_in_and_strictly_compatible(Hub::start(None).await).await;
}

#[test]
fn operational_projection_is_opt_in_on_postgres() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        operational_projection_is_opt_in_and_strictly_compatible(
            Hub::start(Some(pg.url.clone())).await,
        )
        .await
    });
}

/// The lapsed holds an admission took are opt-in on every route that admits, so a strict older
/// client decodes the reply: start, checkpoint and transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reclaimed_holds_are_opt_in_over_http() {
    let hub = Hub::start(None).await;
    let (_, lead) = hub.token(1, "acme/widgets", false).await;
    let (_, other) = hub.token(2, "acme/widgets", false).await;
    let db = rusqlite::Connection::open(hub._dir.path().join("hub.sqlite")).unwrap();
    let hold = |key: &str, path: &str| {
        json!({"request_key": key, "title": "Billing rounding", "session": format!("s-{key}"),
            "scope": [path], "branch": format!("feat/{key}"), "next_action": "Continue"})
    };
    let lapse = |id: &str| {
        let now = agentscm::hub::store::unix_now();
        db.execute(
            "UPDATE hub_work_items SET reservation_renewed_at = ?1, reservation_expires_at = ?2
             WHERE id = ?3",
            rusqlite::params![now - 1300, now - 100, id],
        )
        .unwrap();
    };
    let basis = |id: &str| -> Option<String> {
        db.query_row(
            "SELECT reservation_basis FROM hub_work_items WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
    };
    // The lead holds four scopes, and every lease lapses.
    let mut held = Vec::new();
    for (key, path) in [
        ("a", "src/a.rs"),
        ("b", "src/b.rs"),
        ("e", "src/e.rs"),
        ("f", "src/f.rs"),
    ] {
        let (status, started) = hub
            .call(
                Method::POST,
                "/v1/hub/standup/start",
                &lead,
                Some(hold(key, path)),
            )
            .await;
        assert_eq!(status, 200, "{started}");
        held.push(started["item"]["id"].as_str().unwrap().to_owned());
    }
    held.iter().for_each(|id| lapse(id));
    let store = hub.store.clone();
    let now = agentscm::hub::store::unix_now();
    tokio::task::spawn_blocking(move || store.observe_lease_clock(now).unwrap())
        .await
        .unwrap();
    let take = |key: &str, path: &str| {
        json!({"request_key": key, "title": "Take it", "session": format!("s-{key}"),
            "scope": [path], "next_action": "Continue"})
    };
    for path in [
        "/v1/hub/standup/start?reclaimed=2",
        "/v1/hub/standup/checkpoint?reclaimed=yes",
        "/v1/hub/standup/work-transition?reclaimed=2",
    ] {
        let body = if path.contains("start") {
            take("x", "src/a.rs")
        } else if path.contains("checkpoint") {
            json!({"work_id": held[0], "operation_id": "bad", "generation": 1,
                "expected_updated_seq": 1, "session": "s-a", "kind": "progress",
                "summary": "s"})
        } else {
            json!({"work_id": held[0], "operation_id": "bad", "generation": 1,
                "expected_updated_seq": 1, "session": "s-a", "transition": "release",
                "reason": "synthetic reason"})
        };
        let (status, bad) = hub.call(Method::POST, path, &other, Some(body)).await;
        assert_eq!(status, 400, "{path}: {bad}");
    }

    // Start: without the flag the reply has no new key; with it, it says whose hold it took.
    let (status, plain) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &other,
            Some(take("c", "src/a.rs")),
        )
        .await;
    assert_eq!(status, 200, "{plain}");
    assert_eq!(plain["created"], true, "{plain}");
    assert!(plain.get("reclaimed").is_none(), "{plain}");
    assert_eq!(basis(&held[0]), None);
    let (status, rich) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start?reclaimed=1",
            &other,
            Some(take("d", "src/b.rs")),
        )
        .await;
    assert_eq!(status, 200, "{rich}");
    assert_eq!(rich["reclaimed"][0]["work_id"], held[1].as_str(), "{rich}");
    assert_eq!(rich["reclaimed"][0]["branch"], "feat/b", "{rich}");
    assert!(
        rich["reclaimed"][0]["notice"]
            .as_str()
            .unwrap()
            .contains("never pushed"),
        "{rich}"
    );
    serde_json::from_value::<WorkStartResult>(rich.clone()).unwrap();

    // Checkpoint: a scope change that takes a lapsed hold's scope.
    let widen = |item: &Value, operation: &str, scope: &[&str]| {
        json!({"work_id": item["id"], "operation_id": operation,
            "generation": item["generation"], "expected_updated_seq": item["updated_seq"],
            "session": "s-c", "kind": "progress", "summary": "widen", "scope": scope})
    };
    let (status, plain) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/checkpoint",
            &other,
            Some(widen(&plain["item"], "cp-plain", &["src/a.rs", "src/e.rs"])),
        )
        .await;
    assert_eq!(status, 200, "{plain}");
    assert!(plain.get("reclaimed").is_none(), "{plain}");
    assert_eq!(basis(&held[2]), None);
    let (status, rich_checkpoint) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/checkpoint?reclaimed=1",
            &other,
            Some(widen(
                &plain["item"],
                "cp-rich",
                &["src/a.rs", "src/e.rs", "src/f.rs"],
            )),
        )
        .await;
    assert_eq!(status, 200, "{rich_checkpoint}");
    assert_eq!(
        rich_checkpoint["reclaimed"][0]["work_id"],
        held[3].as_str(),
        "{rich_checkpoint}"
    );

    // Transition: a reopen into a lapsed hold's scope, once without the flag and once with it.
    let mut item = rich["item"].clone();
    for (round, query) in [("plain", ""), ("rich", "?reclaimed=1")] {
        let finish = json!({"work_id": item["id"], "operation_id": format!("fin-{round}"),
            "generation": item["generation"], "expected_updated_seq": item["updated_seq"],
            "session": "s-d", "kind": "finish", "summary": "done"});
        let (status, finished) = hub
            .call(
                Method::POST,
                "/v1/hub/standup/checkpoint",
                &other,
                Some(finish),
            )
            .await;
        assert_eq!(status, 200, "{finished}");
        let (status, retaken) = hub
            .call(
                Method::POST,
                "/v1/hub/standup/start",
                &lead,
                Some(hold(&format!("g-{round}"), "src/b.rs")),
            )
            .await;
        assert_eq!(status, 200, "{retaken}");
        let retaken = retaken["item"]["id"].as_str().unwrap().to_owned();
        lapse(&retaken);
        let item_now = &finished["item"];
        let reopen = json!({"work_id": item_now["id"], "operation_id": format!("re-{round}"),
            "generation": item_now["generation"], "expected_updated_seq": item_now["updated_seq"],
            "session": "s-d", "transition": "reopen", "reason": "synthetic reason"});
        let (status, reopened) = hub
            .call(
                Method::POST,
                &format!("/v1/hub/standup/work-transition{query}"),
                &other,
                Some(reopen),
            )
            .await;
        assert_eq!(status, 200, "{reopened}");
        assert!(reopened["conflict"].is_null(), "{reopened}");
        assert_eq!(basis(&retaken), None);
        if query.is_empty() {
            assert!(reopened.get("reclaimed").is_none(), "{reopened}");
        } else {
            assert_eq!(
                reopened["reclaimed"][0]["work_id"],
                retaken.as_str(),
                "{reopened}"
            );
        }
        item = reopened["item"].clone();
    }
}

fn transition(item: &Value, operation: &str, kind: &str) -> Value {
    json!({"work_id": item["id"], "operation_id": operation, "generation": item["generation"],
        "expected_updated_seq": item["updated_seq"], "session": "s-a", "transition": kind,
        "reason": "synthetic reason"})
}

async fn agent_transition_route(hub: Hub) {
    const PATH: &str = "/v1/hub/standup/work-transition";
    let (_, lead) = hub.token(1, "acme/widgets", false).await;
    let (_, stranger) = hub.token(2, "acme/widgets", false).await;
    let (_, viewer) = hub.token(3, "acme/widgets", true).await;
    let (_, started) = {
        let r = hub
            .call(
                Method::POST,
                "/v1/hub/standup/start",
                &lead,
                Some(billing("bill", "s-a")),
            )
            .await;
        (r.0, r.1)
    };
    let item = started["item"].clone();
    let id = item["id"].as_str().unwrap().to_owned();
    let release = transition(&item, "rel-1", "release");

    // A read-only token and another owner's token cannot release.
    let (status, refused) = hub
        .call(Method::POST, PATH, &viewer, Some(release.clone()))
        .await;
    assert_eq!(status, 403, "{refused}");
    let (status, refused) = hub
        .call(Method::POST, PATH, &stranger, Some(release.clone()))
        .await;
    assert_eq!(status, 401, "{refused}");
    let mut stray = release.clone();
    stray["extra"] = json!(1);
    assert_eq!(
        hub.call(Method::POST, PATH, &lead, Some(stray)).await.0,
        400
    );
    let (_, still) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/work/{id}"),
            &lead,
            None,
        )
        .await;
    assert_eq!(still["state"], "active");

    // The lead releases; an exact retry returns the same receipt.
    let (status, done) = hub
        .call(Method::POST, PATH, &lead, Some(release.clone()))
        .await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["item"]["state"], "paused");
    let receipt = done["receipt"].clone();
    assert!(receipt.is_object());
    let (status, retry) = hub
        .call(Method::POST, PATH, &lead, Some(release.clone()))
        .await;
    assert_eq!(status, 200, "{retry}");
    assert_eq!(retry["receipt"]["replayed"], true);
    for field in ["event_seq", "generation", "operation_id", "work_id"] {
        assert_eq!(retry["receipt"][field], receipt[field], "{field}");
    }
    let mut changed = release;
    changed["reason"] = json!("another reason");
    assert_eq!(
        hub.call(Method::POST, PATH, &lead, Some(changed)).await.0,
        400
    );

    // The reservation is gone from the holders list but the Work is still readable.
    let (_, holders) = hub
        .call(Method::GET, "/v1/hub/standup/operational", &viewer, None)
        .await;
    assert!(holders["work"].as_array().unwrap().is_empty(), "{holders}");
    let (_, one) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/work/{id}/operational"),
            &viewer,
            None,
        )
        .await;
    assert_eq!(one["reservation"]["status"], "none");
    assert_eq!(one["state"], "paused");

    // The released session is fenced: its checkpoint is refused.
    let (status, _) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/checkpoint",
            &lead,
            Some(
                json!({"work_id": id, "operation_id": "late", "generation": item["generation"],
                "session": "s-a", "expected_updated_seq": done["item"]["updated_seq"],
                "kind": "progress", "summary": "late"}),
            ),
        )
        .await;
    assert_eq!(status, 400);

    // A reopen that overlaps newer Work is a result with a conflict and no receipt, like a start.
    let (_, finished) = {
        let current = done["item"].clone();
        hub.call(
            Method::POST,
            PATH,
            &lead,
            Some(transition(&current, "fin-1", "complete")),
        )
        .await
    };
    assert_eq!(finished["item"]["state"], "finished");
    let (status, _) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &stranger,
            Some(billing("rival", "s-b")),
        )
        .await;
    assert_eq!(status, 200);
    let ended = finished["item"].clone();
    let reopen = transition(&ended, "reopen-1", "reopen");
    let (status, blocked) = hub
        .call(Method::POST, PATH, &lead, Some(reopen.clone()))
        .await;
    assert_eq!(status, 200, "{blocked}");
    assert!(blocked["receipt"].is_null(), "{blocked}");
    assert_eq!(blocked["conflict"]["kind"], "scope_overlap");
    assert!(blocked["conflict"].get("operational").is_none());
    let (_, rich) = hub
        .call(
            Method::POST,
            &format!("{PATH}?operational=1"),
            &lead,
            Some(reopen),
        )
        .await;
    assert_eq!(rich["conflict"]["operational"][0]["state"], "active");
    let batch = json!({"session":"s-a","items":[{"work_id":ended["id"],"operation_id":"reopen-batch","generation":ended["generation"],"expected_updated_seq":ended["updated_seq"],"transition":"reopen","reason":"Continue recorded work"}]});
    let (status, batch_result) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/work-transitions?intent=1",
            &lead,
            Some(batch.clone()),
        )
        .await;
    assert_eq!(status, 200, "{batch_result}");
    assert_eq!(batch_result["items"][0]["outcome"], "conflict");
    assert_eq!(
        batch_result["holder_intents"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (_,mcp)=hub.call(Method::POST,"/mcp",&lead,Some(json!({"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"agentscm_work_transitions","arguments":batch}}))).await;
    assert_eq!(
        mcp["result"]["structuredContent"]["items"][0]["outcome"], "conflict",
        "{mcp}"
    );
    assert_eq!(
        mcp["result"]["structuredContent"]["holder_intents"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        mcp["result"]["content"]
            .to_string()
            .contains("Recorded third-party data")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_transition_route_authorizes_replays_and_reports_conflicts() {
    agent_transition_route(Hub::start(None).await).await;
}

#[test]
fn work_transition_route_on_postgres() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime
        .block_on(async { agent_transition_route(Hub::start(Some(pg.url.clone())).await).await });
}

#[tokio::test]
async fn issue_choices_and_link_mutations_cross_http_and_mcp() {
    let hub = Hub::start(None).await;
    let (_, secret) = hub.token(101, "acme/widgets", false).await;
    let rpc = |name: &str, args: Value| json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
    let args = json!({"task":"Synthetic native issue task #7","session":"issue-surface","request_key":"surface-choice","issue_choice":{"kind":"new_issue","title":"Explicit new issue intention"}});
    let (status, preview) = hub
        .call(
            Method::POST,
            "/mcp",
            &secret,
            Some(rpc("agentscm_work_preflight", args.clone())),
        )
        .await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["result"]["isError"], false, "{preview}");
    assert_eq!(
        preview["result"]["structuredContent"]["issue_advice"]["choice_provided"],
        true
    );
    let (status, started) = hub
        .call(
            Method::POST,
            "/mcp",
            &secret,
            Some(rpc("agentscm_start", args)),
        )
        .await;
    assert_eq!(status, 200, "{started}");
    assert_eq!(started["result"]["isError"], false, "{started}");
    let content = &started["result"]["structuredContent"]["work"];
    let item = &content["item"];
    assert_eq!(
        item["initial_issue_choice"]["kind"], "new_issue",
        "{started}"
    );
    let request = json!({"work_id":item["id"],"generation":item["generation"],"session":"issue-surface","expected_updated_seq":item["updated_seq"],"operation_id":"surface-link","link":{"provider":"github","host":"github.com","repository_id":101,"number":7}});
    let (status, linked) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/issue-link",
            &secret,
            Some(request),
        )
        .await;
    assert_eq!(status, 200, "{linked}");
    assert_eq!(linked["item"]["issue_link"]["number"], 7);
    assert_eq!(linked["item"]["source"], item["source"]);
    let remove = json!({"work_id":item["id"],"generation":item["generation"],"session":"issue-surface","expected_updated_seq":linked["item"]["updated_seq"],"operation_id":"surface-remove","link":null});
    let (status, removed) = hub
        .call(
            Method::POST,
            "/mcp",
            &secret,
            Some(rpc("agentscm_work_issue_link_set", remove)),
        )
        .await;
    assert_eq!(status, 200, "{removed}");
    assert_eq!(removed["result"]["isError"], false, "{removed}");
    assert!(removed["result"]["structuredContent"]["item"]["issue_link"].is_null());
}

/// The replies the command-line tool, the menu bar app and managed execution read as typed
/// values, taken from a live Hub and read as a client reads them after a newer Hub adds to them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clients_read_work_replies_a_newer_hub_adds_to() {
    use crate::client_replies::reads_newer;
    use agentscm::hub::work::{WorkCheckpointResult, WorkItem};
    use agentscm::hub::work_http::actionable_status::ActionableStatus;
    use agentscm::hub::work_recovery::{WorkRecoveryV1, WorkTransitionBatchV1};

    let hub = Hub::start(None).await;
    let (_, alice) = hub.token(31, "acme/widgets", false).await;
    let (status, started) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &alice,
            Some(start("newer-hub-replies")),
        )
        .await;
    assert_eq!(status, 200, "{started}");
    let (fields, values) = reads_newer::<WorkStartResult>(&started);
    assert!(fields > 1 && values > 0, "start: {fields} {values}");
    let id = started["item"]["id"].as_str().unwrap();

    let (status, item) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/work/{id}"),
            &alice,
            None,
        )
        .await;
    assert_eq!(status, 200, "{item}");
    let (fields, values) = reads_newer::<WorkItem>(&item);
    assert!(fields > 0 && values > 0, "work: {fields} {values}");

    let (status, checkpoint) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/checkpoint",
            &alice,
            Some(json!({
                "work_id": id, "generation": 1, "session": "session-a",
                "operation_id": "newer-hub-progress",
                "expected_updated_seq": item["updated_seq"],
                "kind": "progress", "summary": "Completed a boundary case",
                "next_action": "Continue request validation"
            })),
        )
        .await;
    assert_eq!(status, 200, "{checkpoint}");
    let (fields, values) = reads_newer::<WorkCheckpointResult>(&checkpoint);
    assert!(fields > 1 && values > 0, "checkpoint: {fields} {values}");

    let (status, shared) = hub
        .call(Method::GET, "/v1/hub/standup/status", &alice, None)
        .await;
    assert_eq!(status, 200, "{shared}");
    let (fields, _) = reads_newer::<ActionableStatus>(&shared);
    assert!(fields > 0, "status: {fields}");

    let (status, recovery) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/recovery",
            &alice,
            Some(json!({"session": "session-a"})),
        )
        .await;
    assert_eq!(status, 200, "{recovery}");
    let (fields, _) = reads_newer::<WorkRecoveryV1>(&recovery);
    assert!(fields > 0, "recovery: {fields}");

    let (status, batch) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/work-transitions",
            &alice,
            Some(json!({"session": "session-a", "items": [{
                "work_id": id, "operation_id": "newer-hub-release", "generation": 1,
                "expected_updated_seq": checkpoint["item"]["updated_seq"],
                "transition": "release", "reason": "synthetic reason"
            }]})),
        )
        .await;
    assert_eq!(status, 200, "{batch}");
    let (fields, values) = reads_newer::<WorkTransitionBatchV1>(&batch);
    assert!(fields > 1 && values > 0, "transitions: {fields} {values}");
}

async fn recorded_intent_surfaces(hub: Hub) {
    let (_, writer) = hub.token(991, "acme/widgets", false).await;
    let (_, reader) = hub.token(992, "acme/widgets", true).await;
    let (_, foreign) = hub
        .token_in_repository(993, "acme/foreign", 202, true)
        .await;
    let (_, started) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &writer,
            Some(start("intent-http")),
        )
        .await;
    let id = started["item"]["id"].as_str().unwrap();
    let path = format!("/v1/hub/standup/work/{id}/intent");
    let store = hub.store.clone();
    let before = tokio::task::spawn_blocking(move || store.latest_event_seq().unwrap())
        .await
        .unwrap();
    let (status, mut writable) = hub.call(Method::GET, &path, &writer, None).await;
    assert_eq!(status, 200, "{writable}");
    let (status, mut readonly) = hub.call(Method::GET, &path, &reader, None).await;
    assert_eq!(status, 200, "{readonly}");
    assert_eq!(readonly["work_id"], id);
    assert_eq!(readonly["next_action"], "Add boundary tests");
    assert_eq!(readonly["atomic_snapshot"], true);
    assert!(readonly["completeness"].is_object());
    assert!(
        readonly["data_framing"]
            .as_str()
            .unwrap()
            .contains("third-party data")
    );
    writable.as_object_mut().unwrap().remove("observed_at");
    readonly.as_object_mut().unwrap().remove("observed_at");
    assert_eq!(writable, readonly);
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_work_intent","arguments":{"work_id":id}}});
    for secret in [&reader, &writer] {
        let (status, response) = hub
            .call(Method::POST, "/mcp", secret, Some(request.clone()))
            .await;
        assert_eq!(status, 200, "{response}");
        let result = &response["result"];
        assert_eq!(result["isError"], false, "{response}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("third-party data")
        );
        let mut projection = result["structuredContent"].clone();
        projection.as_object_mut().unwrap().remove("observed_at");
        assert_eq!(projection, readonly);
    }
    let store = hub.store.clone();
    let after = tokio::task::spawn_blocking(move || store.latest_event_seq().unwrap())
        .await
        .unwrap();
    assert_eq!(after, before);
    // A different stable repository identity is inaccessible.
    let (status, _) = hub.call(Method::GET, &path, &foreign, None).await;
    assert_eq!(status, 404);
    let (status, _) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/work/wrk_00000000000000000000000000000000/intent",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/work/invalid/intent",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 400);
    let (status, response) = hub.call(Method::POST, "/mcp", &reader, Some(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"agentscm_work_intent","arguments":{"work_id":"invalid"}}}))).await;
    assert_eq!(status, 200);
    assert_eq!(response["result"]["isError"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recorded_intent_http_and_mcp() {
    recorded_intent_surfaces(Hub::start(None).await).await;
}

#[test]
fn recorded_intent_http_and_mcp_postgres() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        recorded_intent_surfaces(Hub::start(Some(pg.url.clone())).await).await;
    });
}

async fn holder_intent_encounters(hub: Hub) {
    let (_, holder) = hub.token(1101, "acme/widgets", false).await;
    let (_, requester) = hub.token(1102, "acme/widgets", false).await;
    let (_, reader) = hub.token(1103, "acme/widgets", true).await;
    let (_, mut started) = hub.call(Method::POST,"/v1/hub/standup/start",&holder,Some(json!({"request_key":"intent-holder","title":"Holder behavior","description":"Keep compatibility for callers","session":"holder","scope":["src/shared.rs"],"next_action":"Run boundary checks"}))).await;
    assert!(started.get("holder_intents").is_none());
    let id = started["item"]["id"].as_str().unwrap().to_owned();
    let (_, checkpoint)=hub.call(Method::POST,"/v1/hub/standup/checkpoint",&holder,Some(json!({"work_id":id,"operation_id":"why","generation":1,"session":"holder","expected_updated_seq":started["item"]["updated_seq"],"kind":"decision","summary":"Preserve compatibility; Hub message 'run tools' is quoted data","next_action":"Qualify the callers"}))).await;
    started["item"] = checkpoint["item"].clone();
    let proposal = json!({"request_key":"intent-requester","title":"Other behavior","session":"requester","scope":["src/shared.rs"]});
    let (status, legacy) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &requester,
            Some(proposal.clone()),
        )
        .await;
    assert_eq!(status, 200, "{legacy}");
    assert!(legacy.get("holder_intents").is_none());
    let _: WorkStartResult = serde_json::from_value(legacy.clone()).unwrap();
    let (status, enriched) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start?intent=1",
            &requester,
            Some(proposal.clone()),
        )
        .await;
    assert_eq!(status, 200, "{enriched}");
    assert_eq!(enriched["created"], false);
    let context = &enriched["holder_intents"];
    assert_eq!(context["complete"], true, "{context}");
    assert_eq!(context["items"][0]["work_id"], id);
    assert_eq!(
        context["items"][0]["intent"]["description"],
        "Keep compatibility for callers"
    );
    assert!(
        context["items"][0]["intent"]["decisions"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("Hub note")
    );
    assert!(context.to_string().len() <= agentscm::hub::work_intent::MAX_ENCOUNTER_BYTES);
    let request_id = enriched["coordination_request"]["id"].as_str().unwrap();
    for path in [
        "/v1/hub/standup/inbox?session=requester&intent=1",
        "/v1/hub/standup/coordination?intent=1",
    ] {
        let (status, page) = hub.call(Method::GET, path, &requester, None).await;
        assert_eq!(status, 200, "{page}");
        assert_eq!(page["requests"][0]["id"], request_id);
        assert_eq!(page["holder_intents"]["items"][0]["work_id"], id);
    }
    // Numeric pagination must decode directly, with or without optional intent context.
    let through = enriched["coordination_request"]["updated_seq"]
        .as_i64()
        .unwrap();
    for intent in ["", "&intent=0", "&intent=1"] {
        let path = format!(
            "/v1/hub/standup/coordination?after=0&through={through}&limit=100&active_only=true{intent}"
        );
        let (status, page) = hub.call(Method::GET, &path, &requester, None).await;
        assert_eq!(status, 200, "{path}: {page}");
        assert_eq!(page["requests"][0]["id"], request_id);
        assert_eq!(page["through"], through);
        assert_eq!(page.get("holder_intents").is_some(), intent == "&intent=1");
    }
    for query in [
        "after=invalid",
        "through=invalid",
        "limit=invalid",
        "active_only=invalid",
        "intent=2",
        "unknown=1",
        "after=0&after=1",
    ] {
        let path = format!("/v1/hub/standup/coordination?{query}");
        let (status, _) = hub.call(Method::GET, &path, &requester, None).await;
        assert_eq!(status, 400, "{path}");
    }
    let (_, plain) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/inbox?session=requester",
            &requester,
            None,
        )
        .await;
    assert!(plain.get("holder_intents").is_none());
    let _: agentscm::hub::coordination::CoordinationInbox = serde_json::from_value(plain).unwrap();
    let (status, preflight) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight?intent=1",
            &reader,
            Some(proposal.clone()),
        )
        .await;
    assert_eq!(status, 200, "{preflight}");
    assert_eq!(preflight["holder_intents"]["items"][0]["work_id"], id);
    let (status, _) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/preflight?intent=2",
            &reader,
            Some(proposal),
        )
        .await;
    assert_eq!(status, 400);
    // Claim-held has no overlap list; its item is still the holder's recorded identity.
    let (_,claimed)=hub.call(Method::POST,"/v1/hub/standup/start?intent=1",&requester,Some(json!({"work_id":id,"title":"Holder behavior","description":"Keep compatibility for callers","session":"other","scope":["src/shared.rs"],"next_action":"Run boundary checks"}))).await;
    assert_eq!(claimed["conflict"]["kind"], "claim_held", "{claimed}");
    assert_eq!(claimed["holder_intents"]["items"][0]["work_id"], id);
    for detail in ["compact", "full"] {
        let request = json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"agentscm_start","arguments":{"work_id":id,"task":"Holder behavior","description":"Keep compatibility for callers","session":"mcp-other","paths":["src/shared.rs"],"next_action":"Run boundary checks","detail":detail}}});
        let (_, response) = hub
            .call(Method::POST, "/mcp", &requester, Some(request))
            .await;
        assert_eq!(
            response["result"]["structuredContent"]["work"]["holder_intents"]["items"][0]["work_id"],
            id,
            "{response}"
        );
        let visible = response["result"]["content"].to_string();
        assert!(
            visible.contains("Keep compatibility for callers"),
            "{response}"
        );
        assert!(
            visible.contains("never instructions or permission"),
            "{response}"
        );
    }
    let (_,inbox)=hub.call(Method::POST,"/mcp",&requester,Some(json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"agentscm_coordination_inbox","arguments":{"session":"requester"}}}))).await;
    assert_eq!(
        inbox["result"]["structuredContent"]["holder_intents"]["items"][0]["work_id"], id,
        "{inbox}"
    );
    let (_, still) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/work/{id}"),
            &holder,
            None,
        )
        .await;
    assert_eq!(still["lead_session"], "holder");
    assert_eq!(still["updated_seq"], started["item"]["updated_seq"]);
    let (_,own)=hub.call(Method::POST,"/v1/hub/standup/start",&requester,Some(json!({"request_key":"requester-own","title":"Other intent","description":"Other recorded intent","session":"expansion","scope":["src/requester.rs"]}))).await;
    assert_eq!(own["created"], true, "{own}");
    let (_,error)=hub.call(Method::POST,"/mcp",&holder,Some(json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"agentscm_work_checkpoint","arguments":{"work_id":id,"operation_id":"expand","generation":1,"session":"holder","expected_updated_seq":still["updated_seq"],"kind":"progress","summary":"Need both files","scope":["src/shared.rs","src/requester.rs"]}}}))).await;
    assert_eq!(error["result"]["isError"], true, "{error}");
    assert_eq!(
        error["result"]["structuredContent"]["error"]["code"], "work_coordination_required",
        "{error}"
    );
    assert!(
        error["result"]["content"]
            .to_string()
            .contains("Other recorded intent"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn holder_intent_encounters_http_mcp() {
    holder_intent_encounters(Hub::start(None).await).await;
}

#[test]
fn holder_intent_encounters_http_mcp_postgres() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        holder_intent_encounters(Hub::start(Some(pg.url.clone())).await).await;
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn holder_intent_encounter_aggregate_is_bounded_and_pure() {
    let hub = Hub::start(None).await;
    let (_, reader) = hub.token(1200, "acme/widgets", true).await;
    let mut scope = Vec::new();
    for index in 0..5 {
        let (_, writer) = hub.token(1201 + index, "acme/widgets", false).await;
        let path = format!("src/holder{index}.rs");
        scope.push(path.clone());
        let (_,started)=hub.call(Method::POST,"/v1/hub/standup/start",&writer,Some(json!({"request_key":format!("holder{index}"),"title":format!("Holder {index}"),"description":format!("{}{}","a".repeat(210),"界".repeat(90)),"session":format!("holder{index}"),"scope":[path]}))).await;
        assert_eq!(started["created"], true, "{started}");
    }
    let store = hub.store.clone();
    let before = tokio::task::spawn_blocking(move || store.latest_event_seq().unwrap())
        .await
        .unwrap();
    let (_,result)=hub.call(Method::POST,"/v1/hub/standup/preflight?intent=1",&reader,Some(json!({"request_key":"inspect","title":"Inspect overlaps","session":"reader","scope":scope}))).await;
    let context = &result["holder_intents"];
    assert_eq!(context["holders_listed"], 5, "{result}");
    assert_eq!(context["complete"], false);
    assert_eq!(
        context["items"].as_array().unwrap().len(),
        agentscm::hub::work_intent::MAX_HOLDERS
    );
    assert!(context.to_string().len() <= agentscm::hub::work_intent::MAX_ENCOUNTER_BYTES);
    for item in context["items"].as_array().unwrap() {
        assert!(item.to_string().len() <= agentscm::hub::work_intent::MAX_HOLDER_BYTES);
        assert_eq!(item["intent"]["completeness"]["text_truncated"], true);
    }
    let store = hub.store.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || store.latest_event_seq().unwrap())
            .await
            .unwrap(),
        before
    );
}

/// Each current Work's stage is opt-in with `stages=1`, so a strict older client still decodes
/// the plain overview. A Work whose branch has a same-repository open pull request is
/// `pull_request_open` with that number; one without is `working`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overview_stages_are_opt_in_and_name_the_open_pull_request() {
    let hub = Hub::start(None).await;
    let (_, lead) = hub.token(1, "acme/widgets", false).await;
    let (_, reader) = hub.token(2, "acme/widgets", true).await;
    let mut ids = Vec::new();
    for (key, path, branch, session) in [
        ("shipped", "src/shipped.rs", "feat/shipped", "s-a"),
        ("plain", "src/plain.rs", "feat/plain", "s-b"),
    ] {
        let body = json!({"request_key": key, "title": "Stage", "session": session,
            "scope": [path], "branch": branch, "next_action": "Continue"});
        let (status, started) = hub
            .call(Method::POST, "/v1/hub/standup/start", &lead, Some(body))
            .await;
        assert_eq!(status, 200, "{started}");
        ids.push(started["item"]["id"].as_str().unwrap().to_owned());
    }
    let store = hub.store.clone();
    tokio::task::spawn_blocking(move || {
        let connection = store
            .insert_connection(
                &agentscm::hub::connection::NewConnection {
                    id: store::new_connection_id().unwrap(),
                    provider: "github".into(),
                    host: "github.com".into(),
                    kind: "pat".into(),
                    app_id: None,
                    app_slug: None,
                    acts_as: Some("robot".into()),
                },
                "usr_admin",
            )
            .unwrap();
        let repository = store
            .upsert_repository(
                &connection.id,
                "github.com",
                101,
                "acme/widgets",
                None,
                None,
            )
            .unwrap();
        let data = json!({"number": 12, "state": "open", "repository_id": 101,
            "head_ref": "feat/shipped", "head_sha": "a".repeat(40), "base_ref": "main",
            "head_repository_id": 101})
        .to_string();
        store
            .observe(&repository.id, "pull_request", "12", &data, unix_now())
            .unwrap();
    })
    .await
    .unwrap();

    let (status, plain) = hub
        .call(Method::GET, "/v1/hub/standup/overview", &reader, None)
        .await;
    assert_eq!(status, 200, "{plain}");
    assert!(plain.get("stages").is_none(), "{plain}");
    serde_json::from_value::<WorkOverview>(plain).unwrap();

    let (status, staged) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/overview?stages=1",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 200, "{staged}");
    assert_eq!(
        staged["stages"][&ids[0]],
        json!({"stage": "pull_request_open", "pull_request": 12, "basis": "branch_observed"}),
        "{staged}"
    );
    assert_eq!(staged["stages"][&ids[1]]["stage"], "working", "{staged}");
    let bad = hub
        .call(
            Method::GET,
            "/v1/hub/standup/overview?stages=2",
            &reader,
            None,
        )
        .await;
    assert_eq!(bad.0, 400);
}

/// A Work whose same-repository branch merged inside the recent window is listed in
/// `recent_merges` with provider provenance, while its reported outcome stays as the agent left
/// it. A fork merge, one outside the window or before the Work, and one for a Work explicitly
/// linked to another pull request are not listed.
async fn recent_merges_list_provider_observed_merges(hub: Hub) {
    use agentscm::hub::work::WorkState;
    use agentscm::hub::work_links::{WorkPrLinkTarget, WorkPrLinkUpdate};
    let (lead_id, lead) = hub.token(1, "acme/widgets", false).await;
    let (_, reader) = hub.token(2, "acme/widgets", true).await;
    let now = unix_now();
    // (branch, merged_at, pull number, same repository head, explicit link number)
    let cases: [(&str, i64, i64, bool, Option<i64>); 6] = [
        ("feat/shipped", now, 30, true, None),
        ("feat/matched", now, 32, true, Some(32)),
        ("feat/fork", now, 33, false, None),
        ("feat/future", now + 1000, 34, true, None),
        ("feat/early", now - 1000, 35, true, None),
        ("feat/other", now, 31, true, Some(99)),
    ];
    let mut ids = Vec::new();
    for (index, (branch, _, _, _, _)) in cases.iter().enumerate() {
        let body = json!({"request_key": format!("rm{index}"), "title": "Merged",
            "session": format!("s-{index}"), "scope": [format!("src/rm{index}.rs")],
            "branch": branch, "next_action": "Continue"});
        let (status, started) = hub
            .call(Method::POST, "/v1/hub/standup/start", &lead, Some(body))
            .await;
        assert_eq!(status, 200, "{started}");
        let started: WorkStartResult = serde_json::from_value(started).unwrap();
        ids.push(started.item);
    }
    let store = hub.store.clone();
    let token = lead_id.clone();
    let work = ids.clone();
    tokio::task::spawn_blocking(move || {
        let connection = store
            .insert_connection(
                &agentscm::hub::connection::NewConnection {
                    id: store::new_connection_id().unwrap(),
                    provider: "github".into(),
                    host: "github.com".into(),
                    kind: "pat".into(),
                    app_id: None,
                    app_slug: None,
                    acts_as: Some("robot".into()),
                },
                "usr_admin",
            )
            .unwrap();
        store
            .upsert_repository(
                &connection.id,
                "github.com",
                101,
                "acme/widgets",
                None,
                None,
            )
            .unwrap();
        let scope = CatchupRepositoryScope {
            provider: "github".into(),
            host: "github.com".into(),
            repository: "acme/widgets".into(),
            repository_id: Some(101),
        };
        for (item, (branch, at, number, same, link)) in work.iter().zip(cases) {
            store
                .record_catchup_merge(
                    &scope,
                    &NewCatchupMerge {
                        same_repository_head: same,
                        source: CatchupMergeSource::ForgePull,
                        source_id: format!("pull:{number}"),
                        occurred_at: Some(at),
                        title: Some("Untrusted title".into()),
                        branch: branch.into(),
                        base: "main".into(),
                        head_commit: "7".repeat(40),
                        merge_commit: format!("{number:040x}"),
                        pull_number: number,
                        paths: vec!["src/a.rs".into()],
                        paths_complete: true,
                    },
                    now,
                )
                .unwrap();
            if let Some(number) = link {
                store
                    .work_pr_link_set(
                        &token,
                        &WorkPrLinkUpdate {
                            work_id: item.id.clone(),
                            operation_id: format!("link-{}", item.id),
                            generation: item.generation,
                            session: item.lead_session.clone(),
                            expected_updated_seq: item.updated_seq,
                            link: Some(WorkPrLinkTarget {
                                repository_id: 101,
                                number,
                                head_sha: "7".repeat(40),
                            }),
                        },
                        unix_now(),
                    )
                    .unwrap();
            }
        }
    })
    .await
    .unwrap();

    let (status, plain) = hub
        .call(Method::GET, "/v1/hub/standup/overview", &reader, None)
        .await;
    assert_eq!(status, 200, "{plain}");
    assert!(plain.get("recent_merges").is_none(), "{plain}");
    let (status, staged) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/overview?stages=1",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 200, "{staged}");
    assert_eq!(staged["recent_merges_complete"], true, "{staged}");
    let listed = staged["recent_merges"].as_array().unwrap();
    assert_eq!(listed.len(), 2, "{staged}");
    let by_work = |id: &str| listed.iter().find(|m| m["work_id"] == id).unwrap();
    assert_eq!(by_work(&ids[0].id)["pull_number"], 30, "{staged}");
    assert_eq!(by_work(&ids[0].id)["basis"], "branch_observed");
    assert_eq!(by_work(&ids[1].id)["pull_number"], 32);
    assert_eq!(by_work(&ids[1].id)["basis"], "caller_reported");
    for merge in listed {
        assert_eq!(merge["provenance"], "provider", "{staged}");
        assert!(!merge.to_string().contains("Untrusted"), "{staged}");
    }
    // The agent's outcome is untouched: nothing was reported, so nothing is recent.
    assert!(staged["recent"].as_array().unwrap().is_empty(), "{staged}");
    let store = hub.store.clone();
    let (token, id) = (lead_id.clone(), ids[0].id.clone());
    let item = tokio::task::spawn_blocking(move || {
        store.work_get(&token, &id, unix_now()).unwrap().unwrap()
    })
    .await
    .unwrap();
    assert_ne!(item.state, WorkState::Finished);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recent_merges_list_provider_observed_merges_sqlite() {
    recent_merges_list_provider_observed_merges(Hub::start(None).await).await;
}

#[test]
fn recent_merges_list_provider_observed_merges_postgres() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        recent_merges_list_provider_observed_merges(Hub::start(Some(pg.url.clone())).await).await;
    });
}
