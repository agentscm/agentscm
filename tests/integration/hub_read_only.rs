//! A read-only token (the status page's) reaches read routes and the side-effect-free MCP
//! negotiation/read methods. Every mutating route refuses it and writes nothing. Add a row to
//! `routes()` for each new route.
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{
    self, ForgeIdentity, Store, agent_token_digest, mint_bound_agent_token, unix_now,
};
use agentscm::hub::{HubState, router};
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "/v1/hub/repos/acme%2Fwidgets";

/// What a read-only bearer token gets.
#[derive(Clone, Copy)]
enum Want {
    /// 200: a read route, or one that needs no token.
    Ok,
    /// 403 `read_only`: a route for agent tokens.
    AgentOnly,
    /// 401: a route for human sessions, which no agent token opens.
    HumanOnly,
    /// The reply an unknown token gets.
    AsUnknownToken,
    /// Not an agent or session route: the exact status, which must not be a success.
    Status(u16),
}
use Want::{AgentOnly, AsUnknownToken, HumanOnly, Ok as Allowed, Status};

fn rpc(tool: &str) -> String {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": tool, "arguments": {"task": "t", "paths": ["a.rs"]}}})
    .to_string()
}

#[rustfmt::skip]
fn routes() -> Vec<(Method, String, String, Want)> {
    let (get, post, put, delete) = (Method::GET, Method::POST, Method::PUT, Method::DELETE);
    let id = format!("pub_{}", "a".repeat(32));
    let hash = "b".repeat(64);
    let intent = format!("int_{}", "a".repeat(32));
    let announce = json!({"intent_id": intent, "task": "t", "paths": ["a.rs"], "ttl_s": 60});
    let mut rows: Vec<(Method, String, String, Want)> = Vec::new();
    let mut row = |method: &Method, path: String, body: String, want: Want| {
        rows.push((method.clone(), path, body, want));
    };
    // The routes a read-only token may call, and those that need no token.
    row(&get, "/v1/hub/health".into(), "".into(), Allowed);
    row(&get, "/v1/hub/whoami".into(), "".into(), Allowed);
    row(&get, format!("{REPO}/intents"), "".into(), Allowed);
    row(&get, format!("{REPO}/work"), "".into(), Allowed);
    row(&get, format!("{REPO}/activity"), "".into(), Allowed);
    row(&get, format!("{REPO}/change-records/wrk_missing"), "".into(), Status(404));
    row(&get, "/v1/hub/skill-workflows".into(), "".into(), Allowed);
    row(&get, "/v1/hub/skill-workflows/missing".into(), "".into(), Allowed);
    row(&get, "/v1/hub/standup/briefing".into(), "".into(), Allowed);
    row(&get, "/v1/hub/standup/insights".into(), "".into(), Allowed);
    row(&get, "/v1/hub/standup/work/missing/insights".into(), "".into(), Status(404));
    row(&get, "/v1/hub/standup/work/missing".into(), "".into(), Status(404));
    row(&get, "/v1/hub/standup/work?q=x".into(), "".into(), Allowed);
    row(&get, format!("{REPO}/publications?q=x"), "".into(), Allowed);
    row(&get, "/v1/hub/skill-workflows?q=x".into(), "".into(), Allowed);
    row(&get, format!("{REPO}/activity?before=5"), "".into(), Allowed);
    for tool in ["agentscm_work_search", "agentscm_publications_list", "agentscm_workflow_runs_list"] {
        row(&post, "/mcp".into(), format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{tool}","arguments":{{"q":"x"}}}}}}"#), Allowed);
    }
    for path in ["start", "checkpoint", "ack"] {
        row(&post, format!("/v1/hub/standup/{path}"), "{}".into(), AgentOnly);
    }
    row(&post, "/v1/hub/standup/usage".into(), "{}".into(), AgentOnly);
    row(&post, "/mcp".into(), r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_work_briefing","arguments":{}}}"#.into(), Allowed);
    row(&post, "/mcp".into(), r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_work_insights","arguments":{}}}"#.into(), Allowed);
    // Agent-token routes.
    row(&post, format!("{REPO}/intents"), announce.to_string(), AgentOnly);
    row(&post, format!("{REPO}/intents/{intent}/renew"), r#"{"ttl_s":60}"#.into(), AgentOnly);
    row(&delete, format!("{REPO}/intents/{intent}"), "".into(), AgentOnly);
    // Notices are per token and long-poll: agent-only.
    row(&get, format!("{REPO}/notices"), "".into(), AgentOnly);
    row(&post, format!("{REPO}/publications"), "{}".into(), AgentOnly);
    row(&get, format!("{REPO}/publications"), "".into(), Allowed);
    row(&get, format!("{REPO}/publications/{id}"), "".into(), Status(404));
    row(&put, format!("{REPO}/publications/{id}/blobs/{hash}"), "x".into(), AgentOnly);
    row(&post, format!("{REPO}/publications/{id}/run"), "{}".into(), AgentOnly);
    row(&get, format!("{REPO}/change-records/current?session_id=session-1"), "".into(), AgentOnly);
    row(&post, format!("{REPO}/change-records/events"), "{}".into(), AgentOnly);
    row(&post, format!("{REPO}/change-records/actions/start"), "{}".into(), AgentOnly);
    row(&post, format!("{REPO}/change-records/actions/finish"), "{}".into(), AgentOnly);
    row(&get, format!("{REPO}/change-records/wrk_missing/conversation"), "".into(), Status(404));
    row(&get, format!("{REPO}/change-records/wrk_missing/conversation/{hash}"), "".into(), Status(404));
    row(&put, format!("{REPO}/change-records/wrk_missing/conversation/{hash}"), "x".into(), AgentOnly);
    row(&delete, format!("{REPO}/change-records/wrk_missing/conversation/{hash}"), "".into(), AgentOnly);
    row(&get, "/v1/hub/skills".into(), "".into(), Allowed);
    row(&get, format!("{REPO}/skills/runs"), "".into(), Allowed);
    row(&get, format!("{REPO}/skills/runs/missing"), "".into(), Status(404));
    row(&put, format!("{REPO}/skills/adoption"), "{}".into(), AgentOnly);
    row(&post, format!("{REPO}/skills/evidence"), "{}".into(), AgentOnly);
    // /mcp: negotiation and the repository-scoped workflow read are side-effect-free.
    row(&post, "/mcp".into(), r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}"#.into(), Allowed);
    row(&post, "/mcp".into(), r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.into(), Allowed);
    row(&post, "/mcp".into(), r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.into(), Allowed);
    row(&post, "/mcp".into(), r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_workflow_status","arguments":{"run_id":"missing"}}}"#.into(), Allowed);
    row(&post, "/mcp".into(), r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_work_briefing","arguments":{}}}"#.into(), Allowed);
    row(&post, "/mcp".into(), r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_change_record","arguments":{"work_id":"wrk_missing"}}}"#.into(), Allowed);
    row(&post, "/mcp".into(), r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_shared_conversation","arguments":{"work_id":"wrk_missing"}}}"#.into(), Allowed);
    // Coordination and checkpoint tools mutate state.
    for tool in [
        "agentscm_start",
        "agentscm_check",
        "agentscm_status",
        "agentscm_done",
        "agentscm_ask",
        "agentscm_handoff",
        "agentscm_work_checkpoint",
        "agentscm_work_ack",
        "agentscm_work_usage",
        "agentscm_skill_evidence",
    ] {
        row(&post, "/mcp".into(), rpc(tool), AgentOnly);
    }
    // Hooks answer as for an unknown token.
    for agent in ["claude", "codex", "cursor", "copilot", "generic"] {
        let body = if agent == "generic" {
            json!({"event": "check", "session": "s", "paths": ["a.rs"]})
        } else {
            json!({"hook_event_name": "PreToolUse", "session_id": "s",
                "tool_name": "Edit", "tool_input": {"file_path": "a.rs"}})
        };
        row(&post, format!("/v1/hub/hook/{agent}"), body.to_string(), AsUnknownToken);
    }
    // Human-session routes.
    row(&post, "/v1/hub/tokens".into(), r#"{"name":"n","repository":"acme/widgets","ttl_seconds":60}"#.into(), HumanOnly);
    row(&get, "/v1/hub/tokens".into(), "".into(), HumanOnly);
    row(&delete, "/v1/hub/tokens/tok_x".into(), "".into(), HumanOnly);
    row(&get, "/v1/hub/session".into(), "".into(), HumanOnly);
    row(&delete, "/v1/hub/session".into(), "".into(), HumanOnly);
    row(&post, "/v1/hub/session/read-token".into(), r#"{"repository":"acme/widgets"}"#.into(), HumanOnly);
    row(&get, "/v1/hub/session/repositories".into(), "".into(), HumanOnly);
    row(&get, "/v1/hub/users".into(), "".into(), HumanOnly);
    row(&post, "/v1/hub/users/usr_x/disable".into(), "".into(), HumanOnly);
    row(&post, "/v1/hub/users/usr_x/enable".into(), "".into(), HumanOnly);
    row(&get, "/v1/hub/connections".into(), "".into(), HumanOnly);
    row(&post, "/v1/hub/connections/con_x/refresh".into(), "".into(), HumanOnly);
    row(&post, "/v1/hub/connections/con_x/outbound".into(), r#"{"enabled":true}"#.into(), HumanOnly);
    row(&post, "/v1/hub/connections/con_x/repositories".into(), "{}".into(), HumanOnly);
    row(&post, "/v1/hub/github/apps".into(), "{}".into(), HumanOnly);
    // Not token routes: sign-in steps, setup pages and webhooks, each refused without their own proof.
    row(&get, "/v1/hub/github/setup/callback".into(), "".into(), Status(404));
    row(&get, "/v1/hub/github/setup/state".into(), "".into(), Status(404));
    row(&post, "/v1/hub/webhooks/github/con_x".into(), "{}".into(), Status(404));
    row(&post, "/v1/hub/webhooks/gitlab/con_x".into(), "{}".into(), Status(404));
    row(&post, "/v1/hub/login/exchange".into(), r#"{"code":"c","verifier":"v"}"#.into(), Status(400));
    row(&post, "/v1/hub/login/device/poll".into(), r#"{"handle":"h"}"#.into(), Status(400));
    row(&get, "/v1/hub/login/github".into(), "".into(), Status(404));
    row(&get, "/v1/hub/login/github/callback".into(), "".into(), Status(404));
    row(&post, "/v1/hub/login/github/device".into(), "".into(), Status(404));
    rows
}

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    _dir: TempDir,
}

impl Hub {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        let store = store::open(&url).unwrap();
        let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            base,
            store,
            client: reqwest::Client::new(),
            _dir: dir,
        }
    }

    async fn send(
        &self,
        row: &(Method, String, String, Want),
        secret: Option<&str>,
    ) -> (u16, String) {
        let (method, path, body, _) = row;
        let mut request = self
            .client
            .request(method.clone(), format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(body.clone());
        if let Some(secret) = secret {
            request = request.bearer_auth(secret);
        }
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_token_reaches_only_the_read_routes_and_writes_nothing() {
    let hub = Hub::start().await;
    let user = hub
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 1,
            },
            "alice",
        )
        .unwrap();
    let mint = |name: &str, read_only: bool| {
        let (_, secret, _) = mint_bound_agent_token(
            &*hub.store,
            &user.id,
            name,
            "acme/widgets",
            1,
            600,
            read_only,
        )
        .unwrap();
        secret
    };
    let (secret, agent) = (mint("page", true), mint("agent", false));
    let scope = hub
        .store
        .authenticate(&agent_token_digest(agent.expose()).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    let (secret, agent) = (secret.expose().to_owned(), agent.expose().to_owned());
    let events = hub.store.events_after(0, 1000).unwrap().len();

    for row in routes() {
        let (method, path, _, want) = &row;
        let label = format!("{method} {path}");
        let (status, body) = hub.send(&row, Some(&secret)).await;
        match want {
            Allowed => assert_eq!(status, 200, "{label}: {body}"),
            AgentOnly => {
                assert_eq!(status, 403, "{label}: {body}");
                assert_eq!(
                    serde_json::from_str::<Value>(&body).unwrap(),
                    json!({"error": "read_only"}),
                    "{label}"
                );
            }
            HumanOnly => assert_eq!(status, 401, "{label}: {body}"),
            AsUnknownToken => {
                let bogus = hub.send(&row, Some("agt_not-a-token")).await;
                assert_eq!((status, body), bogus, "{label}");
            }
            Status(expected) => assert_eq!(status, *expected, "{label}: {body}"),
        }
        assert_eq!(
            hub.store.events_after(0, 1000).unwrap().len(),
            events,
            "{label} wrote an event"
        );
        assert!(
            hub.store
                .live_intents("acme/widgets", unix_now(), 10)
                .unwrap()
                .is_empty(),
            "{label}"
        );
        assert!(
            hub.store
                .live_sessions(&scope, unix_now(), 10)
                .unwrap()
                .is_empty(),
            "{label}"
        );
    }

    // The same writes work for an ordinary agent token, so the refusals above are the kind's.
    let announce = routes()
        .into_iter()
        .find(|r| matches!(r.3, AgentOnly) && r.0 == Method::POST && r.1.ends_with("/intents"))
        .unwrap();
    assert_eq!(hub.send(&announce, Some(&agent)).await.0, 200);

    let whoami = hub
        .client
        .get(format!("{}/v1/hub/whoami", hub.base))
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&whoami.text().await.unwrap()).unwrap();
    assert_eq!(body["token"]["read_only"], true);
    assert_eq!(body["token"]["repository_id"], 1);
}
