//! Supported skills on the hub (Experimental): library, adoption, evidence and runs over HTTP and MCP.
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::skills::SkillAdoption;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token, unix_now};
use agentscm::hub::{HubState, router};
use agentscm::skill_workflow::supported::{SkillLock, SkillPin, catalog_entry};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPO_PATH: &str = "/v1/hub/repos/acme%2Fwidgets/skills";
const H1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const H2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    writer: String,
    /// A second writer: a different token for the same repository.
    other_writer: String,
    reader: String,
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
        let owner = store
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
            mint_bound_agent_token(&*store, &owner.id, name, REPO, 1, 600, read_only)
                .unwrap()
                .1
                .expose()
                .to_owned()
        };
        let (writer, reader) = (mint("writer", false), mint("reader", true));
        let other_writer = mint("other-writer", false);
        Self {
            base,
            store,
            client: reqwest::Client::new(),
            writer,
            other_writer,
            reader,
            _directory: directory,
        }
    }

    async fn send(
        &self,
        method: Method,
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
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (
            status,
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap_or(Value::Null),
        )
    }

    fn adopt(&self) {
        let entry = catalog_entry("pr-gate", 1).unwrap();
        let lock = SkillLock {
            version: 1,
            skills: vec![SkillPin::first_party(entry, None)],
        };
        self.store
            .skill_adoption_put(&SkillAdoption {
                repository: REPO.into(),
                source: "default_branch".into(),
                lock_json: serde_json::to_string(&lock).unwrap(),
                default_branch: Some("main".into()),
                observed_at: unix_now(),
                reported_by: None,
            })
            .unwrap();
    }

    async fn record(
        &self,
        session: &str,
        gate: &str,
        kind: &str,
        head: &str,
    ) -> (StatusCode, Value) {
        self.record_with(&self.writer, session, gate, kind, head)
            .await
    }

    async fn record_with(
        &self,
        token: &str,
        session: &str,
        gate: &str,
        kind: &str,
        head: &str,
    ) -> (StatusCode, Value) {
        let body = json!({"skill": "pr-gate", "gate": gate, "kind": kind, "head_sha": head,
            "assurance": "self_reported", "summary": "done", "branch": "feat/x", "session": session});
        self.send(
            Method::POST,
            &format!("{REPO_PATH}/evidence"),
            token,
            Some(body),
        )
        .await
    }

    async fn tool(&self, name: &str, arguments: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        let (_, reply) = self
            .send(Method::POST, "/mcp", &self.writer, Some(body))
            .await;
        reply["result"].clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn library_shows_the_catalog_with_and_without_adoption_and_flags_an_unknown_version() {
    let hub = Hub::start().await;
    let (status, bare) = hub
        .send(Method::GET, "/v1/hub/skills", &hub.reader, None)
        .await;
    assert_eq!(status, 200, "{bare}");
    assert_eq!(bare["catalog"][0]["name"], "pr-gate");
    assert_eq!(bare["catalog"][0]["gates"][0]["id"], "branch");
    assert_eq!(bare["catalog"][0]["gates"][1]["enforce"], json!(["stop"]));
    let steps = bare["catalog"][0]["walkthrough"].as_array().unwrap();
    assert_eq!(steps.len(), 8);
    assert!(
        steps[1]
            .as_str()
            .unwrap()
            .starts_with("2. Tests: The author runs the tests on the exact head commit."),
        "{steps:?}"
    );
    let procedure = bare["catalog"][0]["procedure"].as_str().unwrap();
    assert!(procedure.starts_with("---\nname: pr-gate"), "{procedure}");
    assert_eq!(bare["adoption"]["source"], "none");
    let (_, none) = hub
        .send(
            Method::GET,
            "/v1/hub/skills?repo=acme%2Fwidgets",
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(none["adoption"]["pins"], json!([]));
    let (status, _) = hub
        .send(
            Method::GET,
            "/v1/hub/skills?repo=other%2Frepo",
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(status, 401);

    // A checkout reports a pin of a version this hub does not know.
    let entry = catalog_entry("pr-gate", 1).unwrap();
    let mut pin = SkillPin::first_party(entry, None);
    pin.sha256 = "0".repeat(64);
    let lock = serde_json::to_value(SkillLock {
        version: 1,
        skills: vec![pin],
    })
    .unwrap();
    let (status, put) = hub
        .send(
            Method::PUT,
            &format!("{REPO_PATH}/adoption"),
            &hub.writer,
            Some(lock),
        )
        .await;
    assert_eq!(
        (status, put["source"].clone(), put["applied"].clone()),
        (StatusCode::OK, json!("reported_by_checkout"), json!(true)),
        "{put}"
    );
    let (_, lib) = hub
        .send(
            Method::GET,
            "/v1/hub/skills?repo=acme%2Fwidgets",
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(lib["adoption"]["source"], "reported_by_checkout");
    assert_eq!(lib["adoption"]["pins"][0]["status"], "unknown_version");
    let (status, refused) = hub.record("s1", "branch", "task_branch", H1).await;
    assert_eq!(
        (status, refused["error"].clone()),
        (StatusCode::CONFLICT, json!("not_adopted")),
        "{refused}"
    );

    // Reading may be done by a read token; reporting may not.
    let (status, _) = hub
        .send(
            Method::PUT,
            &format!("{REPO_PATH}/adoption"),
            &hub.reader,
            Some(json!({})),
        )
        .await;
    assert_eq!(status, 403);
    let (status, _) = hub
        .send(
            Method::PUT,
            &format!("{REPO_PATH}/adoption"),
            &hub.writer,
            Some(json!({"version": 2, "skills": []})),
        )
        .await;
    assert_eq!(status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_runs_through_the_gates_to_ci_and_every_assurance_is_labelled() {
    let hub = Hub::start().await;
    hub.adopt();
    let (status, view) = hub.record("author", "branch", "task_branch", H1).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["current_gate"], "tests");
    assert_eq!(view["gates"][0]["assurance"], "self_reported");
    assert_eq!(view["params"]["merge_method"], "any");
    for (gate, kind, session, next) in [
        ("tests", "revision_tests", "author", "review"),
        ("review", "independent_review", "reviewer", "ci"),
        ("ci", "hosted_ci", "author", "approval"),
    ] {
        let (status, view) = hub.record(session, gate, kind, H1).await;
        assert_eq!(status, 200, "{view}");
        assert_eq!(view["current_gate"], next, "{view}");
    }
    let (_, run) = hub
        .send(
            Method::GET,
            &format!("{REPO_PATH}/runs/by-branch?branch=feat%2Fx"),
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(run["state"], "open");
    assert_eq!(run["blocks_finish"], false);
    let id = run["run_id"].as_str().unwrap();
    let (status, again) = hub
        .send(
            Method::GET,
            &format!("{REPO_PATH}/runs/{id}"),
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(
        (status, again["run_id"].clone()),
        (StatusCode::OK, json!(id))
    );

    let (_, list) = hub
        .send(
            Method::GET,
            &format!("{REPO_PATH}/runs?state=all&limit=50"),
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(list["runs"].as_array().unwrap().len(), 1);
    assert_eq!(list["truncated"], false);
    let (_, completed) = hub
        .send(
            Method::GET,
            &format!("{REPO_PATH}/runs?state=completed"),
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(completed["runs"], json!([]));
    let (_, lib) = hub
        .send(
            Method::GET,
            "/v1/hub/skills?repo=acme%2Fwidgets",
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(
        lib["counts"]["pr-gate"],
        json!({"open": 1, "at_stop_gate": 0, "completed": 0})
    );

    let (status, _) = hub
        .send(
            Method::GET,
            &format!("{REPO_PATH}/runs/by-branch?branch=none"),
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = hub
        .send(
            Method::GET,
            &format!("{REPO_PATH}/runs/run_missing"),
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = hub
        .send(
            Method::GET,
            &format!("{REPO_PATH}/runs?state=bogus"),
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_are_explicit_and_readers_cannot_record() {
    let hub = Hub::start().await;
    hub.adopt();
    assert_eq!(
        hub.record("author", "branch", "task_branch", H1).await.0,
        200
    );
    assert_eq!(
        hub.record("author", "tests", "revision_tests", H1).await.0,
        200
    );

    // The author's own session cannot review.
    let (status, own) = hub
        .record("author", "review", "independent_review", H1)
        .await;
    assert_eq!(
        (status, own["error"].clone()),
        (StatusCode::CONFLICT, json!("not_independent")),
        "{own}"
    );
    // A review of another head than the run's is stale.
    let (status, stale) = hub
        .record("reviewer", "review", "independent_review", H2)
        .await;
    assert_eq!(
        (status, stale["error"].clone()),
        (StatusCode::CONFLICT, json!("stale_head")),
        "{stale}"
    );
    assert!(stale["message"].as_str().unwrap().contains(H1));
    // A kind the gate does not take.
    let (status, unknown) = hub.record("author", "tests", "hosted_ci", H1).await;
    assert_eq!(
        (status, unknown["error"].clone()),
        (StatusCode::CONFLICT, json!("unknown_gate")),
        "{unknown}"
    );

    // A new author head makes the tests evidence stale rather than lost.
    let (_, moved) = hub.record("author", "tests", "revision_tests", H2).await;
    assert_eq!(moved["head_sha"], H2);
    assert_eq!(moved["stale_evidence"].as_array().unwrap().len(), 1);

    let body = json!({"skill": "pr-gate", "gate": "branch", "kind": "task_branch", "head_sha": H1,
        "assurance": "self_reported", "summary": "x", "branch": "feat/y"});
    let (status, denied) = hub
        .send(
            Method::POST,
            &format!("{REPO_PATH}/evidence"),
            &hub.reader,
            Some(body),
        )
        .await;
    assert_eq!(
        (status, denied["error"].clone()),
        (StatusCode::FORBIDDEN, json!("read_only"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_tools_list_status_and_record_with_labels_and_named_errors() {
    let hub = Hub::start().await;
    let listed = hub.tool("agentscm_skill_list", json!({"repo": REPO})).await;
    let text = listed["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("pr-gate v1 (available)"), "{text}");
    assert!(
        text.contains("Follow when:")
            && text.contains("Prerequisites:")
            && text.contains("Not for:")
            && text.contains("How it works:")
            && text.contains("2. Tests: The author runs the tests on the exact head commit."),
        "{text}"
    );
    hub.adopt();
    let listed = hub.tool("agentscm_skill_list", json!({"repo": REPO})).await;
    let text = listed["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("pr-gate v1 (adopted)") && text.contains("merge_method"),
        "{text}"
    );
    assert!(
        text.contains("Procedure:") && text.contains("same agent token and session"),
        "{text}"
    );
    assert_eq!(
        listed["structuredContent"]["adoption"]["source"],
        "default_branch"
    );
    assert_eq!(
        listed["structuredContent"]["adoption"]["pins"][0]["walkthrough"]
            .as_array()
            .map(Vec::len),
        Some(8)
    );

    let recorded = hub
        .tool("agentscm_skill_evidence", json!({"repo": REPO, "skill": "pr-gate", "gate": "branch",
            "kind": "task_branch", "head_sha": H1, "summary": "branch made", "branch": "feat/x", "session": "author"}))
        .await;
    assert_eq!(recorded["isError"], false, "{recorded}");
    let text = recorded["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("- branch: done [self-reported]"), "{text}");
    assert!(text.contains("Next: Run the tests"), "{text}");
    assert!(text.contains("Finishing is blocked"), "{text}");

    let status = hub
        .tool(
            "agentscm_skill_status",
            json!({"repo": REPO, "branch": "feat/x"}),
        )
        .await;
    assert!(
        status["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[self-reported]")
    );
    let empty = hub
        .tool(
            "agentscm_skill_status",
            json!({"repo": REPO, "branch": "nothing"}),
        )
        .await;
    assert_eq!(empty["isError"], false);

    let missing = hub
        .tool(
            "agentscm_skill_evidence",
            json!({"repo": REPO, "skill": "pr-gate"}),
        )
        .await;
    assert_eq!(missing["isError"], true);
    let text = missing["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("missing field `gate`"), "{text}");
    let refused = hub
        .tool("agentscm_skill_evidence", json!({"repo": REPO, "skill": "pr-gate", "gate": "review",
            "kind": "independent_review", "head_sha": H1, "summary": "ok", "branch": "feat/x", "session": "author"}))
        .await;
    assert_eq!(refused["isError"], true);
    assert!(
        refused["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not_independent")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_for_an_older_head_is_refused_and_cannot_move_the_head_back() {
    let hub = Hub::start().await;
    hub.adopt();
    hub.record("author", "branch", "task_branch", H1).await;
    hub.record("author", "tests", "revision_tests", H1).await;
    let (_, moved) = hub.record("author", "tests", "revision_tests", H2).await;
    assert_eq!(moved["head_sha"], H2);

    for (session, gate, kind) in [
        ("author", "tests", "revision_tests"),
        ("reviewer", "review", "independent_review"),
    ] {
        let (status, refused) = hub.record(session, gate, kind, H1).await;
        assert_eq!(
            (status, refused["error"].clone()),
            (StatusCode::CONFLICT, json!("stale_head")),
            "{refused}"
        );
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("older than the run head"),
            "{refused}"
        );
    }
    let (_, run) = hub
        .send(
            Method::GET,
            &format!("{REPO_PATH}/runs/by-branch?branch=feat%2Fx"),
            &hub.reader,
            None,
        )
        .await;
    assert_eq!(run["head_sha"], H2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_independence_needs_a_different_token_or_session_than_the_author() {
    let hub = Hub::start().await;
    hub.adopt();
    hub.record("author", "branch", "task_branch", H1).await;
    hub.record("author", "tests", "revision_tests", H1).await;
    let (status, same) = hub
        .record("author", "review", "independent_review", H1)
        .await;
    assert_eq!(
        (status, same["error"].clone()),
        (StatusCode::CONFLICT, json!("not_independent")),
        "{same}"
    );
    // Another token that picked the same session name is a different recorder.
    let (status, other_token) = hub
        .record_with(
            &hub.other_writer,
            "author",
            "review",
            "independent_review",
            H1,
        )
        .await;
    assert_eq!(status, 200, "{other_token}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_token_with_another_session_may_review_but_is_self_reported() {
    let hub = Hub::start().await;
    hub.adopt();
    hub.record("author", "branch", "task_branch", H1).await;
    hub.record("author", "tests", "revision_tests", H1).await;
    let (status, view) = hub
        .record("reviewer", "review", "independent_review", H1)
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["gates"][2]["assurance"], "self_reported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forge_connected_repository_refuses_checkout_adoption_reports() {
    use agentscm::hub::connection::NewConnection;
    let hub = Hub::start().await;
    let lock = serde_json::to_value(SkillLock {
        version: 1,
        skills: vec![SkillPin::first_party(
            catalog_entry("pr-gate", 1).unwrap(),
            None,
        )],
    })
    .unwrap();
    let adoption = format!("{REPO_PATH}/adoption");

    // Without a forge connection the checkout's report is the adoption.
    let (status, put) = hub
        .send(Method::PUT, &adoption, &hub.writer, Some(lock.clone()))
        .await;
    assert_eq!(
        (status, put["applied"].clone()),
        (StatusCode::OK, json!(true)),
        "{put}"
    );
    let before = hub.store.skill_adoption(REPO).unwrap();

    let connection = hub
        .store
        .insert_connection(
            &NewConnection {
                id: store::new_connection_id().unwrap(),
                provider: "github".into(),
                host: "github.com".into(),
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: Some("bot".into()),
            },
            "test",
        )
        .unwrap();
    hub.store
        .upsert_repository(&connection.id, "github.com", 1, REPO, None, None)
        .unwrap();

    let (status, refused) = hub
        .send(
            Method::PUT,
            &adoption,
            &hub.writer,
            Some(json!({"version": 1, "skills": []})),
        )
        .await;
    assert_eq!(
        (status, refused["error"].clone()),
        (StatusCode::CONFLICT, json!("forge_authoritative")),
        "{refused}"
    );
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("agentscm-skills.lock.json on the default branch"),
        "{refused}"
    );
    assert_eq!(hub.store.skill_adoption(REPO).unwrap(), before);
}
