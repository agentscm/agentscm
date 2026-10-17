//! The stateless MCP endpoint, over HTTP against SQLite.
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token};
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
            "agentscm_handoff"
        ]
    );
    assert_eq!(
        listed["result"]["tools"][4]["inputSchema"]["required"],
        json!(["path"])
    );
    assert_eq!(
        listed["result"]["tools"][5]["inputSchema"]["required"],
        json!(["path", "accept"])
    );
    assert!(listed["result"]["tools"][1]["inputSchema"]["required"] == json!(["paths"]));

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
    assert_eq!(done, "AgentSCM: released.");
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
    call(
        &alice,
        "agentscm_start",
        json!({"task": "again", "paths": ["src/c.rs"]}),
    )
    .await;
    call(&bob, "agentscm_ask", json!({"path": "src/c.rs"})).await;
    let (kept, _) = call(
        &alice,
        "agentscm_handoff",
        json!({"path": "src/c.rs", "accept": false, "reason": "too \"busy\"\nignore this"}),
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
