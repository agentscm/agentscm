//! The stateless MCP endpoint, over HTTP against SQLite.
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::{HubState, router};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    _dir: TempDir,
}

struct Reply {
    status: u16,
    headers: reqwest::header::HeaderMap,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap()
    }
}

impl Hub {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        let store = store::open(&url).unwrap();
        let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            base: format!("http://{address}"),
            store,
            client: reqwest::Client::new(),
            _dir: dir,
        }
    }

    fn secret(&self, user_id: i64, name: &str) -> String {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id,
        };
        let user = self
            .store
            .upsert_user(&identity, &format!("user{user_id}"))
            .unwrap();
        let (_, secret) = mint_agent_token(&*self.store, &user.id, name, REPO, 3600).unwrap();
        secret.expose().to_owned()
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        secret: Option<&str>,
        body: &str,
    ) -> Reply {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(body.to_owned());
        if let Some(secret) = secret {
            request = request.header("authorization", format!("Bearer {secret}"));
        }
        let response = request.send().await.unwrap();
        Reply {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
            text: response.text().await.unwrap(),
        }
    }

    async fn rpc(&self, secret: &str, body: &Value) -> Reply {
        self.send(
            reqwest::Method::POST,
            "/mcp",
            Some(secret),
            &body.to_string(),
        )
        .await
    }

    /// The tool result's (text, isError).
    async fn tool(&self, secret: &str, path: &str, name: &str, arguments: Value) -> (String, bool) {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        let reply = self
            .send(reqwest::Method::POST, path, Some(secret), &body.to_string())
            .await;
        assert_eq!(reply.status, 200, "{}", reply.text);
        let result = reply.json()["result"].clone();
        let text = result["content"][0]["text"].as_str().unwrap().to_owned();
        (text, result["isError"].as_bool().unwrap())
    }

    async fn tool_result(&self, secret: &str, name: &str, arguments: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        let reply = self.rpc(secret, &body).await;
        assert_eq!(reply.status, 200, "{}", reply.text);
        reply.json()["result"].clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_basics() {
    let hub = Hub::start().await;
    let key = hub.secret(1, "alice");

    let init = |version: &str| {
        json!({"jsonrpc": "2.0", "id": "a", "method": "initialize",
            "params": {"protocolVersion": version, "capabilities": {}}})
    };
    for (asked, answered) in [
        ("2025-03-26", "2025-03-26"),
        ("2025-11-25", "2025-11-25"),
        ("1999-01-01", "2025-06-18"),
    ] {
        let reply = hub.rpc(&key, &init(asked)).await;
        assert_eq!(reply.status, 200);
        assert!(reply.headers.get("mcp-session-id").is_none());
        let body = reply.json();
        assert_eq!(body["id"], "a");
        assert_eq!(body["result"]["protocolVersion"], answered);
        assert_eq!(body["result"]["serverInfo"]["name"], "agentscm");
        assert!(
            body["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("agentscm_start")
        );
        assert!(
            body["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("agentscm_ask asks that agent to hand it over")
        );
        assert!(
            body["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("agentscm_workflow_status reads a retained native workflow run")
        );
    }

    let ping = hub
        .rpc(&key, &json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}))
        .await;
    assert_eq!(ping.json()["result"], json!({}));

    let listed = hub
        .rpc(
            &key,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await
        .json();
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "agentscm_start",
            "agentscm_check",
            "agentscm_status",
            "agentscm_done",
            "agentscm_ask",
            "agentscm_handoff",
            "agentscm_work_briefing",
            "agentscm_work_checkpoint",
            "agentscm_work_ack",
            "agentscm_workflow_status"
        ]
    );
    assert_eq!(names.len(), 10);
    assert_eq!(
        listed["result"]["tools"][4]["inputSchema"]["required"],
        json!(["path"])
    );
    assert_eq!(
        listed["result"]["tools"][5]["inputSchema"]["required"],
        json!(["path", "accept"])
    );
    assert!(listed["result"]["tools"][1]["inputSchema"]["required"] == json!(["paths"]));
    assert_eq!(
        listed["result"]["tools"][9]["inputSchema"]["required"],
        json!(["run_id"])
    );
    assert_eq!(
        listed["result"]["tools"][9]["inputSchema"]["additionalProperties"],
        false
    );
    assert_eq!(
        listed["result"]["tools"][9]["annotations"]["readOnlyHint"],
        true
    );
    assert_eq!(
        listed["result"]["tools"][6]["annotations"]["readOnlyHint"],
        true
    );
    assert_eq!(
        listed["result"]["tools"][7]["inputSchema"]["required"],
        json!([
            "work_id",
            "operation_id",
            "generation",
            "expected_updated_seq",
            "session",
            "kind",
            "summary"
        ])
    );
    assert_eq!(
        listed["result"]["tools"][7]["inputSchema"]["properties"]["expected_updated_seq"]["type"],
        "integer"
    );

    // Notifications and client responses are accepted without a body.
    for message in [
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "method": "no/such"}),
        json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
    ] {
        let reply = hub.rpc(&key, &message).await;
        assert_eq!((reply.status, reply.text.as_str()), (202, ""), "{message}");
    }

    let unknown = hub
        .rpc(
            &key,
            &json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}),
        )
        .await
        .json();
    assert_eq!(unknown["error"]["code"], -32601);
    let post = reqwest::Method::POST;
    let batch = hub
        .send(
            post.clone(),
            "/mcp",
            Some(&key),
            "[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}]",
        )
        .await;
    assert_eq!(batch.json()["error"]["code"], -32600);
    assert_eq!(batch.json()["id"], Value::Null);
    let garbage = hub.send(post.clone(), "/mcp", Some(&key), "{nope").await;
    assert_eq!(
        (garbage.status, garbage.json()["error"]["code"].clone()),
        (200, json!(-32700))
    );
    let bad_id = hub
        .rpc(
            &key,
            &json!({"jsonrpc": "2.0", "id": [1], "method": "ping"}),
        )
        .await;
    assert_eq!(bad_id.json()["error"]["code"], -32600);
    let no_method = hub.rpc(&key, &json!({"jsonrpc": "2.0", "id": 5})).await;
    assert_eq!(no_method.json()["error"]["code"], -32600);
    assert_eq!(no_method.json()["id"], 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_and_auth_rules() {
    let hub = Hub::start().await;
    let key = hub.secret(1, "alice");
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string();

    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let reply = hub.send(method, "/mcp", Some(&key), "").await;
        assert_eq!((reply.status, reply.text.as_str()), (405, ""));
        assert_eq!(reply.headers["allow"], "POST");
    }
    for secret in [None, Some("agt_not-a-token")] {
        let reply = hub.send(reqwest::Method::POST, "/mcp", secret, &ping).await;
        assert_eq!(reply.status, 401);
        assert_eq!(reply.headers["www-authenticate"], "Bearer");
    }
    assert!(
        hub.rpc(&key, &json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
            .await
            .headers
            .get("access-control-allow-origin")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_agents_round_trip_to_a_stop() {
    let hub = Hub::start().await;
    let alice = hub.secret(1, "alice-agent");
    let bob = hub.secret(2, "bob-agent");

    let (text, error) = hub
        .tool(
            &alice,
            "/mcp",
            "agentscm_start",
            json!({"task": "fix billing", "paths": ["src/billing.rs"]}),
        )
        .await;
    assert!(!error);
    assert!(
        text.contains("no other agents are working in acme/widgets"),
        "{text}"
    );
    assert!(!text.contains("already working"));

    let (text, error) = hub
        .tool(
            &bob,
            "/mcp",
            "agentscm_start",
            json!({"task": "refactor docs"}),
        )
        .await;
    assert!(!error);
    assert!(
        text.contains("1 other agent") && text.contains("fix billing"),
        "{text}"
    );

    let (clean, _) = hub
        .tool(
            &bob,
            "/mcp",
            "agentscm_check",
            json!({"paths": ["docs/a.md"]}),
        )
        .await;
    assert_eq!(
        clean,
        "AgentSCM: nothing new about these files since this session's last report."
    );

    let (stop, error) = hub
        .tool(
            &bob,
            "/mcp",
            "agentscm_check",
            json!({"paths": ["src/billing.rs"]}),
        )
        .await;
    assert!(!error);
    assert!(
        stop.starts_with("AgentSCM: another agent is already working on src/billing.rs."),
        "{stop}"
    );
    assert!(stop.contains("alice-agent"), "{stop}");

    // Checked once, not reported again.
    let (again, _) = hub
        .tool(
            &bob,
            "/mcp",
            "agentscm_check",
            json!({"paths": ["src/billing.rs"]}),
        )
        .await;
    assert_eq!(
        again,
        "AgentSCM: nothing new about these files since this session's last report."
    );

    // Alice's next heartbeat carries the notice that bob came after her.
    let (status, error) = hub.tool(&alice, "/mcp", "agentscm_status", json!({})).await;
    assert!(!error);
    assert!(
        status.contains("started working on src/billing.rs"),
        "{status}"
    );
    assert!(
        status.contains("1 other agent") && status.contains("no open pull requests"),
        "{status}"
    );

    let (done, _) = hub.tool(&bob, "/mcp", "agentscm_done", json!({})).await;
    assert!(
        done.starts_with("AgentSCM: released; unfinished durable work was paused"),
        "{done}"
    );
    let (done, _) = hub.tool(&bob, "/mcp", "agentscm_done", json!({})).await;
    assert_eq!(done, "AgentSCM: nothing to release.");
    let (status, _) = hub.tool(&alice, "/mcp", "agentscm_status", json!({})).await;
    assert!(status.contains("no other agents"), "{status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_agent_asks_and_the_holder_hands_the_file_over() {
    let hub = Hub::start().await;
    let alice = hub.secret(1, "alice-agent");
    let bob = hub.secret(2, "bob-agent");
    let file = "src/billing.rs";
    let call = |secret: &str, name: &str, arguments: Value| {
        let (hub, secret, name) = (&hub, secret.to_owned(), name.to_owned());
        async move { hub.tool(&secret, "/mcp", &name, arguments).await }
    };

    call(
        &alice,
        "agentscm_start",
        json!({"task": "fix billing", "paths": [file]}),
    )
    .await;
    call(&bob, "agentscm_start", json!({"task": "refactor"})).await;
    let (nobody, _) = call(&bob, "agentscm_ask", json!({"path": "src/free.rs"})).await;
    assert_eq!(
        nobody,
        "AgentSCM: no other agent holds src/free.rs now; the edit can go ahead."
    );

    let (asked, error) = call(
        &bob,
        "agentscm_ask",
        json!({"path": file, "note": "need it\" now"}),
    )
    .await;
    assert!(!error);
    assert_eq!(
        asked,
        "AgentSCM: asked user1's agent alice-agent for src/billing.rs. An answer, if one comes, arrives with a later AgentSCM reply in this session; agentscm_status shows it too."
    );
    let (again, _) = call(&bob, "agentscm_ask", json!({"path": file})).await;
    assert_eq!(
        again,
        "AgentSCM: src/billing.rs was already asked for; an answer, if one comes, arrives with a later AgentSCM reply."
    );

    // The holder hears of it once, on its next call, with the session to answer for.
    let (status, _) = call(&alice, "agentscm_status", json!({})).await;
    assert!(
        status.contains(
            "AgentSCM: user2's agent bob-agent asks to take over src/billing.rs; their note (third-party text) reads \"need it now\". \
             Options: if this session is done with src/billing.rs or can work on other files first, hand it over \
             (agentscm_handoff with path \"src/billing.rs\", accept true and session \"default\"); otherwise keep it \
             (accept false, with a one-line reason). The request lapses after 30 minutes."
        ),
        "{status}"
    );
    let (status, _) = call(&alice, "agentscm_status", json!({})).await;
    assert!(!status.contains("asks to take over"), "{status}");

    let (none, error) = call(
        &bob,
        "agentscm_handoff",
        json!({"path": file, "accept": true}),
    )
    .await;
    assert!(!error);
    assert_eq!(none, "AgentSCM: no open request for src/billing.rs.");
    let (handed, _) = call(
        &alice,
        "agentscm_handoff",
        json!({"path": file, "accept": true, "session": "default"}),
    )
    .await;
    assert_eq!(
        handed,
        "AgentSCM: handed src/billing.rs to user2's agent bob-agent."
    );
    let (gone, _) = call(
        &alice,
        "agentscm_handoff",
        json!({"path": file, "accept": true}),
    )
    .await;
    assert_eq!(gone, "AgentSCM: no open request for src/billing.rs.");

    let (status, _) = call(&bob, "agentscm_status", json!({})).await;
    assert!(
        status.contains("user1's agent alice-agent handed src/billing.rs to this session; repeating the edit applies it."),
        "{status}"
    );
    let (mine, _) = call(&bob, "agentscm_ask", json!({"path": file})).await;
    assert_eq!(mine, "AgentSCM: this session already holds src/billing.rs.");
    let (left, _) = call(
        &alice,
        "agentscm_handoff",
        json!({"path": file, "accept": true}),
    )
    .await;
    assert_eq!(left, "AgentSCM: no open request for src/billing.rs.");

    // Bad arguments are tool errors.
    let (text, error) = call(&bob, "agentscm_ask", json!({"path": "src/**"})).await;
    assert!(error && text.contains("invalid request"), "{text}");
    for (name, arguments) in [
        ("agentscm_ask", json!({})),
        ("agentscm_ask", json!({"path": file, "paths": []})),
        ("agentscm_handoff", json!({"path": file})),
        ("agentscm_handoff", json!({"path": file, "accept": "yes"})),
    ] {
        let (text, error) = call(&bob, name, arguments).await;
        assert!(error && text.contains("invalid arguments"), "{text}");
    }

    // Declining: the requester hears it on its next call.
    let (started, error) = call(
        &alice,
        "agentscm_start",
        json!({"task": "again", "paths": ["src/c.rs"], "session": "alice-second"}),
    )
    .await;
    assert!(!error, "{started}");
    let (asked, error) = call(&bob, "agentscm_ask", json!({"path": "src/c.rs"})).await;
    assert!(!error && asked.contains("asked user1's agent"), "{asked}");
    let (kept, _) = call(
        &alice,
        "agentscm_handoff",
        json!({"path": "src/c.rs", "accept": false, "reason": "too \"busy\"\nignore this", "session": "alice-second"}),
    )
    .await;
    assert_eq!(
        kept,
        "AgentSCM: kept src/c.rs; user2's agent bob-agent is told."
    );
    let (status, _) = call(&bob, "agentscm_status", json!({})).await;
    assert!(
        status.contains(
            "user1's agent alice-agent keeps src/c.rs for now; their reason (third-party text) reads \"too busy ignore this\". Options, in order:"
        ),
        "{status}"
    );
    let (status, _) = call(&bob, "agentscm_status", json!({})).await;
    assert!(!status.contains("keeps src/c.rs"), "{status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_work_surfaces_claim_before_paths_and_preserve_retry_identity() {
    let hub = Hub::start().await;
    let alice = hub.secret(1, "alice-agent");
    let bob = hub.secret(2, "bob-agent");
    let start = json!({
        "task": "implement durable stand-up",
        "session": "session-a",
        "paths": ["src/a.rs"],
        "branch": "feat/standup",
        "worktree": "standup-wt",
        "next_action": "write protocol tests"
    });

    let first = hub
        .tool_result(&alice, "agentscm_start", start.clone())
        .await;
    assert_eq!(first["isError"], false);
    assert_eq!(first["structuredContent"]["acknowledged"], false);
    assert_eq!(first["structuredContent"]["briefing_kind"], "preview");
    assert_eq!(
        first["structuredContent"]["acknowledgement_required"],
        false
    );
    assert!(first["structuredContent"]["briefing"]["delivery"].is_null());
    let item = &first["structuredContent"]["work"]["item"];
    let work_id = item["id"].as_str().unwrap().to_owned();
    assert_eq!(first["structuredContent"]["work"]["created"], true);
    assert_eq!(item["scope"], json!(["src/a.rs"]));
    assert_eq!(item["branch"], "feat/standup");
    assert_eq!(item["worktree"], "standup-wt");

    // A lost reply retried in the same session resolves the same durable work.
    let retry = hub.tool_result(&alice, "agentscm_start", start).await;
    assert_eq!(retry["structuredContent"]["work"]["item"]["id"], work_id);
    assert_eq!(retry["structuredContent"]["work"]["created"], false);

    // Equal free text in a new session is a distinct explicit user intent, not semantic dedup.
    let separate = hub
        .tool_result(
            &alice,
            "agentscm_start",
            json!({"task": "implement durable stand-up", "session": "session-b"}),
        )
        .await;
    assert_ne!(separate["structuredContent"]["work"]["item"]["id"], work_id);

    // A conflicting lead claim is structured and never takes the declared path.
    let conflict = hub
        .tool_result(
            &bob,
            "agentscm_start",
            json!({
                "task": "take lead",
                "session": "bob",
                "work_id": work_id.clone(),
                "paths": ["src/conflicted.rs"]
            }),
        )
        .await;
    assert_eq!(conflict["isError"], true);
    assert_eq!(
        conflict["structuredContent"]["work"]["conflict"]["kind"],
        "claim_held"
    );
    assert!(
        conflict["structuredContent"]["alternatives"]
            .as_array()
            .is_some_and(|alternatives| (1..=3).contains(&alternatives.len())),
        "{conflict}"
    );
    let live = hub.store.live_intents(REPO, unix_now(), 100).unwrap();
    assert!(
        live.iter()
            .all(|intent| !intent.paths.contains(&"src/conflicted.rs".to_owned())),
        "conflicting claim announced a path: {live:?}"
    );

    let generation = first["structuredContent"]["work"]["item"]["generation"]
        .as_i64()
        .unwrap();
    let expected_updated_seq = first["structuredContent"]["work"]["item"]["updated_seq"]
        .as_i64()
        .unwrap();
    let checkpoint = json!({
        "work_id": work_id.clone(),
        "operation_id": "progress-1",
        "generation": generation,
        "expected_updated_seq": expected_updated_seq,
        "session": "session-a",
        "kind": "progress",
        "summary": "protocol surface implemented"
    });
    let progress = hub
        .tool_result(&alice, "agentscm_work_checkpoint", checkpoint.clone())
        .await;
    assert_eq!(progress["isError"], false);
    assert_eq!(
        progress["structuredContent"]["item"]["current"]["kind"],
        "progress"
    );
    assert_eq!(
        progress["structuredContent"]["receipt"]["operation_id"],
        "progress-1"
    );
    assert_eq!(progress["structuredContent"]["receipt"]["work_id"], work_id);
    assert_eq!(progress["structuredContent"]["receipt"]["replayed"], false);
    let progress_event_seq = progress["structuredContent"]["receipt"]["event_seq"]
        .as_i64()
        .unwrap();
    let repeated = hub
        .tool_result(&alice, "agentscm_work_checkpoint", checkpoint.clone())
        .await;
    assert_eq!(
        repeated["structuredContent"]["item"],
        progress["structuredContent"]["item"]
    );
    assert_eq!(
        repeated["structuredContent"]["receipt"]["event_seq"],
        progress_event_seq
    );
    assert_eq!(repeated["structuredContent"]["receipt"]["replayed"], true);
    let stale = hub
        .tool_result(
            &alice,
            "agentscm_work_checkpoint",
            json!({
                "work_id": work_id.clone(),
                "operation_id": "progress-stale-observation",
                "generation": generation,
                "expected_updated_seq": expected_updated_seq,
                "session": "session-a",
                "kind": "progress",
                "summary": "must not overwrite newer work"
            }),
        )
        .await;
    assert_eq!(stale["isError"], true);
    let changed = hub
        .tool_result(
            &alice,
            "agentscm_work_checkpoint",
            json!({
                "work_id": work_id.clone(),
                "operation_id": "progress-1",
                "generation": generation,
                "expected_updated_seq": expected_updated_seq,
                "session": "session-a",
                "kind": "progress",
                "summary": "different bytes"
            }),
        )
        .await;
    assert_eq!(changed["isError"], true);

    let current_updated_seq = progress["structuredContent"]["item"]["updated_seq"]
        .as_i64()
        .unwrap();
    let later = hub
        .tool_result(
            &alice,
            "agentscm_work_checkpoint",
            json!({
                "work_id": work_id.clone(),
                "operation_id": "progress-2",
                "generation": generation,
                "expected_updated_seq": current_updated_seq,
                "session": "session-a",
                "kind": "decision",
                "summary": "retain typed checkpoint receipts"
            }),
        )
        .await;
    assert_eq!(later["isError"], false);
    let resolved_retry = hub
        .tool_result(&alice, "agentscm_work_checkpoint", checkpoint.clone())
        .await;
    assert_eq!(
        resolved_retry["structuredContent"]["receipt"]["event_seq"],
        progress_event_seq
    );
    assert_eq!(
        resolved_retry["structuredContent"]["receipt"]["replayed"],
        true
    );
    assert_eq!(
        resolved_retry["structuredContent"]["item"],
        later["structuredContent"]["item"]
    );
    assert_eq!(
        resolved_retry["structuredContent"]["item"]["current"]["kind"],
        "decision"
    );

    let later_updated_seq = later["structuredContent"]["item"]["updated_seq"]
        .as_i64()
        .unwrap();
    let handover = hub
        .tool_result(
            &alice,
            "agentscm_work_checkpoint",
            json!({
                "work_id": work_id.clone(),
                "operation_id": "handover-1",
                "generation": generation,
                "expected_updated_seq": later_updated_seq,
                "session": "session-a",
                "kind": "handover",
                "summary": "ready for another lead"
            }),
        )
        .await;
    assert_eq!(handover["isError"], false);
    let takeover = hub
        .tool_result(
            &bob,
            "agentscm_start",
            json!({
                "task": "implement durable stand-up",
                "session": "bob",
                "work_id": work_id.clone(),
                "paths": ["src/a.rs"],
                "branch": "feat/standup",
                "worktree": "standup-wt",
                "takeover_generation": generation
            }),
        )
        .await;
    assert_eq!(takeover["isError"], false, "{takeover}");
    assert_eq!(takeover["structuredContent"]["work"]["took_over"], true);

    // A lost old-generation result remains resolvable by exact operation retry after takeover.
    let post_takeover_retry = hub
        .tool_result(&alice, "agentscm_work_checkpoint", checkpoint)
        .await;
    assert_eq!(post_takeover_retry["isError"], false);
    assert_eq!(
        post_takeover_retry["structuredContent"]["receipt"]["event_seq"],
        progress_event_seq
    );
    assert_eq!(
        post_takeover_retry["structuredContent"]["receipt"]["replayed"],
        true
    );
    assert_eq!(
        post_takeover_retry["structuredContent"]["item"]["generation"],
        generation + 1
    );
    assert_eq!(
        post_takeover_retry["structuredContent"]["item"]["lead_session"],
        "bob"
    );
    let new_old_generation_operation = hub
        .tool_result(
            &alice,
            "agentscm_work_checkpoint",
            json!({
                "work_id": work_id,
                "operation_id": "old-generation-new-operation",
                "generation": generation,
                "expected_updated_seq": takeover["structuredContent"]["work"]["item"]
                    ["updated_seq"],
                "session": "session-a",
                "kind": "progress",
                "summary": "must remain fenced after takeover"
            }),
        )
        .await;
    assert_eq!(new_old_generation_operation["isError"], true);

    let briefing = hub
        .tool_result(&alice, "agentscm_work_briefing", json!({"limit": 20}))
        .await;
    assert_eq!(briefing["isError"], false);
    let delivery = briefing["structuredContent"]["delivery"].as_str().unwrap();
    assert_eq!(briefing["structuredContent"]["complete"], true);
    let ack = hub
        .tool_result(&alice, "agentscm_work_ack", json!({"delivery": delivery}))
        .await;
    assert_eq!(ack["structuredContent"], json!({"acknowledged": true}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_are_tool_results_with_fixed_text() {
    let hub = Hub::start().await;
    let key = hub.secret(1, "alice");

    // A token for another repository, named by the client.
    for path in ["/mcp?repo=acme/other", "/mcp?repo=acme%2Fother"] {
        let (text, error) = hub.tool(&key, path, "agentscm_status", json!({})).await;
        assert!(error);
        assert_eq!(
            text,
            "AgentSCM: coordination unknown (this token is for another repository). \
             Other agents' work on these files is not visible."
        );
    }
    let (_, error) = hub
        .tool(&key, "/mcp?repo=acme/widgets", "agentscm_status", json!({}))
        .await;
    assert!(!error);

    // Arguments that do not fit, and input the store rejects, never echo the input.
    let secret_looking = "../sekrit-path";
    for (name, arguments) in [
        ("agentscm_start", json!({})),
        ("agentscm_start", json!({"task": "t", "extra": 1})),
        ("agentscm_start", json!({"task": 5})),
        ("agentscm_check", json!({"paths": []})),
        ("agentscm_check", json!({"paths": "src/a.rs"})),
        ("agentscm_status", json!({"unknown": true})),
    ] {
        let (text, error) = hub.tool(&key, "/mcp", name, arguments).await;
        assert!(error, "{name}");
        assert!(
            text.contains("coordination unknown (invalid arguments)"),
            "{text}"
        );
    }
    let (text, error) = hub
        .tool(
            &key,
            "/mcp",
            "agentscm_check",
            json!({"paths": [secret_looking]}),
        )
        .await;
    assert!(error && !text.contains("sekrit"), "{text}");
    assert!(text.contains("(invalid request)"), "{text}");

    // An unknown tool or a call without a name is a protocol error.
    for params in [json!({"name": "nope"}), json!({}), json!("x")] {
        let reply = hub
            .rpc(
                &key,
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params}),
            )
            .await;
        assert_eq!(reply.json()["error"]["code"], -32602);
    }
}
