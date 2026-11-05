//! Default agent-facing MCP replies stay far below the size agents' hosts save to a file
//! instead of showing (about 25,000 tokens in Claude Code), on a busy synthetic repository.
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::work::{CheckpointKind, WorkCheckpoint, WorkRole, WorkStart};
use agentscm::hub::{HubState, router};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
/// Characters of a whole `tools/call` result: content, structured content and metadata.
const BUDGET: usize = 20_000;

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    _dir: TempDir,
}

fn words(seed: usize, chars: usize) -> String {
    let mut text = format!("Synthetic {seed}:");
    while text.len() < chars {
        text.push_str(" refine the bounded request validation path");
    }
    text.truncate(chars);
    text
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

    fn token(&self, user_id: i64, name: &str) -> (String, String) {
        let owner = self
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id,
                },
                &format!("person{user_id}"),
            )
            .unwrap();
        let (id, secret) = mint_agent_token(&*self.store, &owner.id, name, REPO, 3600).unwrap();
        (id, secret.expose().to_owned())
    }

    /// The whole `result` of one tool call, as JSON text.
    async fn result(&self, secret: &str, name: &str, arguments: Value) -> String {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        let reply = self
            .client
            .post(format!("{}/mcp", self.base))
            .bearer_auth(secret)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
        let reply: Value = serde_json::from_str(&reply.text().await.unwrap()).unwrap();
        reply["result"].to_string()
    }
}

/// Three owners, 36 unfinished and 40 recently finished Work items with long titles, scopes,
/// next actions and checkpoints, and coordination requests between owners.
fn busy(hub: &Hub) -> Vec<(String, String)> {
    let tokens: Vec<(String, String)> = (0..6)
        .map(|n| hub.token(1 + n / 2, &format!("client-{:016x}-{:08x}", n + 1, n + 7)))
        .collect();
    let now = unix_now();
    for n in 0..76 {
        let token = &tokens[n % tokens.len()].0;
        let session = format!("fixture-session-{n:03}-0123456789abcdef0123456789");
        let start = WorkStart {
            work_id: None,
            source: None,
            request_key: Some(format!("fixture-{n}")),
            title: words(n, 180),
            session: session.clone(),
            role: WorkRole::Lead,
            scope: (0..6).map(|p| format!("src/area{n:03}/module{p}.rs")).collect(),
            branch: Some(format!("feat/fixture-{n:03}-bounded-validation")),
            worktree: Some(format!("fixture-{n:03}")),
            next_action: Some(words(n + 1000, 250)),
            depends_on: Vec::new(),
            related_work: Vec::new(),
            takeover_generation: None,
            preflight: None,
        };
        let item = hub.store.work_start(token, &start, now).unwrap().item;
        let mut seq = item.updated_seq;
        let kinds: &[CheckpointKind] = if n < 36 {
            &[CheckpointKind::Decision, CheckpointKind::Progress]
        } else {
            &[CheckpointKind::Decision, CheckpointKind::Finish]
        };
        for (k, kind) in kinds.iter().enumerate() {
            let result = hub
                .store
                .work_checkpoint(
                    token,
                    &WorkCheckpoint {
                        work_id: item.id.clone(),
                        operation_id: format!("fixture-{n}-{k}"),
                        generation: item.generation,
                        session: session.clone(),
                        expected_updated_seq: Some(seq),
                        kind: *kind,
                        summary: words(n * 10 + k, 450),
                        next_action: Some(words(n * 10 + k + 1, 250)),
                        scope: None,
                        branch: None,
                        worktree: None,
                        resolve_blocker: false,
                        source_refs: Vec::new(),
                        depends_on: Vec::new(),
                        related_work: Vec::new(),
                    },
                    now + 1,
                )
                .unwrap();
            seq = result.item.updated_seq;
        }
    }
    // Overlapping starts by other owners queue coordination requests.
    for n in 0..6 {
        let token = &tokens[(n + 2) % tokens.len()].0;
        let start = WorkStart {
            work_id: None,
            source: None,
            request_key: Some(format!("fixture-overlap-{n}")),
            title: words(n + 500, 180),
            session: format!("fixture-overlap-{n}"),
            role: WorkRole::Lead,
            scope: vec![format!("src/area{n:03}/module0.rs")],
            branch: None,
            worktree: None,
            next_action: None,
            depends_on: Vec::new(),
            related_work: Vec::new(),
            takeover_generation: None,
            preflight: None,
        };
        hub.store.work_start(token, &start, now + 2).unwrap();
    }
    tokens
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_start_and_overview_replies_fit_the_budget_on_a_busy_repository() {
    let hub = Hub::start().await;
    let tokens = busy(&hub);
    let secret = &tokens[0].1;
    let start = hub
        .result(secret, "agentscm_start", json!({"task": "Measure reply sizes",
            "session": "budget-session", "request_key": "budget", "paths": ["src/budget.rs"]}))
        .await;
    let overview = hub.result(secret, "agentscm_work_overview", json!({})).await;
    let overview_10 = hub.result(secret, "agentscm_work_overview", json!({"limit": 10})).await;
    let overview_20 = hub.result(secret, "agentscm_work_overview", json!({"limit": 20})).await;
    let started: Value = serde_json::from_str(&start).unwrap();
    let work_id = started["structuredContent"]["work"]["item"]["id"].clone();
    let checkpoint = hub
        .result(secret, "agentscm_work_checkpoint", json!({"work_id": work_id, "operation_id": "budget-1",
            "generation": 1, "expected_updated_seq": started["structuredContent"]["work"]["item"]["updated_seq"],
            "session": "budget-session", "kind": "progress", "summary": words(1, 450)}))
        .await;
    let sizes = [
        ("agentscm_start", start.len()),
        ("agentscm_work_overview", overview.len()),
        ("agentscm_work_overview limit 10", overview_10.len()),
        ("agentscm_work_overview limit 20", overview_20.len()),
        ("agentscm_work_checkpoint", checkpoint.len()),
    ];
    let structured: Value = serde_json::from_str(&start).unwrap();
    for (part, value) in structured["structuredContent"].as_object().unwrap() {
        eprintln!("start part {part}: {}", value.to_string().len());
    }
    for (name, size) in sizes {
        eprintln!("reply size {name}: {size}");
    }
    for (name, size) in sizes {
        assert!(size < BUDGET, "{name} is {size} characters");
    }
}
