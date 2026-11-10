//! Sessions under the session that started them: one owner-wide name per native session, the
//! declared `started_by` link, each session's subagents, and the wait of a started session.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::session::SESSION_TTL_SECONDS;
use agentscm::hub::store::{
    self, ForgeIdentity, Store, agent_token_digest, mint_bound_agent_token, unix_now,
};
use agentscm::hub::{HubState, router};
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const WIDGETS: (&str, i64) = ("acme/widgets", 101);
const OTHER: (&str, i64) = ("acme/other", 102);
const PARENT: &str = "11111111-1111-4111-8111-111111111111";
const CHILD: &str = "22222222-2222-4222-8222-222222222222";
const BOBS: &str = "44444444-4444-4444-8444-444444444444";
const UNSEEN: &str = "99999999-9999-4999-8999-999999999999";

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

    /// An agent token of person `id` for `repository`.
    async fn secret(&self, id: i64, repository: (&'static str, i64)) -> String {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let identity = ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: id,
            };
            let owner = store
                .upsert_user(&identity, &format!("person{id}"))
                .unwrap();
            let (name, repository_id) = repository;
            let (_, secret, _) = mint_bound_agent_token(
                &*store,
                &owner.id,
                "laptop",
                name,
                repository_id,
                3600,
                false,
            )
            .unwrap();
            secret.expose().to_owned()
        })
        .await
        .unwrap()
    }

    async fn call(&self, method: Method, path: &str, secret: &str, body: Option<Value>) -> Value {
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
        let text = response.text().await.unwrap();
        assert_eq!(status, 200, "{path}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    /// One Claude Code hook event of `session`.
    async fn hook(&self, secret: &str, session: &str, event: &str, more: Value) {
        let mut body = json!({"hook_event_name": event, "session_id": session, "cwd": "/repo"});
        for (key, value) in more.as_object().unwrap() {
            body[key] = value.clone();
        }
        self.call(Method::POST, "/v1/hub/hook/claude", secret, Some(body))
            .await;
    }

    /// The text of one `agentscm_start` reply.
    async fn start_work(&self, secret: &str, session: &str, started_by: Option<&str>) -> String {
        // Named by a few characters of the ID: the whole ID must appear in no reply.
        let label = &session[..4];
        let mut arguments = json!({"task": format!("task {label}"),
            "request_key": format!("key-{label}"), "session": session,
            "paths": [format!("src/{label}.rs")]});
        if let Some(started_by) = started_by {
            arguments["started_by"] = json!(started_by);
        }
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "agentscm_start", "arguments": arguments}});
        self.call(Method::POST, "/mcp", secret, Some(body))
            .await
            .to_string()
    }

    /// The sessions of `repository`'s activity read, and the whole reply as text.
    async fn sessions(&self, secret: &str, repository: (&str, i64)) -> (Vec<Value>, String) {
        let path = format!(
            "/v1/hub/repos/{}/activity",
            repository.0.replace('/', "%2F")
        );
        let body = self.call(Method::GET, &path, secret, None).await;
        (
            body["sessions"].as_array().unwrap().clone(),
            body.to_string(),
        )
    }
}

fn hex16(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The session whose `started_by` is `by`.
fn started<'a>(sessions: &'a [Value], by: &Value) -> &'a Value {
    sessions
        .iter()
        .find(|session| session["started_by"] == *by)
        .unwrap_or_else(|| panic!("no session started by {by}: {sessions:?}"))
}

async fn scenario(hub: Hub) {
    let alice = hub.secret(1, WIDGETS).await;
    let alice_other = hub.secret(1, OTHER).await;
    let bob = hub.secret(2, WIDGETS).await;
    let begin = json!({"source": "startup"});
    // One of Alice's sessions works in both repositories; it was started by another of hers.
    hub.hook(&alice, PARENT, "SessionStart", begin.clone())
        .await;
    hub.hook(&alice, CHILD, "SessionStart", begin.clone()).await;
    hub.hook(&alice_other, CHILD, "SessionStart", begin.clone())
        .await;
    hub.hook(&bob, BOBS, "SessionStart", begin).await;

    // A link is taken only for a session of the same person that the Hub knows, and never a loop.
    // One that is not taken never refuses the start: the reply says why it was not recorded.
    for (by, why) in [
        (UNSEEN, "not a session of yours that this Hub knows"),
        (BOBS, "not a session of yours that this Hub knows"),
        ("default", "native session IDs of both sessions"),
        (CHILD, "a session that this session started"),
    ] {
        let unlinked = hub.start_work(&alice_other, CHILD, Some(by)).await;
        assert!(
            unlinked.contains("started_by was not recorded") && unlinked.contains(why),
            "{by}: {unlinked}"
        );
        assert!(unlinked.contains("wrk_"), "{by}: {unlinked}");
    }
    let (child_only, _) = hub.sessions(&alice_other, OTHER).await;
    assert_eq!(child_only[0]["started_by"], Value::Null);
    let accepted = hub.start_work(&alice_other, CHILD, Some(PARENT)).await;
    assert!(
        accepted.contains("wrk_") && !accepted.contains("was not recorded"),
        "{accepted}"
    );
    let looped = hub.start_work(&alice, PARENT, Some(CHILD)).await;
    assert!(
        looped.contains("a session that this session started"),
        "{looped}"
    );

    // Both repositories name the session alike and say who started it; neither shows an id.
    let (widgets, text) = hub.sessions(&alice, WIDGETS).await;
    for id in [PARENT, CHILD, BOBS] {
        assert!(!text.contains(id), "{text}");
    }
    let parent = widgets
        .iter()
        .find(|s| s["owner"]["login"] == "person1" && s["started_by"].is_null())
        .unwrap();
    assert!(hex16(&parent["name"]), "{parent}");
    let child = started(&widgets, &parent["name"]);
    assert!(hex16(&child["name"]) && child["name"] != parent["name"]);
    let (other, text) = hub.sessions(&alice_other, OTHER).await;
    assert!(!text.contains(CHILD), "{text}");
    assert_eq!(other.len(), 1);
    assert_eq!(
        (&other[0]["name"], &other[0]["started_by"]),
        (&child["name"], &parent["name"])
    );
    assert_ne!(other[0]["key"], child["key"]);
    // The hold the start took names its session the same way.
    let held = hub
        .call(
            Method::GET,
            "/v1/hub/repos/acme%2Fother/intents",
            &alice_other,
            None,
        )
        .await;
    assert_eq!(held["intents"][0]["session"]["name"], child["name"]);
    // Past its last call a session is no longer live, but one that started another is still read
    // for its own person, so what its sessions ask can be shown under it.
    assert!(text.contains("\"quiet_sessions\":[]"), "{text}");
    let (store, secret) = (hub.store.clone(), alice.clone());
    tokio::task::spawn_blocking(move || {
        let digest = agent_token_digest(&secret).unwrap();
        let me = store.authenticate(&digest, unix_now()).unwrap().unwrap();
        let later = unix_now() + SESSION_TTL_SECONDS + 60;
        assert!(store.live_sessions(&me, later, 10).unwrap().is_empty());
        let also = [PARENT.to_owned(), BOBS.to_owned()];
        let kept = store.sessions_with(&me, later, 10, &also).unwrap();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(
            (kept[0].session.as_str(), &kept[0].started_by),
            (PARENT, &None)
        );
        assert_eq!(
            store.session_starts(&me.owner.id).unwrap(),
            [(CHILD.to_owned(), PARENT.to_owned())]
        );
        // Each leads the Work its start made, the child's in the other repository.
        assert_eq!(
            store.owner_lead_sessions(&me.owner.id).unwrap(),
            [PARENT, CHILD]
        );
        // Records are read newest first, whatever the session IDs.
        store
            .session_started_by(&me.owner.id, UNSEEN, PARENT, later)
            .unwrap();
        assert_eq!(
            store.session_starts(&me.owner.id).unwrap(),
            [
                (UNSEEN.to_owned(), PARENT.to_owned()),
                (CHILD.to_owned(), PARENT.to_owned())
            ]
        );
    })
    .await
    .unwrap();
    // Another person sees who started whom, and Bob's own session belongs to no one.
    let (seen, _) = hub.sessions(&bob, WIDGETS).await;
    assert_eq!(started(&seen, &parent["name"])["name"], child["name"]);
    let bobs = seen
        .iter()
        .find(|s| s["owner"]["login"] == "person2")
        .unwrap();
    assert!(hex16(&bobs["name"]) && bobs["started_by"].is_null());
    // A session under a name its client chose has no name, and no hook call can be matched to
    // it: its subagents are not known, which is not none.
    hub.hook(&bob, "main", "SessionStart", json!({"source": "startup"}))
        .await;
    let (seen, _) = hub.sessions(&bob, WIDGETS).await;
    let unnamed = seen.iter().find(|s| s["name"].is_null()).unwrap();
    assert_eq!(unnamed["subagents"], Value::Null);

    // A subagent: its start, what its session started it to do, its ask, and its stop.
    let of = |sessions: Vec<Value>| {
        let name = parent["name"].clone();
        sessions
            .into_iter()
            .find(|s| s["name"] == name)
            .map(|s| s["subagents"].clone())
    };
    assert_eq!(of(hub.sessions(&alice, WIDGETS).await.0), Some(json!([])));
    let explore = json!({"agent_id": "a1", "agent_type": "Explore"});
    hub.hook(&alice, PARENT, "SubagentStart", explore.clone())
        .await;
    hub.hook(
        &alice,
        PARENT,
        "PostToolUse",
        json!({"tool_name": "Agent", "subagent": {"id": "a1", "type": "Explore",
            "does": "Find the session keys", "running": true}}),
    )
    .await;
    let mine = of(hub.sessions(&alice, WIDGETS).await.0).unwrap();
    assert_eq!(mine.as_array().unwrap().len(), 1, "{mine}");
    assert_eq!(
        (&mine[0]["state"], &mine[0]["type"], &mine[0]["does"]),
        (
            &json!("running"),
            &json!("Explore"),
            &json!("Find the session keys")
        )
    );
    assert!(mine[0]["started_at"].is_i64() && mine[0]["seen_at"].is_i64());
    // Anyone else sees that it runs, and none of its text.
    let (seen, text) = hub.sessions(&bob, WIDGETS).await;
    let theirs = of(seen).unwrap();
    assert_eq!(
        (&theirs[0]["state"], &theirs[0]["type"], &theirs[0]["does"]),
        (&json!("running"), &Value::Null, &Value::Null)
    );
    assert!(!text.contains("Explore") && !text.contains("session keys"));
    let state = |subagents: Value| subagents[0]["state"].clone();
    let mut ask = explore.clone();
    ask["tool_name"] = json!("Bash");
    hub.hook(&alice, PARENT, "PermissionRequest", ask).await;
    assert_eq!(
        state(of(hub.sessions(&alice, WIDGETS).await.0).unwrap()),
        "waiting"
    );
    hub.hook(&alice, PARENT, "UserPromptSubmit", json!({}))
        .await;
    assert_eq!(
        state(of(hub.sessions(&alice, WIDGETS).await.0).unwrap()),
        "running"
    );
    hub.hook(&alice, PARENT, "SubagentStop", explore).await;
    // The host also reports the stop of agents it runs for itself, which never started here.
    hub.hook(
        &alice,
        PARENT,
        "SubagentStop",
        json!({"agent_id": "internal", "agent_type": ""}),
    )
    .await;
    let ended = of(hub.sessions(&alice, WIDGETS).await.0).unwrap();
    assert_eq!(ended.as_array().unwrap().len(), 1, "{ended}");
    assert_eq!(state(ended), "ended");
    // A subagent that ran in the foreground is described when its call returns, after it stopped.
    let plan = json!({"agent_id": "a2", "agent_type": "Plan"});
    hub.hook(&alice, PARENT, "SubagentStart", plan.clone())
        .await;
    hub.hook(&alice, PARENT, "SubagentStop", plan).await;
    hub.hook(
        &alice,
        PARENT,
        "PostToolUse",
        json!({"tool_name": "Agent", "subagent": {"id": "a2", "type": "Plan",
            "does": "Plan the read", "running": false}}),
    )
    .await;
    let both = of(hub.sessions(&alice, WIDGETS).await.0).unwrap();
    assert_eq!(both.as_array().unwrap().len(), 2, "{both}");
    assert_eq!(
        (&both[1]["state"], &both[1]["does"]),
        (&json!("ended"), &json!("Plan the read"))
    );
    // The session's end takes its subagents with it.
    hub.hook(&alice, PARENT, "SessionEnd", json!({"reason": "exit"}))
        .await;
    assert_eq!(of(hub.sessions(&alice, WIDGETS).await.0), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_show_who_started_them_and_their_subagents() {
    scenario(Hub::start(None).await).await;
}

#[test]
fn postgres_sessions_show_who_started_them_and_their_subagents() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async { scenario(Hub::start(Some(pg.url.clone())).await).await });
}
