use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{ForgeIdentity, Store, mint_bound_agent_token};
use agentscm::hub::{HubState, router, store};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPOSITORY: &str = "acme/widgets";

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    _directory: TempDir,
}

impl Hub {
    async fn start() -> Self {
        let directory = TempDir::new().unwrap();
        let database = format!("sqlite://{}", directory.path().join("hub.sqlite").display());
        let store = store::open(&database).unwrap();
        let objects = LocalBackend::open(directory.path().join("objects")).unwrap();
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            base,
            store,
            client: reqwest::Client::new(),
            _directory: directory,
        }
    }

    fn token(&self, read_only: bool) -> String {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "forge.example".into(),
            user_id: if read_only { 2 } else { 1 },
        };
        let user = self
            .store
            .upsert_user(&identity, if read_only { "reader" } else { "writer" })
            .unwrap();
        let (_, secret, _) = mint_bound_agent_token(
            &*self.store,
            &user.id,
            if read_only { "reader" } else { "writer" },
            REPOSITORY,
            77,
            600,
            read_only,
        )
        .unwrap();
        secret.expose().to_owned()
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let bytes = response.bytes().await.unwrap();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes)}));
        (status, body)
    }
}

fn rpc(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})
}

async fn start_work(hub: &Hub, token: &str) -> Value {
    let (status, body) = hub.request(reqwest::Method::POST, "/v1/hub/standup/start", token, Some(json!({
        "work_id":null,"source":null,"request_key":"insight-contract","title":"verify insight contracts",
        "session":"surface-session","role":"lead","scope":["src/synthetic.rs"],"branch":null,
        "worktree":null,"next_action":null,"depends_on":[],"related_work":[],"takeover_generation":null
    }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn usage(work_id: &str, observation_id: &str) -> Value {
    json!({
        "work_id":work_id,"observation_id":observation_id,"generation":1,"session":"surface-session",
        "phase":"implementation","outcome":"succeeded","agent":"custom-mcp-client",
        "provider":"provider-without-enum-registration","model":"model/2026.experimental",
        "usage_basis":"provider_result","input_tokens":null,"output_tokens":37,
        "cost":{"kind":"reported_charge","currency":"EUR","amount_micros":1234,"basis":"provider-result-v7"}
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_usage_is_strict_idempotent_and_provider_neutral() {
    let hub = Hub::start().await;
    let writer = hub.token(false);
    let work = start_work(&hub, &writer).await;
    let id = work["item"]["id"].as_str().unwrap();
    let report = usage(id, "observation-one");

    let (status, first) = hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/usage",
            &writer,
            Some(report.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["replayed"], false);
    let (status, replay) = hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/usage",
            &writer,
            Some(report.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true);
    let (status, mcp_usage) = hub
        .request(
            reqwest::Method::POST,
            "/mcp",
            &writer,
            Some(rpc("agentscm_work_usage", usage(id, "mcp-observation"))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{mcp_usage}");
    assert_eq!(mcp_usage["result"]["isError"], false);
    assert_eq!(mcp_usage["result"]["structuredContent"]["work_id"], id);
    assert_eq!(
        mcp_usage["result"]["structuredContent"]["observation_id"],
        "mcp-observation"
    );
    assert_eq!(mcp_usage["result"]["structuredContent"]["replayed"], false);

    let mut changed = report.clone();
    changed["output_tokens"] = json!(38);
    let (status, _) = hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/usage",
            &writer,
            Some(changed),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let mut unknown = report;
    unknown["unexpected"] = json!(true);
    unknown["observation_id"] = json!("observation-two");
    let (status, _) = hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/usage",
            &writer,
            Some(unknown),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, insight) = hub
        .request(
            reqwest::Method::GET,
            &format!("/v1/hub/standup/work/{id}/insights"),
            &writer,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{insight}");
    assert_eq!(
        insight["usage"]["token_groups"][0]["agent"],
        "custom-mcp-client"
    );
    assert_eq!(
        insight["usage"]["token_groups"][0]["provider"],
        "provider-without-enum-registration"
    );
    assert_eq!(
        insight["usage"]["token_groups"][0]["model"],
        "model/2026.experimental"
    );
    assert_eq!(insight["usage"]["input_tokens"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_http_and_generic_mcp_reads_are_side_effect_free_and_usage_is_writer_only() {
    let hub = Hub::start().await;
    let writer = hub.token(false);
    let reader = hub.token(true);
    let work = start_work(&hub, &writer).await;
    let id = work["item"]["id"].as_str().unwrap();
    let (status, _) = hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/usage",
            &writer,
            Some(usage(id, "reader-visible")),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = hub
        .request(
            reqwest::Method::GET,
            &format!("/v1/hub/standup/work/{id}/insights"),
            &reader,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, page) = hub
        .request(
            reqwest::Method::GET,
            "/v1/hub/standup/insights?limit=20",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["work_id"] == id)
    );
    let (status, body) = hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/usage",
            &reader,
            Some(usage(id, "refused")),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, tools) = hub
        .request(
            reqwest::Method::POST,
            "/mcp",
            &reader,
            Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    let names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"agentscm_work_insight"));
    assert!(names.contains(&"agentscm_work_insights"));
    assert!(!names.contains(&"agentscm_work_usage"));

    let (status, mcp) = hub
        .request(
            reqwest::Method::POST,
            "/mcp",
            &reader,
            Some(rpc("agentscm_work_insight", json!({"work_id":id}))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{mcp}");
    assert_eq!(mcp["result"]["isError"], false);
    assert_eq!(mcp["result"]["structuredContent"]["work_id"], id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_usage_and_insight_commands_use_the_http_contract() {
    let hub = Hub::start().await;
    let writer = hub.token(false);
    let work = start_work(&hub, &writer).await;
    let id = work["item"]["id"].as_str().unwrap();
    let config = TempDir::new().unwrap();
    let token_file = config.path().join("agent.token");
    std::fs::write(&token_file, format!("{writer}\n")).unwrap();
    let run = |arguments: &[&str], input: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command
            .args(arguments)
            .env_clear()
            .env("HOME", config.path())
            .env("XDG_CONFIG_HOME", config.path())
            .env("AGENTSCM_HUB_URL", &hub.base)
            .env("AGENTSCM_HUB_TOKEN_FILE", &token_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().unwrap();
        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    };
    let report = usage(id, "cli-observation").to_string();
    let output = run(&["work", "usage", "--request", "-"], Some(&report));
    assert!(output.status.success(), "{output:?}");
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["observation_id"], "cli-observation");

    let output = run(&["work", "insight", id], None);
    assert!(output.status.success(), "{output:?}");
    let insight: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(insight["work_id"], id);

    let output = run(&["work", "insights", "--limit", "1"], None);
    assert!(output.status.success(), "{output:?}");
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["items"][0]["work_id"], id);
}

#[test]
fn scoped_openapi_tracks_the_strict_usage_contract_and_all_routes() {
    let document: Value =
        serde_json::from_str(include_str!("../docs/openapi-work-insights.json")).unwrap();
    for path in [
        "/v1/hub/standup/usage",
        "/v1/hub/standup/work/{id}/insights",
        "/v1/hub/standup/insights",
    ] {
        assert!(document["paths"].get(path).is_some(), "missing {path}");
    }
    let usage = &document["components"]["schemas"]["UsageRecord"];
    assert_eq!(usage["additionalProperties"], false);
    assert_eq!(
        usage["properties"]["input_tokens"]["maximum"],
        1_000_000_000_000_i64
    );
    assert_eq!(
        usage["properties"]["input_tokens"]["type"],
        json!(["integer", "null"])
    );
    assert_eq!(
        usage["properties"]["usage_basis"]["enum"],
        json!(["reported", "provider_result"])
    );
    for field in ["observation_id", "session", "agent", "provider", "model"] {
        assert_eq!(usage["properties"][field]["maxLength"], 255, "{field}");
    }
    assert_eq!(
        usage["properties"]["source_commit"]["pattern"],
        "^(?:[0-9a-f]{40}|[0-9a-f]{64})$"
    );
    assert_eq!(
        document["components"]["schemas"]["ReportedCost"]["properties"]["basis"]["maxLength"],
        255
    );
    let after = &document["paths"]["/v1/hub/standup/insights"]["get"]["parameters"];
    assert!(
        after.as_array().unwrap().iter().any(
            |parameter| parameter["name"] == "after" && parameter["schema"]["maxLength"] == 512
        )
    );
    assert!(usage["properties"]["agent"].get("enum").is_none());
    assert!(usage["properties"]["provider"].get("enum").is_none());
    assert!(usage["properties"]["model"].get("enum").is_none());
}
