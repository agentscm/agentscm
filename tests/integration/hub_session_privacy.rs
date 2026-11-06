//! Another owner's agents, as a reader sees them over HTTP and MCP: sessions by their digest,
//! never their native id, and stored host-named labels masked. The owner's own reads keep both.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token};
use agentscm::hub::{HubState, router};
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const ALICE_SESSION: &str = "alice-native-session-0001";
const ALICE_LABEL: &str = "client-alices-macbook-1a2b3c4d";
const ALICE_HOST: &str = "alices-macbook";
const BOB_SESSION: &str = "bob-native-session-0002";
const BOB_OVERLAP_SESSION: &str = "bob-native-session-0003";
const BOB_HTTP_SESSION: &str = "bob-native-session-0004";
const BOB_LABEL: &str = "client-bobs-desktop-5e6f7a8b";
const BOB_HOST: &str = "bobs-desktop";

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

    async fn secret(&self, id: i64, label: &str) -> String {
        let (store, label) = (self.store.clone(), label.to_owned());
        tokio::task::spawn_blocking(move || {
            let owner = store
                .upsert_user(
                    &ForgeIdentity {
                        provider: "github".into(),
                        host: "github.com".into(),
                        user_id: id,
                    },
                    &format!("person{id}"),
                )
                .unwrap();
            let (_, secret, _) =
                mint_bound_agent_token(&*store, &owner.id, &label, REPO, 101, 3600, false).unwrap();
            secret.expose().to_owned()
        })
        .await
        .unwrap()
    }

    /// The reply's status and body text.
    async fn call(
        &self,
        method: Method,
        path: &str,
        secret: &str,
        body: Option<Value>,
    ) -> (u16, String) {
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
        (status, response.text().await.unwrap())
    }

    async fn get(&self, path: &str, secret: &str) -> String {
        let (status, text) = self.call(Method::GET, path, secret, None).await;
        assert_eq!(status, 200, "{path}: {text}");
        text
    }

    /// The whole JSON-RPC reply of one tool call.
    async fn tool(&self, secret: &str, name: &str, arguments: Value) -> String {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        let (status, text) = self.call(Method::POST, "/mcp", secret, Some(body)).await;
        assert_eq!(status, 200, "{name}: {text}");
        text
    }
}

fn hex16(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Neither Alice's session id nor her host name is anywhere in a reply Bob gets.
fn assert_hidden(what: &str, text: &str) {
    for secret in [ALICE_SESSION, ALICE_HOST] {
        assert!(!text.contains(secret), "{what} shows {secret}: {text}");
    }
}

async fn scenario(hub: Hub) {
    let alice = hub.secret(1, ALICE_LABEL).await;
    let bob = hub.secret(2, BOB_LABEL).await;
    let start = |key: &str, session: &str, path: &str| json!({"task": key, "request_key": key, "session": session, "paths": [path]});
    // Alice holds src/shared.rs; Bob holds src/bob.rs.
    let alice_start = hub
        .tool(
            &alice,
            "agentscm_start",
            start("alice-work", ALICE_SESSION, "src/shared.rs"),
        )
        .await;
    assert!(alice_start.contains(ALICE_SESSION), "{alice_start}");
    hub.tool(
        &bob,
        "agentscm_start",
        start("bob-work", BOB_SESSION, "src/bob.rs"),
    )
    .await;
    let work: Value =
        serde_json::from_str(&hub.get("/v1/hub/standup/overview", &alice).await).unwrap();
    let alice_id = work["current"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["lead_session"] == ALICE_SESSION)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Bob's second session asks for Alice's file: refused over MCP and HTTP, each reply naming
    // her Work, and a coordination request is queued to her.
    let refused = hub
        .tool(
            &bob,
            "agentscm_start",
            start("bob-overlap", BOB_OVERLAP_SESSION, "src/shared.rs"),
        )
        .await;
    assert!(refused.contains(&alice_id), "{refused}");
    assert_hidden("MCP refused start", &refused);
    let (status, refused) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start?operational=1",
            &bob,
            Some(json!({"request_key": "bob-overlap-http", "title": "Overlap", "session": BOB_HTTP_SESSION,
                "scope": ["src/shared.rs"]})),
        )
        .await;
    assert_eq!(status, 200, "{refused}");
    assert!(refused.contains(&alice_id), "{refused}");
    assert_hidden("HTTP refused start", &refused);

    // Every read Bob makes names Alice's agent by digest and masked label only.
    let encoded = REPO.replace('/', "%2F");
    for path in [
        "/v1/hub/standup/overview".to_owned(),
        "/v1/hub/standup/overview?operational=1".to_owned(),
        "/v1/hub/standup/operational".to_owned(),
        format!("/v1/hub/standup/work/{alice_id}"),
        format!("/v1/hub/standup/work/{alice_id}/operational"),
        format!("/v1/hub/standup/work/{alice_id}/reconciliation"),
        "/v1/hub/standup/briefing".to_owned(),
        "/v1/hub/standup/catchup".to_owned(),
        "/v1/hub/standup/coordination".to_owned(),
        format!("/v1/hub/standup/inbox?session={BOB_OVERLAP_SESSION}"),
        "/v1/hub/standup/status".to_owned(),
        "/v1/hub/standup/work".to_owned(),
        format!("/v1/hub/repos/{encoded}/intents"),
        format!("/v1/hub/repos/{encoded}/activity"),
        format!("/v1/hub/repos/{encoded}/work"),
    ] {
        assert_hidden(&path, &hub.get(&path, &bob).await);
    }
    for (name, arguments) in [
        ("agentscm_work_overview", json!({})),
        ("agentscm_work_briefing", json!({})),
        ("agentscm_work_catchup", json!({})),
        (
            "agentscm_coordination_inbox",
            json!({"session": BOB_OVERLAP_SESSION}),
        ),
        ("agentscm_work_status", json!({})),
        ("agentscm_work_search", json!({})),
    ] {
        assert_hidden(name, &hub.tool(&bob, name, arguments).await);
    }
    let inbox: Value = serde_json::from_str(
        &hub.get(
            &format!("/v1/hub/standup/inbox?session={ALICE_SESSION}"),
            &alice,
        )
        .await,
    )
    .unwrap();
    assert!(!inbox["requests"].as_array().unwrap().is_empty(), "{inbox}");

    // The host name cannot be found by search either, though Alice still finds her own.
    let found = |text: String| -> usize {
        serde_json::from_str::<Value>(&text).unwrap()["items"]
            .as_array()
            .unwrap()
            .len()
    };
    let path = format!("/v1/hub/standup/work?q={ALICE_HOST}");
    assert_eq!(found(hub.get(&path, &bob).await), 0);
    assert_eq!(found(hub.get(&path, &alice).await), 1);

    // The digest stands for the same session in every field that names it.
    let view: Value = serde_json::from_str(
        &hub.get("/v1/hub/standup/overview?operational=1", &bob)
            .await,
    )
    .unwrap();
    let item = view["current"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == alice_id.as_str())
        .unwrap();
    assert!(hex16(&item["lead_session"]), "{item}");
    assert_eq!(item["lead_agent"], "client-host-1a2b3c4d");
    let lead = &item["participants"][0];
    assert_eq!(
        (&lead["session"], &lead["agent"]),
        (&item["lead_session"], &item["lead_agent"])
    );
    let held = view["operational"]["work"]
        .as_array()
        .unwrap()
        .iter()
        .find(|work| work["work_id"] == alice_id.as_str())
        .unwrap();
    assert_eq!(held["reservation"]["holder_session"], item["lead_session"]);
    assert_eq!(held["reservation"]["holder_agent"], item["lead_agent"]);

    // Alice sees her own agent as stored, and Bob's as Bob's readers see Alice's.
    let own = hub
        .get("/v1/hub/standup/overview?operational=1", &alice)
        .await;
    assert!(
        own.contains(ALICE_SESSION) && own.contains(ALICE_LABEL),
        "{own}"
    );
    for secret in [BOB_SESSION, BOB_HOST] {
        assert!(!own.contains(secret), "Alice sees {secret}: {own}");
    }
    let own = hub.tool(&alice, "agentscm_work_overview", json!({})).await;
    assert!(
        own.contains(ALICE_SESSION) && own.contains(ALICE_LABEL),
        "{own}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_owners_see_session_digests_and_masked_labels() {
    scenario(Hub::start(None).await).await;
}

#[test]
fn postgres_other_owners_see_session_digests_and_masked_labels() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async { scenario(Hub::start(Some(pg.url.clone())).await).await });
}
