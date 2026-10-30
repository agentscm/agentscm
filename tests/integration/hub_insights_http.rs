use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::insights::{MAX_OBSERVATIONS, UsageRecord};
use agentscm::hub::store::{ForgeIdentity, Store, mint_bound_agent_token, unix_now};
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
        self.token_with_id(read_only).1
    }

    fn token_with_id(&self, read_only: bool) -> (String, String) {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "forge.example".into(),
            user_id: if read_only { 2 } else { 1 },
        };
        let user = self
            .store
            .upsert_user(&identity, if read_only { "reader" } else { "writer" })
            .unwrap();
        let (id, secret, _) = mint_bound_agent_token(
            &*self.store,
            &user.id,
            if read_only { "reader" } else { "writer" },
            REPOSITORY,
            77,
            600,
            read_only,
        )
        .unwrap();
        (id, secret.expose().to_owned())
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.request_with_completion(method, path, token, body, false)
            .await
    }

    async fn request_with_completion(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Option<Value>,
        completion_reporting: bool,
    ) -> (StatusCode, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token);
        if completion_reporting {
            request = request.header("x-agentscm-completion-reporting", "true");
        }
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

async fn insight(hub: &Hub, token: &str, work_id: &str) -> Value {
    let (status, insight) = hub
        .request(
            reqwest::Method::GET,
            &format!("/v1/hub/standup/work/{work_id}/insights"),
            token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{insight}");
    insight
}

fn finish(work_id: &str, operation_id: &str, expected_updated_seq: i64) -> Value {
    json!({
        "work_id":work_id,"operation_id":operation_id,"generation":1,
        "expected_updated_seq":expected_updated_seq,"session":"surface-session","kind":"finish",
        "summary":"Completed the synthetic work","next_action":null,"scope":null,
        "branch":null,"worktree":null,"resolve_blocker":false,"source_refs":[],
        "depends_on":[],"related_work":[]
    })
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

fn assert_unknown_completion(insight: &Value) {
    let usage = &insight["usage"];
    assert_eq!(usage["observations"], 1);
    assert_eq!(usage["coverage"], "partial");
    assert_eq!(usage["input_tokens"], Value::Null);
    assert_eq!(usage["output_tokens"], Value::Null);
    assert_eq!(usage["input_known_observations"], 0);
    assert_eq!(usage["output_known_observations"], 0);
    assert_eq!(usage["missing_cost_observations"], 1);
    assert_eq!(usage["reported_failed_attempts"], 0);
    assert!(usage["cost_groups"].as_array().unwrap().is_empty());
    assert_eq!(usage["token_groups"].as_array().unwrap().len(), 1);
    let group = &usage["token_groups"][0];
    assert_eq!(group["agent"], "writer");
    assert_eq!(group["provider"], "unknown");
    assert_eq!(group["model"], "unknown");
    assert_eq!(group["usage_basis"], "reported");
    assert_eq!(group["observations"], 1);
    assert_eq!(group["input_tokens"], Value::Null);
    assert_eq!(group["output_tokens"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_reporting_is_off_by_default_for_done_and_finish() {
    let done_hub = Hub::start().await;
    let done_writer = done_hub.token(false);
    let done_work = start_work(&done_hub, &done_writer).await;
    let done_id = done_work["item"]["id"].as_str().unwrap();

    let (status, native) = done_hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/hook/claude",
            &done_writer,
            Some(json!({
                "hook_event_name":"SessionEnd",
                "session_id":"surface-session",
                "cwd":"/tmp"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{native}");
    let (status, mcp) = done_hub
        .request(
            reqwest::Method::POST,
            "/mcp",
            &done_writer,
            Some(rpc("agentscm_done", json!({"session":"surface-session"}))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{mcp}");
    assert_eq!(
        insight(&done_hub, &done_writer, done_id).await["usage"]["observations"],
        0
    );

    let finish_hub = Hub::start().await;
    let finish_writer = finish_hub.token(false);
    let finish_work = start_work(&finish_hub, &finish_writer).await;
    let finish_id = finish_work["item"]["id"].as_str().unwrap();
    let checkpoint = finish(
        finish_id,
        "default-off-finish",
        finish_work["item"]["updated_seq"].as_i64().unwrap(),
    );
    let (status, http) = finish_hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/checkpoint",
            &finish_writer,
            Some(checkpoint.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{http}");
    let (status, mcp) = finish_hub
        .request(
            reqwest::Method::POST,
            "/mcp",
            &finish_writer,
            Some(rpc("agentscm_work_checkpoint", checkpoint)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{mcp}");
    assert_eq!(
        insight(&finish_hub, &finish_writer, finish_id).await["usage"]["observations"],
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_and_mcp_done_share_one_opted_in_completion_observation() {
    for mcp_first in [false, true] {
        let hub = Hub::start().await;
        let writer = hub.token(false);
        let work = start_work(&hub, &writer).await;
        let id = work["item"]["id"].as_str().unwrap();
        let native = || {
            hub.request_with_completion(
                reqwest::Method::POST,
                "/v1/hub/hook/claude",
                &writer,
                Some(json!({
                    "hook_event_name":"SessionEnd",
                    "session_id":"surface-session",
                    "cwd":"/tmp"
                })),
                true,
            )
        };
        let mcp = || {
            hub.request_with_completion(
                reqwest::Method::POST,
                "/mcp",
                &writer,
                Some(rpc("agentscm_done", json!({"session":"surface-session"}))),
                true,
            )
        };

        if mcp_first {
            let (status, first) = mcp().await;
            assert_eq!(status, StatusCode::OK, "{first}");
            assert_unknown_completion(&insight(&hub, &writer, id).await);
            let (status, second) = native().await;
            assert_eq!(status, StatusCode::OK, "{second}");
        } else {
            let (status, first) = native().await;
            assert_eq!(status, StatusCode::OK, "{first}");
            assert_unknown_completion(&insight(&hub, &writer, id).await);
            let (status, second) = mcp().await;
            assert_eq!(status, StatusCode::OK, "{second}");
        }
        assert_unknown_completion(&insight(&hub, &writer, id).await);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_and_mcp_finish_share_one_opted_in_completion_observation() {
    for mcp_first in [false, true] {
        let hub = Hub::start().await;
        let writer = hub.token(false);
        let work = start_work(&hub, &writer).await;
        let id = work["item"]["id"].as_str().unwrap();
        let checkpoint = finish(
            id,
            "opted-in-finish",
            work["item"]["updated_seq"].as_i64().unwrap(),
        );
        let http = || {
            hub.request_with_completion(
                reqwest::Method::POST,
                "/v1/hub/standup/checkpoint",
                &writer,
                Some(checkpoint.clone()),
                true,
            )
        };
        let mcp = || {
            hub.request_with_completion(
                reqwest::Method::POST,
                "/mcp",
                &writer,
                Some(rpc("agentscm_work_checkpoint", checkpoint.clone())),
                true,
            )
        };

        if mcp_first {
            let (status, first) = mcp().await;
            assert_eq!(status, StatusCode::OK, "{first}");
            assert_unknown_completion(&insight(&hub, &writer, id).await);
            let (status, second) = http().await;
            assert_eq!(status, StatusCode::OK, "{second}");
        } else {
            let (status, first) = http().await;
            assert_eq!(status, StatusCode::OK, "{first}");
            assert_unknown_completion(&insight(&hub, &writer, id).await);
            let (status, second) = mcp().await;
            assert_eq!(status, StatusCode::OK, "{second}");
        }
        assert_unknown_completion(&insight(&hub, &writer, id).await);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_finished_checkpoint_replays_do_not_report_completion() {
    let hub = Hub::start().await;
    let writer = hub.token(false);
    let work = start_work(&hub, &writer).await;
    let id = work["item"]["id"].as_str().unwrap();
    let stale_finish = finish(
        id,
        "generation-one-finish",
        work["item"]["updated_seq"].as_i64().unwrap(),
    );
    let (status, finished) = hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/checkpoint",
            &writer,
            Some(stale_finish.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{finished}");

    let handover = json!({
        "work_id":id,"operation_id":"handover-after-finish","generation":1,
        "expected_updated_seq":finished["item"]["updated_seq"],
        "session":"surface-session","kind":"handover",
        "summary":"Reopen this work under a new generation","next_action":null,
        "scope":null,"branch":null,"worktree":null,"resolve_blocker":false,
        "source_refs":[],"depends_on":[],"related_work":[]
    });
    let (status, handed_over) = hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/checkpoint",
            &writer,
            Some(handover),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{handed_over}");
    let takeover = json!({
        "work_id":id,"source":null,"request_key":null,
        "title":"verify insight contracts","session":"surface-session","role":"lead",
        "scope":["src/synthetic.rs"],"branch":null,"worktree":null,"next_action":null,
        "depends_on":[],"related_work":[],"takeover_generation":1
    });
    let (status, reopened) = hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/start",
            &writer,
            Some(takeover),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reopened}");
    assert_eq!(reopened["item"]["generation"], 2);
    assert_eq!(reopened["item"]["state"], "active");

    let mut generation_two_finish = finish(
        id,
        "generation-two-finish",
        reopened["item"]["updated_seq"].as_i64().unwrap(),
    );
    generation_two_finish["generation"] = json!(2);
    let (status, current_finish) = hub
        .request(
            reqwest::Method::POST,
            "/v1/hub/standup/checkpoint",
            &writer,
            Some(generation_two_finish),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{current_finish}");
    assert_eq!(current_finish["item"]["generation"], 2);
    assert_eq!(current_finish["item"]["state"], "finished");
    assert_eq!(insight(&hub, &writer, id).await["usage"]["observations"], 0);

    let (status, http_replay) = hub
        .request_with_completion(
            reqwest::Method::POST,
            "/v1/hub/standup/checkpoint",
            &writer,
            Some(stale_finish.clone()),
            true,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{http_replay}");
    assert_eq!(http_replay["receipt"]["replayed"], true);
    assert_eq!(insight(&hub, &writer, id).await["usage"]["observations"], 0);

    let (status, mcp_replay) = hub
        .request_with_completion(
            reqwest::Method::POST,
            "/mcp",
            &writer,
            Some(rpc("agentscm_work_checkpoint", stale_finish)),
            true,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{mcp_replay}");
    assert_eq!(mcp_replay["result"]["isError"], false);
    assert_eq!(
        mcp_replay["result"]["structuredContent"]["receipt"]["replayed"],
        true
    );
    assert_eq!(insight(&hub, &writer, id).await["usage"]["observations"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_reporting_failure_does_not_block_finish() {
    let hub = Hub::start().await;
    let (token_id, writer) = hub.token_with_id(false);
    let work = start_work(&hub, &writer).await;
    let id = work["item"]["id"].as_str().unwrap();

    for index in 0..MAX_OBSERVATIONS {
        let report: UsageRecord =
            serde_json::from_value(usage(id, &format!("capacity-{index}"))).unwrap();
        hub.store
            .work_usage(&token_id, &report, unix_now())
            .unwrap();
    }

    let (status, done) = hub
        .request_with_completion(
            reqwest::Method::POST,
            "/mcp",
            &writer,
            Some(rpc("agentscm_done", json!({"session":"surface-session"}))),
            true,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["result"]["isError"], false);
    assert_eq!(done["result"]["content"].as_array().unwrap().len(), 1);
    assert_eq!(
        insight(&hub, &writer, id).await["usage"]["observations"].as_u64(),
        Some(MAX_OBSERVATIONS as u64)
    );

    let (status, native) = hub
        .request_with_completion(
            reqwest::Method::POST,
            "/v1/hub/hook/claude",
            &writer,
            Some(json!({
                "hook_event_name":"SessionEnd",
                "session_id":"surface-session",
                "cwd":"/tmp"
            })),
            true,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{native}");
    assert_eq!(
        insight(&hub, &writer, id).await["usage"]["observations"].as_u64(),
        Some(MAX_OBSERVATIONS as u64)
    );

    let (status, current) = hub
        .request(
            reqwest::Method::GET,
            &format!("/v1/hub/standup/work/{id}"),
            &writer,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{current}");

    let checkpoint = finish(
        id,
        "finish-at-usage-capacity",
        current["updated_seq"].as_i64().unwrap(),
    );
    let (status, response) = hub
        .request_with_completion(
            reqwest::Method::POST,
            "/v1/hub/standup/checkpoint",
            &writer,
            Some(checkpoint),
            true,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["item"]["state"], "finished");
    assert_eq!(response["receipt"]["replayed"], false);
    assert_eq!(
        insight(&hub, &writer, id).await["usage"]["observations"].as_u64(),
        Some(MAX_OBSERVATIONS as u64)
    );
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
        serde_json::from_str(include_str!("../../docs/openapi-work-insights.json")).unwrap();
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
