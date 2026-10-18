//! Authenticated hub and MCP reads of operator-mapped native skill workflows.

use std::path::Path;
use std::sync::Arc;

use agentscm::authority::Authority;
use agentscm::backend::LocalBackend;
use agentscm::checks::SourcePathExpectation;
use agentscm::hub::skill_workflow::SkillWorkflowRegistry;
use agentscm::hub::store::{
    self, ForgeIdentity, Principal, Store, agent_token_digest, mint_bound_agent_token, unix_now,
};
use agentscm::hub::{HubState, router};
use agentscm::model::FileChange;
use agentscm::skill_workflow::{
    SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource, advance,
    compile, start,
};
use agentscm::store::Engine;
use reqwest::Method;
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const HUB_REPOSITORY: &str = "acme/widgets";
const OTHER_REPOSITORY: &str = "acme/other";
const SKILL_PATH: &str = ".agents/skills/review/SKILL.md";
const SOURCE_PATH: &str = "src/example.rs";
const SOURCE_BYTES: &[u8] = b"fn example() {}\n";

struct Native {
    dir: TempDir,
    repository: String,
}

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    _dir: TempDir,
}

impl Native {
    fn initialized() -> Self {
        let dir = TempDir::new().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[
                    FileChange {
                        path: SKILL_PATH.into(),
                        content: Some(b"# Review\n".to_vec()),
                    },
                    FileChange {
                        path: SOURCE_PATH.into(),
                        content: Some(SOURCE_BYTES.to_vec()),
                    },
                ],
            )
            .unwrap();
        let authority = Authority::open(dir.path()).unwrap();
        let repository = authority.repository_id().unwrap();
        drop(authority);
        drop(engine);

        let definition = WorkflowDefinition {
            version: 1,
            name: "review".into(),
            source: WorkflowSource {
                repository: repository.clone(),
                revision,
                policy_epoch: 0,
            },
            skill: SkillReference {
                entrypoint: SKILL_PATH.into(),
                resources: vec![],
            },
            steps: vec![StepDefinition {
                id: "source".into(),
                needs: vec![],
                required: true,
                action: StepAction::SourcePaths {
                    expectations: vec![SourcePathExpectation {
                        path: SOURCE_PATH.into(),
                        sha256: format!("{:x}", Sha256::digest(SOURCE_BYTES)),
                    }],
                },
            }],
        };
        let plan = compile(dir.path(), &definition).unwrap();
        start(dir.path(), "run-1", &plan).unwrap();
        advance(dir.path(), "run-1", 8).unwrap();
        Self { dir, repository }
    }

    fn authority_only() -> Self {
        let dir = TempDir::new().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        let authority = Authority::open(dir.path()).unwrap();
        let repository = authority.repository_id().unwrap();
        drop(authority);
        drop(engine);
        Self { dir, repository }
    }

    fn registry(&self) -> SkillWorkflowRegistry {
        registry(self.dir.path(), &self.repository)
    }
}

impl Hub {
    async fn start(registry: SkillWorkflowRegistry) -> Self {
        let dir = TempDir::new().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        let store = store::open(&url).unwrap();
        let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
        let state = HubState::new(store.clone(), Arc::new(objects)).with_skill_workflows(registry);
        let app = router(state);
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

    fn token(&self, repository: &str, read_only: bool) -> (String, Principal) {
        let user = self
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: if repository == HUB_REPOSITORY { 1 } else { 2 },
                },
                "reader",
            )
            .unwrap();
        let (_, secret, _) = mint_bound_agent_token(
            &*self.store,
            &user.id,
            "reader",
            repository,
            1,
            600,
            read_only,
        )
        .unwrap();
        let principal = self
            .store
            .authenticate(&agent_token_digest(secret.expose()).unwrap(), unix_now())
            .unwrap()
            .unwrap();
        (secret.expose().to_owned(), principal)
    }

    async fn get(&self, path: &str, token: &str) -> (u16, Value) {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        (status, body)
    }

    async fn workflow_tool(&self, path: &str, token: &str, arguments: Value) -> Value {
        let (status, body) = self
            .rpc(
                path,
                token,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": "agentscm_workflow_status", "arguments": arguments}
                }),
            )
            .await;
        assert_eq!(status, 200);
        body
    }

    async fn rpc(&self, path: &str, token: &str, body: Value) -> (u16, Value) {
        let response = self
            .client
            .request(Method::POST, format!("{}{path}", self.base))
            .bearer_auth(token)
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&body).unwrap())
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (
            status,
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap(),
        )
    }
}

fn registry(data_dir: &Path, native_repository: &str) -> SkillWorkflowRegistry {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("bindings.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version": 1,
            "bindings": [{
                "hub_repository": HUB_REPOSITORY,
                "native_repository": native_repository,
                "data_dir": data_dir,
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    SkillWorkflowRegistry::from_file(&path).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_run_has_the_same_envelope_over_http_and_mcp_for_a_read_only_token() {
    let native = Native::initialized();
    let definition =
        agentscm::skill_workflow::runtime::retained_definition(native.dir.path(), "run-1").unwrap();
    let plan = compile(native.dir.path(), &definition).unwrap();
    for index in 2..=18 {
        start(native.dir.path(), &format!("run-{index}"), &plan).unwrap();
    }
    let hub = Hub::start(native.registry()).await;
    let (token, principal) = hub.token(HUB_REPOSITORY, true);
    let events = hub.store.events_after(0, 1000).unwrap().len();

    let (status, initialized) = hub
        .rpc(
            "/mcp",
            &token,
            json!({
                "jsonrpc": "2.0",
                "id": "init",
                "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {}}
            }),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    let instructions = initialized["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("agentscm_workflow_status"));
    assert!(instructions.contains("Historical run state"));
    assert!(instructions.contains("present applicability"));
    assert!(instructions.contains("neither is an admission decision"));
    for write_tool in ["agentscm_start", "agentscm_check", "agentscm_done"] {
        assert!(!instructions.contains(write_tool));
    }
    let (status, ping) = hub
        .rpc(
            "/mcp",
            &token,
            json!({"jsonrpc": "2.0", "id": "ping", "method": "ping"}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(ping["result"], json!({}));
    let (status, listed) = hub
        .rpc(
            "/mcp",
            &token,
            json!({"jsonrpc": "2.0", "id": "list", "method": "tools/list"}),
        )
        .await;
    assert_eq!(status, 200);
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    let workflow_tool = &tools[0];
    assert_eq!(workflow_tool["name"], "agentscm_workflow_status");
    assert_eq!(workflow_tool["annotations"]["readOnlyHint"], true);

    let (status, recent) = hub.get("/v1/hub/skill-workflows", &token).await;
    assert_eq!(status, 200);
    assert_eq!(recent["availability"], "available");
    assert_eq!(recent["runs"].as_array().unwrap().len(), 16);
    assert_eq!(recent["runs"][0]["run_id"], "run-18");
    assert_eq!(recent["runs"][0]["state"], "pending");
    assert_eq!(recent["runs"][0]["revision"], definition.source.revision);
    assert_eq!(recent["runs"][15]["run_id"], "run-3");
    assert_eq!(recent["runs"][0]["name"], "review");
    assert_eq!(recent["runs"][0]["sequence"], "18");
    assert_eq!(recent["next_cursor"], "3");
    let (status, older) = hub.get("/v1/hub/skill-workflows?before=3", &token).await;
    assert_eq!(status, 200);
    assert_eq!(older["runs"].as_array().unwrap().len(), 2);
    assert_eq!(older["runs"][0]["run_id"], "run-2");
    assert_eq!(older["runs"][1]["run_id"], "run-1");
    assert_eq!(older["next_cursor"], Value::Null);
    let (status, empty) = hub.get("/v1/hub/skill-workflows?before=1", &token).await;
    assert_eq!(status, 200);
    assert!(empty["runs"].as_array().unwrap().is_empty());
    assert_eq!(empty["next_cursor"], Value::Null);

    let (status, http) = hub.get("/v1/hub/skill-workflows/run-1", &token).await;
    assert_eq!(status, 200);
    assert_eq!(http["availability"], "available");
    assert_eq!(http["projection"]["run"]["run_id"], "run-1");
    assert_eq!(http["projection"]["run"]["state"], "succeeded");
    assert_eq!(http["projection"]["run"]["admission"], "not_assessed");
    assert_eq!(http["projection"]["present_landing_binding"], Value::Null);

    let mcp = hub
        .workflow_tool("/mcp", &token, json!({"run_id": "run-1"}))
        .await;
    assert_eq!(mcp["result"]["isError"], false);
    let mcp_envelope: Value =
        serde_json::from_str(mcp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mcp_envelope, http);
    let (status, denied) = hub
        .rpc(
            "/mcp",
            &token,
            json!({
                "jsonrpc": "2.0",
                "id": "write",
                "method": "tools/call",
                "params": {
                    "name": "agentscm_start",
                    "arguments": {"task": "must not start"}
                }
            }),
        )
        .await;
    assert_eq!(status, 403);
    assert_eq!(denied, json!({"error": "read_only"}));
    assert_eq!(hub.store.events_after(0, 1000).unwrap().len(), events);
    assert!(
        hub.store
            .live_sessions(&principal, unix_now(), 10)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_scope_and_unconfigured_hubs_never_return_another_mapping() {
    let native = Native::initialized();
    let hub = Hub::start(native.registry()).await;
    let (other, _) = hub.token(OTHER_REPOSITORY, true);
    let (status, body) = hub.get("/v1/hub/skill-workflows/run-1", &other).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({
            "version": 1,
            "availability": "unavailable",
            "reason": "native_workflow_not_configured"
        })
    );
    let other_mcp = hub
        .workflow_tool("/mcp", &other, json!({"run_id": "run-1"}))
        .await;
    let other_envelope: Value =
        serde_json::from_str(other_mcp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(other_envelope, body);
    let (status, list) = hub.get("/v1/hub/skill-workflows", &other).await;
    assert_eq!(status, 200);
    assert_eq!(list["availability"], "unavailable");
    assert_eq!(list["reason"], "native_workflow_not_configured");
    assert!(list.get("runs").is_none());

    let unconfigured = Hub::start(SkillWorkflowRegistry::unconfigured()).await;
    let (token, _) = unconfigured.token(HUB_REPOSITORY, true);
    let (status, body) = unconfigured
        .get("/v1/hub/skill-workflows/run-1", &token)
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["reason"], "native_workflow_not_configured");
    assert!(body.get("projection").is_none());
    let unconfigured_mcp = unconfigured
        .workflow_tool("/mcp", &token, json!({"run_id": "run-1"}))
        .await;
    assert_eq!(unconfigured_mcp["result"]["isError"], false);
    let mcp_envelope: Value = serde_json::from_str(
        unconfigured_mcp["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(mcp_envelope, body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_http_and_mcp_requests_are_bounded_errors() {
    let native = Native::initialized();
    let hub = Hub::start(native.registry()).await;
    let (token, _) = hub.token(HUB_REPOSITORY, true);

    for path in [
        "/v1/hub/skill-workflows?limit=1",
        "/v1/hub/skill-workflows?before=0",
        "/v1/hub/skill-workflows?before=03",
        "/v1/hub/skill-workflows?before=-1",
        "/v1/hub/skill-workflows?before=abc",
        "/v1/hub/skill-workflows?before=9223372036854775808",
        "/v1/hub/skill-workflows?before=3&before=2",
        "/v1/hub/skill-workflows?before=3&repo=acme%2Fother",
        "/v1/hub/skill-workflows/run-1?limit=1",
        "/v1/hub/skill-workflows/bad%20id",
    ] {
        let (status, body) = hub.get(path, &token).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body, json!({"error": "invalid_request"}));
    }
    for arguments in [
        json!({}),
        json!({"run_id": "run-1", "extra": true}),
        json!({"run_id": "bad id"}),
        json!({"run_id": 1}),
    ] {
        let body = hub.workflow_tool("/mcp", &token, arguments).await;
        assert_eq!(body["result"]["isError"], true);
        assert!(
            body["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("invalid arguments")
        );
    }
    let foreign = hub
        .workflow_tool("/mcp?repo=acme%2Fother", &token, json!({"run_id": "run-1"}))
        .await;
    assert_eq!(foreign["result"]["isError"], true);
    assert!(
        foreign["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("another repository")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_native_authority_suppresses_the_retained_payload() {
    let native = Native::initialized();
    let hub = Hub::start(native.registry()).await;
    let (token, _) = hub.token(HUB_REPOSITORY, true);
    Connection::open(native.dir.path().join("state.sqlite"))
        .unwrap()
        .execute(
            "UPDATE authority_state SET repository_id=?1 WHERE singleton=1",
            ["0".repeat(32)],
        )
        .unwrap();

    let (status, body) = hub.get("/v1/hub/skill-workflows/run-1", &token).await;
    assert_eq!(status, 200);
    assert_eq!(body["availability"], "unavailable");
    assert_eq!(body["reason"], "native_workflow_binding_replaced");
    assert!(body.get("projection").is_none());
    let (status, list) = hub.get("/v1/hub/skill-workflows", &token).await;
    assert_eq!(status, 200);
    assert_eq!(list["availability"], "unavailable");
    assert_eq!(list["reason"], "native_workflow_binding_replaced");
    assert!(list.get("runs").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_journals_and_missing_configured_stores_are_never_initialized() {
    let native = Native::authority_only();
    assert!(!native.dir.path().join("skill-workflows").exists());
    let hub = Hub::start(native.registry()).await;
    let (token, _) = hub.token(HUB_REPOSITORY, true);
    let (status, list) = hub.get("/v1/hub/skill-workflows", &token).await;
    assert_eq!(status, 200);
    assert_eq!(list["availability"], "available");
    assert_eq!(list["runs"], json!([]));
    assert!(!native.dir.path().join("skill-workflows").exists());
    let (status, body) = hub.get("/v1/hub/skill-workflows/missing", &token).await;
    assert_eq!(status, 404, "{body}");
    assert!(!native.dir.path().join("skill-workflows").exists());

    let config_dir = TempDir::new().unwrap();
    let missing = config_dir.path().join("missing-native");
    let config = config_dir.path().join("bindings.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "version": 1,
            "bindings": [{
                "hub_repository": HUB_REPOSITORY,
                "native_repository": "0".repeat(32),
                "data_dir": missing,
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(SkillWorkflowRegistry::from_file(&config).is_err());
    assert!(!missing.exists());
}

#[test]
fn binding_configuration_is_strict_bounded_and_unique() {
    let native = Native::authority_only();
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("bindings.json");
    let write = |value: Value| std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    write(json!({"version": 1, "bindings": [], "unknown": true}));
    assert!(SkillWorkflowRegistry::from_file(&path).is_err());
    write(json!({
        "version": 1,
        "bindings": [
            {"hub_repository": HUB_REPOSITORY, "native_repository": native.repository, "data_dir": native.dir.path()},
            {"hub_repository": HUB_REPOSITORY, "native_repository": native.repository, "data_dir": native.dir.path()}
        ]
    }));
    assert!(SkillWorkflowRegistry::from_file(&path).is_err());
    write(json!({
        "version": 1,
        "bindings": [{
            "hub_repository": HUB_REPOSITORY,
            "native_repository": "A".repeat(32),
            "data_dir": native.dir.path()
        }]
    }));
    assert!(SkillWorkflowRegistry::from_file(&path).is_err());
    std::fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
    assert!(SkillWorkflowRegistry::from_file(&path).is_err());
}
