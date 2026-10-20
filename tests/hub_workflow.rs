//! Concurrent agent coordination through the real loopback Hub and SQLite store.

#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use agentscm::backend::LocalBackend;
use agentscm::hub::coordination::CoordinationRequest;
use agentscm::hub::session::SessionCall;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token, unix_now};
use agentscm::hub::work::{
    CLAIM_STALE_SECONDS, CheckpointKind, WorkCheckpoint, WorkRole, WorkStart,
};
use agentscm::hub::{HubState, router};
use hub_pg::PgSchema;
use postgres::NoTls;
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPOSITORY: &str = "acme/widgets";
const OTHER_REPOSITORY: &str = "acme/private";
const REPOSITORY_ID: i64 = 101;

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    server: tokio::task::JoinHandle<()>,
    _directory: TempDir,
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Hub {
    async fn start() -> Self {
        let directory = TempDir::new().unwrap();
        let database = format!("sqlite://{}", directory.path().join("hub.sqlite").display());
        let store = tokio::task::spawn_blocking(move || store::open(&database))
            .await
            .unwrap()
            .unwrap();
        let objects = LocalBackend::open(directory.path().join("objects")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            store,
            client: reqwest::Client::new(),
            server,
            _directory: directory,
        }
    }

    async fn token(&self, owner_id: i64, agent: &str, repository: &str, read_only: bool) -> String {
        let store = self.store.clone();
        let agent = agent.to_owned();
        let repository = repository.to_owned();
        tokio::task::spawn_blocking(move || {
            let owner = store
                .upsert_user(
                    &ForgeIdentity {
                        provider: "github".into(),
                        host: "github.com".into(),
                        user_id: owner_id,
                    },
                    &format!("contributor{owner_id}"),
                )
                .unwrap();
            let (_, secret, _) = mint_bound_agent_token(
                &*store,
                &owner.id,
                &agent,
                &repository,
                REPOSITORY_ID,
                3600,
                read_only,
            )
            .unwrap();
            secret.expose().to_owned()
        })
        .await
        .unwrap()
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        secret: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
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
        let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"text": text}));
        (status, value)
    }

    async fn mcp(&self, secret: &str, name: &str, arguments: Value) -> Value {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        });
        let (status, response) = self.call(Method::POST, "/mcp", secret, Some(body)).await;
        assert_eq!(status, 200, "{response}");
        response["result"].clone()
    }
}

fn start(key: &str, title: &str, session: &str, scope: &[&str]) -> Value {
    json!({
        "request_key": key,
        "title": title,
        "session": session,
        "scope": scope,
        "branch": format!("feat/{key}"),
        "worktree": format!("{key}-checkout"),
        "next_action": "Inspect the current implementation"
    })
}

fn checkpoint(
    work_id: &str,
    operation_id: &str,
    session: &str,
    updated_seq: i64,
    kind: &str,
    summary: &str,
) -> Value {
    json!({
        "work_id": work_id,
        "operation_id": operation_id,
        "generation": 1,
        "expected_updated_seq": updated_seq,
        "session": session,
        "kind": kind,
        "summary": summary
    })
}

fn assert_scope_overlap(conflict: &Value, expected_incumbent: &str) {
    assert_eq!(conflict["kind"], "scope_overlap", "{conflict}");
    let overlaps = conflict["overlaps"].as_array().unwrap();
    assert!(!overlaps.is_empty(), "{conflict}");
    assert!(
        overlaps
            .iter()
            .any(|overlap| overlap["item"]["id"] == expected_incumbent),
        "{conflict}"
    );
    assert!(
        overlaps.iter().any(|overlap| {
            overlap["reasons"]
                .as_array()
                .is_some_and(|reasons| !reasons.is_empty())
        }),
        "{conflict}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_http_claims_and_mcp_participation_fence_overlapping_work() {
    let hub = Hub::start().await;
    let alpha = hub.token(7, "alpha-agent", REPOSITORY, false).await;
    let beta = hub.token(7, "beta-agent", REPOSITORY, false).await;
    let gamma = hub.token(7, "gamma-agent", REPOSITORY, false).await;
    let delta = hub.token(7, "delta-agent", REPOSITORY, false).await;
    let reader = hub.token(7, "overview", REPOSITORY, true).await;
    let outsider = hub
        .token(7, "private-repository", OTHER_REPOSITORY, true)
        .await;

    let alpha_start = start(
        "alpha-goal",
        "Refine the request engine",
        "alpha-session",
        &["src/core.rs", "src/alpha-only.rs"],
    );
    let beta_start = start(
        "beta-goal",
        "Add request tracing",
        "beta-session",
        &["src/core.rs", "src/beta-only.rs"],
    );
    let (alpha_reply, beta_reply) = tokio::join!(
        hub.call(
            Method::POST,
            "/v1/hub/standup/start",
            &alpha,
            Some(alpha_start)
        ),
        hub.call(
            Method::POST,
            "/v1/hub/standup/start",
            &beta,
            Some(beta_start)
        )
    );
    assert_eq!(alpha_reply.0, 200, "{}", alpha_reply.1);
    assert_eq!(beta_reply.0, 200, "{}", beta_reply.1);

    let created = [
        alpha_reply.1["created"].as_bool().unwrap(),
        beta_reply.1["created"].as_bool().unwrap(),
    ];
    assert_eq!(created.into_iter().filter(|created| *created).count(), 1);
    let (winner, winner_secret, winner_session, loser, loser_secret, loser_session) =
        if alpha_reply.1["created"] == true {
            (
                &alpha_reply.1,
                &alpha,
                "alpha-session",
                &beta_reply.1,
                &beta,
                "beta-session",
            )
        } else {
            (
                &beta_reply.1,
                &beta,
                "beta-session",
                &alpha_reply.1,
                &alpha,
                "alpha-session",
            )
        };
    let winner_id = winner["item"]["id"].as_str().unwrap().to_owned();
    assert_eq!(winner["conflict"], Value::Null, "{winner}");
    assert!(
        winner["context"]["current"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != winner_id),
        "the pre-claim context included the work it was about to create: {winner}"
    );
    assert_eq!(
        winner["context"]["until"].as_i64().unwrap()
            - winner["context"]["recent_since"].as_i64().unwrap(),
        24 * 60 * 60
    );
    assert_eq!(loser["created"], false);
    assert_eq!(loser["item"]["id"], winner_id);
    assert_scope_overlap(&loser["conflict"], &winner_id);
    assert_eq!(loser["coordination_request"]["status"], "pending");

    // An MCP client receives the same bounded conflict and must not announce
    // a rejected path through the older live-intent surface.
    let rejected = hub
        .mcp(
            loser_secret,
            "agentscm_start",
            json!({
                "task": "Try another overlapping lead",
                "session": "rejected-session",
                "request_key": "rejected-overlap",
                "paths": ["src/core.rs", "src/rejected-only.rs"]
            }),
        )
        .await;
    assert_eq!(rejected["isError"], true, "{rejected}");
    assert_scope_overlap(
        &rejected["structuredContent"]["work"]["conflict"],
        &winner_id,
    );
    let live = hub.store.live_intents(REPOSITORY, unix_now(), 100).unwrap();
    assert!(
        live.iter()
            .all(|intent| !intent.paths.contains(&"src/rejected-only.rs".to_owned())),
        "rejected lead announced its path: {live:?}"
    );

    // Close these proposed goals before exercising an unrelated scope expansion;
    // pending requests deliberately fence the incumbent's next edit checkpoint.
    for (request, session, operation_id) in [
        (
            &loser["coordination_request"],
            loser_session,
            "withdraw-first-overlap",
        ),
        (
            &rejected["structuredContent"]["work"]["coordination_request"],
            "rejected-session",
            "withdraw-second-overlap",
        ),
    ] {
        let withdrawn = hub
            .mcp(
                loser_secret,
                "agentscm_coordination_withdraw",
                json!({
                    "request_id": request["id"],
                    "operation_id": operation_id,
                    "session": session,
                    "expected_updated_seq": request["updated_seq"]
                }),
            )
            .await;
        assert_eq!(withdrawn["isError"], false, "{withdrawn}");
        assert_eq!(withdrawn["structuredContent"]["status"], "withdrawn");
    }

    // Explicit collaboration on the incumbent is allowed despite sharing its scope.
    let joined = hub
        .mcp(
            loser_secret,
            "agentscm_start",
            json!({
                "task": "Help with the request engine",
                "session": "helper-session",
                "work_id": winner_id,
                "role": "helper",
                "paths": ["src/core.rs"]
            }),
        )
        .await;
    assert_eq!(joined["isError"], false, "{joined}");
    assert_eq!(joined["structuredContent"]["work"]["joined"], true);
    assert!(
        joined["structuredContent"]["work"]["item"]["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|participant| participant["role"] == "helper")
    );

    let helper_check = hub
        .mcp(
            loser_secret,
            "agentscm_check",
            json!({"session": "helper-session", "paths": ["src/core.rs"]}),
        )
        .await;
    assert_eq!(helper_check["isError"], false, "{helper_check}");

    let outside_scope = hub
        .mcp(
            loser_secret,
            "agentscm_check",
            json!({
                "session": "helper-session",
                "paths": ["src/outside-declared-scope.rs"]
            }),
        )
        .await;
    assert_eq!(outside_scope["isError"], true, "{outside_scope}");
    assert_eq!(
        outside_scope["structuredContent"]["error"]["kind"],
        "invalid_or_changed"
    );
    let no_goal = hub
        .mcp(
            &delta,
            "agentscm_check",
            json!({"session": "delta-session", "paths": ["src/no-goal.rs"]}),
        )
        .await;
    assert_eq!(no_goal["isError"], true, "{no_goal}");
    let live = hub.store.live_intents(REPOSITORY, unix_now(), 100).unwrap();
    for refused_path in ["src/outside-declared-scope.rs", "src/no-goal.rs"] {
        assert!(
            live.iter()
                .all(|intent| !intent.paths.contains(&refused_path.to_owned())),
            "refused check announced {refused_path}: {live:?}"
        );
    }

    // A separate lead may own a non-overlapping path.
    let (status, separate) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &gamma,
            Some(start(
                "ui-goal",
                "Refine the work panel",
                "gamma-session",
                &["ui/panel.tsx"],
            )),
        )
        .await;
    assert_eq!(status, 200, "{separate}");
    assert_eq!(separate["created"], true, "{separate}");
    let separate_id = separate["item"]["id"].as_str().unwrap();

    let work_path = format!("/v1/hub/standup/work/{winner_id}");
    let (status, current) = hub.call(Method::GET, &work_path, winner_secret, None).await;
    assert_eq!(status, 200, "{current}");
    let expansion = json!({
        "work_id": winner_id,
        "operation_id": "expand-into-ui",
        "generation": current["generation"],
        "expected_updated_seq": current["updated_seq"],
        "session": winner_session,
        "kind": "progress",
        "summary": "Request work now needs a UI adjustment",
        "scope": ["src/core.rs", "ui/**"]
    });
    let (status, refused) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/checkpoint",
            winner_secret,
            Some(expansion),
        )
        .await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(refused["error"], "work_coordination_required");
    assert_eq!(refused["coordination_request"]["status"], "pending");
    assert!(
        refused["overlaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|overlap| overlap["item"]["id"] == separate_id
                && overlap["reasons"]
                    .as_array()
                    .is_some_and(|rows| !rows.is_empty()))
    );

    let mcp_refused = hub
        .mcp(
            winner_secret,
            "agentscm_work_checkpoint",
            json!({
                "work_id": winner_id,
                "operation_id": "expand-into-ui-mcp",
                "generation": current["generation"],
                "expected_updated_seq": current["updated_seq"],
                "session": winner_session,
                "kind": "progress",
                "summary": "Request work still needs a UI adjustment",
                "scope": ["src/core.rs", "ui/**"]
            }),
        )
        .await;
    assert_eq!(mcp_refused["isError"], true, "{mcp_refused}");
    assert!(
        mcp_refused["structuredContent"]["error"]["overlaps"]
            .as_array()
            .is_some_and(|overlaps| !overlaps.is_empty())
    );
    assert_eq!(
        mcp_refused["structuredContent"]["error"]["kind"],
        "work_coordination_required"
    );
    assert_eq!(
        mcp_refused["structuredContent"]["error"]["coordination_request"]["status"],
        "pending"
    );

    let events_before_overview = hub.store.events_after(0, 1000).unwrap().len();
    let (status, overview) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/overview?recent_since=0&limit=100",
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 200, "{overview}");
    assert!(
        overview["current"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == winner_id)
    );
    assert!(
        overview["current"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == separate_id)
    );
    assert_eq!(overview["overlaps"], json!([]));
    assert_eq!(overview["recent_since"], 0);
    assert!(overview["until"].as_i64().unwrap() <= unix_now());

    let mcp_overview = hub
        .mcp(
            &reader,
            "agentscm_work_overview",
            json!({"recent_since": 0, "limit": 100}),
        )
        .await;
    assert_eq!(mcp_overview["isError"], false, "{mcp_overview}");
    assert!(
        mcp_overview["structuredContent"]["current"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == winner_id)
    );
    assert_eq!(mcp_overview["structuredContent"]["overlaps"], json!([]));
    assert_eq!(
        hub.store.events_after(0, 1000).unwrap().len(),
        events_before_overview,
        "read-only overview calls wrote a Hub event"
    );

    let (status, hidden) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/overview?recent_since=0&limit=100",
            &outsider,
            None,
        )
        .await;
    assert_eq!(status, 200, "{hidden}");
    assert_eq!(hidden["current"], json!([]));
    assert_eq!(hidden["recent"], json!([]));
    assert!(!hidden.to_string().contains(&winner_id));
    assert_eq!(
        hub.call(Method::GET, &work_path, &outsider, None).await.0,
        404
    );
    for path in [
        "/v1/hub/standup/overview?limit=101",
        "/v1/hub/standup/overview?recent_since=-1",
        "/v1/hub/standup/overview?repository=acme/private",
    ] {
        assert_eq!(hub.call(Method::GET, path, &reader, None).await.0, 400);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exact_claims_acknowledgements_and_reconnects_remain_token_scoped() {
    let hub = Hub::start().await;
    let alpha = hub.token(9, "alpha-agent", REPOSITORY, false).await;
    let beta = hub.token(9, "beta-agent", REPOSITORY, false).await;
    let reader = hub.token(9, "overview", REPOSITORY, true).await;

    // Concurrent requests for one stable goal create one item and elect one lead.
    let alpha_start = start(
        "shared-goal",
        "Implement shared validation",
        "alpha-shared",
        &["src/shared.rs"],
    );
    let beta_start = start(
        "shared-goal",
        "Implement shared validation",
        "beta-shared",
        &["src/shared.rs"],
    );
    let (alpha_reply, beta_reply) = tokio::join!(
        hub.call(
            Method::POST,
            "/v1/hub/standup/start",
            &alpha,
            Some(alpha_start)
        ),
        hub.call(
            Method::POST,
            "/v1/hub/standup/start",
            &beta,
            Some(beta_start)
        )
    );
    assert_eq!(alpha_reply.0, 200, "{}", alpha_reply.1);
    assert_eq!(beta_reply.0, 200, "{}", beta_reply.1);
    assert_eq!(alpha_reply.1["item"]["id"], beta_reply.1["item"]["id"]);
    assert_eq!(
        [
            alpha_reply.1["created"].as_bool().unwrap(),
            beta_reply.1["created"].as_bool().unwrap()
        ]
        .into_iter()
        .filter(|created| *created)
        .count(),
        1
    );
    let (lead, lead_secret, lead_session, follower) = if alpha_reply.1["created"] == true {
        (&alpha_reply.1, &alpha, "alpha-shared", &beta_reply.1)
    } else {
        (&beta_reply.1, &beta, "beta-shared", &alpha_reply.1)
    };
    assert_eq!(follower["conflict"]["kind"], "claim_held", "{follower}");
    let work_id = lead["item"]["id"].as_str().unwrap().to_owned();

    let (status, first_delivery) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/briefing?limit=100",
            lead_secret,
            None,
        )
        .await;
    assert_eq!(status, 200, "{first_delivery}");
    assert_eq!(first_delivery["complete"], true);
    let delivery = first_delivery["delivery"].as_str().unwrap();
    // Another token for the same owner cannot acknowledge this agent's delivery.
    assert_eq!(
        hub.call(
            Method::POST,
            "/v1/hub/standup/ack",
            if lead_secret == &alpha { &beta } else { &alpha },
            Some(json!({"delivery": delivery}))
        )
        .await
        .0,
        400
    );
    assert_eq!(
        hub.call(
            Method::POST,
            "/v1/hub/standup/ack",
            lead_secret,
            Some(json!({"delivery": delivery}))
        )
        .await
        .0,
        200
    );

    let mut updated_seq = lead["item"]["updated_seq"].as_i64().unwrap();
    let (status, decision) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/checkpoint",
            lead_secret,
            Some(checkpoint(
                &work_id,
                "decision-1",
                lead_session,
                updated_seq,
                "decision",
                "Keep the validation boundary repository scoped",
            )),
        )
        .await;
    assert_eq!(status, 200, "{decision}");
    updated_seq = decision["item"]["updated_seq"].as_i64().unwrap();

    // The next briefing is a distinct catch-up after the acknowledged start.
    let (status, decision_delivery) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/briefing?limit=100",
            lead_secret,
            None,
        )
        .await;
    assert_eq!(status, 200, "{decision_delivery}");
    assert!(
        decision_delivery["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| {
                event["kind"] == "work_checkpoint" && event["item"]["current"]["kind"] == "decision"
            })
    );
    let decision_delivery_id = decision_delivery["delivery"].as_str().unwrap();
    assert_eq!(
        hub.call(
            Method::POST,
            "/v1/hub/standup/ack",
            lead_secret,
            Some(json!({"delivery": decision_delivery_id}))
        )
        .await
        .0,
        200
    );

    let (status, finished) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/checkpoint",
            lead_secret,
            Some(checkpoint(
                &work_id,
                "finish-1",
                lead_session,
                updated_seq,
                "finish",
                "Validation boundary implemented and checked",
            )),
        )
        .await;
    assert_eq!(status, 200, "{finished}");
    assert_eq!(finished["item"]["state"], "finished");

    let (status, finish_delivery) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/briefing?limit=100",
            lead_secret,
            None,
        )
        .await;
    assert_eq!(status, 200, "{finish_delivery}");
    assert!(finish_delivery["items"].as_array().unwrap().is_empty());
    assert!(
        finish_delivery["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| { event["work_id"] == work_id && event["item"]["state"] == "finished" })
    );
    assert_eq!(
        hub.call(
            Method::POST,
            "/v1/hub/standup/ack",
            lead_secret,
            Some(json!({"delivery": finish_delivery["delivery"]}))
        )
        .await
        .0,
        200
    );

    // The other token reconnects after that acknowledgement and still receives
    // the missed finish because acknowledgement state is per agent token.
    let reconnect_secret = if lead_secret == &alpha { &beta } else { &alpha };
    let (status, reconnect) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/briefing?limit=100",
            reconnect_secret,
            None,
        )
        .await;
    assert_eq!(status, 200, "{reconnect}");
    assert!(
        reconnect["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| { event["work_id"] == work_id && event["item"]["state"] == "finished" })
    );

    let since = unix_now() - 60;
    let (status, overview) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/overview?recent_since={since}&limit=100"),
            &reader,
            None,
        )
        .await;
    assert_eq!(status, 200, "{overview}");
    assert!(overview["current"].as_array().unwrap().is_empty());
    assert!(
        overview["recent"]
            .as_array()
            .unwrap()
            .iter()
            .any(|outcome| {
                outcome["kind"] == "completion"
                    && outcome["item"]["id"] == work_id
                    && outcome["item"]["state"] == "finished"
            })
    );
    assert_eq!(overview["overlaps"], json!([]));
    assert_eq!(overview["current_complete"], true);
    assert_eq!(overview["recent_complete"], true);
    assert_eq!(overview["overlaps_complete"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_coordination_replays_until_ack_and_requires_a_real_resolution() {
    let hub = Hub::start().await;
    let lead = hub.token(11, "lead-agent", REPOSITORY, false).await;
    let requester = hub.token(11, "requester-agent", REPOSITORY, false).await;
    let outsider = hub
        .token(11, "other-repository", OTHER_REPOSITORY, false)
        .await;

    let (status, incumbent) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &lead,
            Some(start(
                "coordination-incumbent",
                "Own the shared request path",
                "lead-session",
                &["src/shared-request.rs"],
            )),
        )
        .await;
    assert_eq!(status, 200, "{incumbent}");
    let work_id = incumbent["item"]["id"].as_str().unwrap().to_owned();

    let proposed = start(
        "coordination-proposal",
        "Extend the shared request path",
        "requester-session",
        &["src/shared-request.rs", "src/requester-only.rs"],
    );
    let (status, refused) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &requester,
            Some(proposed.clone()),
        )
        .await;
    assert_eq!(status, 200, "{refused}");
    assert_eq!(refused["created"], false);
    assert_scope_overlap(&refused["conflict"], &work_id);
    let request = refused["coordination_request"].clone();
    let request_id = request["id"].as_str().unwrap().to_owned();
    assert_eq!(request["status"], "pending");
    assert_eq!(request["acknowledgements"], json!([]));
    assert_eq!(request["responses"], json!([]));

    // A retry after a lost response resolves to the same durable request.
    let (status, replay) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &requester,
            Some(proposed),
        )
        .await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["coordination_request"]["id"], request_id);
    assert_eq!(
        replay["coordination_request"]["acknowledgements"],
        json!([])
    );

    let surfaced = hub
        .mcp(&lead, "agentscm_status", json!({"session": "lead-session"}))
        .await;
    assert_eq!(surfaced["isError"], false, "{surfaced}");
    assert!(
        surfaced["_meta"]["agentscm"]["coordination_notifications"]["notices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|candidate| candidate["request_id"] == request_id)
    );
    assert!(surfaced["content"].as_array().unwrap().len() >= 2);

    // Reading again after a gap in polling is a replay, not an implicit receipt.
    let inbox_path = "/v1/hub/standup/inbox?session=lead-session&limit=100";
    let (status, first_inbox) = hub.call(Method::GET, inbox_path, &lead, None).await;
    assert_eq!(status, 200, "{first_inbox}");
    assert_eq!(first_inbox["requests"][0]["id"], request_id);
    assert_eq!(first_inbox["requests"][0]["acknowledgements"], json!([]));
    let (status, replayed_inbox) = hub.call(Method::GET, inbox_path, &lead, None).await;
    assert_eq!(status, 200, "{replayed_inbox}");
    assert_eq!(replayed_inbox["requests"][0]["id"], request_id);
    assert_eq!(replayed_inbox["requests"][0]["acknowledgements"], json!([]));

    let acknowledged_seq = first_inbox["requests"][0]["updated_seq"].as_i64().unwrap();
    let (status, acknowledged) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/coordination/ack",
            &lead,
            Some(json!({
                "request_id": request_id,
                "target_work_id": work_id,
                "session": "lead-session",
                "expected_updated_seq": acknowledged_seq
            })),
        )
        .await;
    assert_eq!(status, 200, "{acknowledged}");
    assert_eq!(
        acknowledged["acknowledgements"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        acknowledged["acknowledgements"][0]["target_work_id"],
        work_id
    );

    let reply = hub
        .mcp(
            &lead,
            "agentscm_coordination_reply",
            json!({
                "request_id": request_id,
                "operation_id": "lead-offers-join",
                "target_work_id": work_id,
                "session": "lead-session",
                "expected_updated_seq": acknowledged["updated_seq"],
                "kind": "join",
                "note": "Join the existing work and report against the shared goal."
            }),
        )
        .await;
    assert_eq!(reply["isError"], false, "{reply}");
    assert_eq!(reply["structuredContent"]["status"], "responded");
    assert_eq!(reply["structuredContent"]["responses"][0]["kind"], "join");
    let _: CoordinationRequest =
        serde_json::from_value(reply["structuredContent"].clone()).unwrap();

    // The requester's session receives the report, but delivery itself is not an ack.
    let requester_inbox = hub
        .mcp(
            &requester,
            "agentscm_coordination_inbox",
            json!({"session": "requester-session", "limit": 100}),
        )
        .await;
    assert_eq!(requester_inbox["isError"], false, "{requester_inbox}");
    let reported = &requester_inbox["structuredContent"]["requests"][0];
    assert_eq!(reported["id"], request_id);
    assert_eq!(reported["responses"][0]["operation_id"], "lead-offers-join");
    assert!(
        reported["acknowledgements"]
            .as_array()
            .unwrap()
            .iter()
            .all(|receipt| receipt["target_work_id"].is_string()),
        "requester was acknowledged merely by reading: {reported}"
    );

    // A response is a report, not permission to close an overlap that still exists.
    let unresolved = hub
        .mcp(
            &requester,
            "agentscm_coordination_resolve",
            json!({
                "request_id": request_id,
                "operation_id": "resolve-before-join",
                "session": "requester-session",
                "expected_updated_seq": reported["updated_seq"]
            }),
        )
        .await;
    assert_eq!(unresolved["isError"], true, "{unresolved}");

    let requester_ack = hub
        .mcp(
            &requester,
            "agentscm_coordination_ack",
            json!({
                "request_id": request_id,
                "session": "requester-session",
                "expected_updated_seq": reported["updated_seq"]
            }),
        )
        .await;
    assert_eq!(requester_ack["isError"], false, "{requester_ack}");
    assert!(
        requester_ack["structuredContent"]["acknowledgements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|receipt| receipt["target_work_id"].is_null())
    );

    let joined = hub
        .mcp(
            &requester,
            "agentscm_start",
            json!({
                "task": "Join the shared request work",
                "session": "requester-session",
                "work_id": work_id,
                "role": "helper",
                "paths": ["src/shared-request.rs"]
            }),
        )
        .await;
    assert_eq!(joined["isError"], false, "{joined}");
    assert_eq!(joined["structuredContent"]["work"]["joined"], true);

    let resolved = hub
        .mcp(
            &requester,
            "agentscm_coordination_resolve",
            json!({
                "request_id": request_id,
                "operation_id": "resolve-after-join",
                "session": "requester-session",
                "expected_updated_seq": requester_ack["structuredContent"]["updated_seq"]
            }),
        )
        .await;
    assert_eq!(resolved["isError"], false, "{resolved}");
    assert_eq!(resolved["structuredContent"]["status"], "resolved");

    // Repository scoping applies to both inspection and mutation, even with known IDs.
    let (status, hidden) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/coordination?after=0&limit=100",
            &outsider,
            None,
        )
        .await;
    assert_eq!(status, 200, "{hidden}");
    assert_eq!(hidden["requests"], json!([]));
    assert!(!hidden.to_string().contains(&request_id));
    let (status, hidden_mutation) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/coordination/ack",
            &outsider,
            Some(json!({
                "request_id": request_id,
                "target_work_id": work_id,
                "session": "lead-session",
                "expected_updated_seq": resolved["structuredContent"]["updated_seq"]
            })),
        )
        .await;
    assert!(matches!(status, 400 | 403), "{hidden_mutation}");
    assert!(!hidden_mutation.to_string().contains(&request_id));
    assert!(!hidden_mutation.to_string().contains(&work_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_long_poll_and_fixed_pages_follow_current_recipients() {
    let hub = Hub::start().await;
    let original = hub.token(13, "original-lead", REPOSITORY, false).await;
    let requester = hub.token(13, "requester", REPOSITORY, false).await;
    let successor = hub.token(13, "successor", REPOSITORY, false).await;
    let additional = hub
        .token(13, "additional-requester", REPOSITORY, false)
        .await;

    let (status, incumbent) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &original,
            Some(start(
                "takeover-incumbent",
                "Own the takeover path",
                "original-session",
                &["src/takeover.rs"],
            )),
        )
        .await;
    assert_eq!(status, 200, "{incumbent}");
    let work_id = incumbent["item"]["id"].as_str().unwrap().to_owned();

    let (status, refused) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &requester,
            Some(start(
                "takeover-request",
                "Need the takeover path",
                "requester-session",
                &["src/takeover.rs"],
            )),
        )
        .await;
    assert_eq!(status, 200, "{refused}");
    let request = refused["coordination_request"].clone();
    let request_id = request["id"].as_str().unwrap().to_owned();

    let (status, handover) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/checkpoint",
            &original,
            Some(checkpoint(
                &work_id,
                "offer-handover",
                "original-session",
                incumbent["item"]["updated_seq"].as_i64().unwrap(),
                "handover",
                "Successor may take the current generation",
            )),
        )
        .await;
    assert_eq!(status, 200, "{handover}");

    let takeover = json!({
        "work_id": work_id,
        "title": "Own the takeover path",
        "session": "successor-session",
        "role": "lead",
        "scope": ["src/takeover.rs"],
        "branch": "feat/takeover-incumbent",
        "worktree": "takeover-incumbent-checkout",
        "next_action": "Inspect the current implementation",
        "takeover_generation": 1
    });
    let (status, taken) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &successor,
            Some(takeover),
        )
        .await;
    assert_eq!(status, 200, "{taken}");
    assert_eq!(taken["took_over"], true, "{taken}");
    assert_eq!(taken["item"]["generation"], 2);
    assert_eq!(taken["item"]["lead_session"], "successor-session");

    let fenced_check = hub
        .mcp(
            &original,
            "agentscm_check",
            json!({"session": "original-session", "paths": ["src/takeover.rs"]}),
        )
        .await;
    assert_eq!(fenced_check["isError"], true, "{fenced_check}");

    let (status, old_inbox) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/inbox?session=original-session&limit=100",
            &original,
            None,
        )
        .await;
    assert_eq!(status, 200, "{old_inbox}");
    assert_eq!(old_inbox["requests"], json!([]));
    let (status, old_ack) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/coordination/ack",
            &original,
            Some(json!({
                "request_id": request_id,
                "target_work_id": work_id,
                "session": "original-session",
                "expected_updated_seq": request["updated_seq"]
            })),
        )
        .await;
    assert_eq!(status, 401, "{old_ack}");

    let (status, successor_inbox) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/inbox?session=successor-session&limit=100",
            &successor,
            None,
        )
        .await;
    assert_eq!(status, 200, "{successor_inbox}");
    let transferred = successor_inbox["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["id"] == request_id)
        .unwrap();
    assert_eq!(
        transferred["targets"][0]["lead"]["session"],
        "successor-session"
    );
    assert_eq!(transferred["targets"][0]["generation"], 2);

    let (status, acknowledged) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/coordination/ack",
            &successor,
            Some(json!({
                "request_id": request_id,
                "target_work_id": work_id,
                "session": "successor-session",
                "expected_updated_seq": transferred["updated_seq"]
            })),
        )
        .await;
    assert_eq!(status, 200, "{acknowledged}");

    // Wait after a fixed cursor; a reply commits a new version and wakes the requester.
    let wait_path = format!(
        "/v1/hub/standup/inbox?session=requester-session&after={}&limit=100&wait_seconds=5",
        acknowledged["updated_seq"].as_i64().unwrap()
    );
    let waiting = hub.call(Method::GET, &wait_path, &requester, None);
    let reporting = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        hub.call(
            Method::POST,
            "/v1/hub/standup/coordination/reply",
            &successor,
            Some(json!({
                "request_id": request_id,
                "operation_id": "successor-proposes-split",
                "target_work_id": work_id,
                "session": "successor-session",
                "expected_updated_seq": acknowledged["updated_seq"],
                "kind": "propose_split",
                "note": "Keep takeover.rs here and split the caller-only path."
            })),
        )
        .await
    };
    let (waited, response) = tokio::join!(waiting, reporting);
    assert_eq!(response.0, 200, "{}", response.1);
    assert_eq!(waited.0, 200, "{}", waited.1);
    assert!(
        waited.1["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|candidate| candidate["id"] == request_id
                && candidate["responses"][0]["operation_id"] == "successor-proposes-split"),
        "{}",
        waited.1
    );

    // Capture a watermark, then prove later requests cannot drift into its pages.
    let mut before_watermark = Vec::new();
    for (key, session) in [
        ("fixed-page-one", "additional-one"),
        ("fixed-page-two", "additional-two"),
    ] {
        let (status, row) = hub
            .call(
                Method::POST,
                "/v1/hub/standup/start",
                &additional,
                Some(start(
                    key,
                    "Need the paged path",
                    session,
                    &["src/takeover.rs"],
                )),
            )
            .await;
        assert_eq!(status, 200, "{row}");
        before_watermark.push(
            row["coordination_request"]["id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let (status, first_page) = hub
        .call(
            Method::GET,
            "/v1/hub/standup/inbox?session=successor-session&after=0&limit=1",
            &successor,
            None,
        )
        .await;
    assert_eq!(status, 200, "{first_page}");
    assert_eq!(first_page["complete"], false);
    let through = first_page["through"].as_i64().unwrap();

    let (status, after_watermark) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &additional,
            Some(start(
                "fixed-page-later",
                "Need the later paged path",
                "additional-later",
                &["src/takeover.rs"],
            )),
        )
        .await;
    assert_eq!(status, 200, "{after_watermark}");
    let later_id = after_watermark["coordination_request"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut fixed_ids = Vec::new();
    let mut page = first_page;
    loop {
        fixed_ids.extend(
            page["requests"]
                .as_array()
                .unwrap()
                .iter()
                .map(|request| request["id"].as_str().unwrap().to_owned()),
        );
        if page["complete"] == true {
            break;
        }
        let path = format!(
            "/v1/hub/standup/inbox?session=successor-session&after={}&through={through}&limit=1",
            page["next_after"].as_i64().unwrap()
        );
        let (status, next) = hub.call(Method::GET, &path, &successor, None).await;
        assert_eq!(status, 200, "{next}");
        assert_eq!(next["through"], through);
        page = next;
    }
    assert!(!fixed_ids.contains(&later_id));
    assert!(before_watermark.iter().all(|id| fixed_ids.contains(id)));
    let (status, fresh) = hub
        .call(
            Method::GET,
            &format!("/v1/hub/standup/inbox?session=successor-session&after={through}&limit=100"),
            &successor,
            None,
        )
        .await;
    assert_eq!(status, 200, "{fresh}");
    assert!(
        fresh["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|request| request["id"] == later_id)
    );

    // A successful finish removes both durable ownership and the live MCP intent.
    let finishing = hub
        .mcp(
            &requester,
            "agentscm_start",
            json!({
                "task": "Finish an isolated live assignment",
                "session": "finishing-session",
                "request_key": "finish-releases-live-intent",
                "paths": ["src/finished-live.rs"]
            }),
        )
        .await;
    assert_eq!(finishing["isError"], false, "{finishing}");
    let finishing_work = &finishing["structuredContent"]["work"]["item"];
    let finishing_id = finishing_work["id"].as_str().unwrap().to_owned();
    assert!(
        hub.store
            .live_intents(REPOSITORY, unix_now(), 100)
            .unwrap()
            .iter()
            .any(|intent| intent.paths.contains(&"src/finished-live.rs".to_owned()))
    );
    let finished = hub
        .mcp(
            &requester,
            "agentscm_work_checkpoint",
            checkpoint(
                &finishing_id,
                "finish-and-release",
                "finishing-session",
                finishing_work["updated_seq"].as_i64().unwrap(),
                "finish",
                "The isolated assignment is complete",
            ),
        )
        .await;
    assert_eq!(finished["isError"], false, "{finished}");
    assert_eq!(finished["structuredContent"]["item"]["state"], "finished");
    assert!(
        hub.store
            .live_intents(REPOSITORY, unix_now(), 100)
            .unwrap()
            .iter()
            .all(|intent| !intent.paths.contains(&"src/finished-live.rs".to_owned()))
    );
    let (status, replacement) = hub
        .call(
            Method::POST,
            "/v1/hub/standup/start",
            &original,
            Some(start(
                "finished-live-replacement",
                "Claim the released live path",
                "replacement-session",
                &["src/finished-live.rs"],
            )),
        )
        .await;
    assert_eq!(status, 200, "{replacement}");
    assert_eq!(replacement["created"], true, "{replacement}");
}

#[test]
fn postgres_finish_and_live_heartbeat_share_one_lock_order() {
    let Some(pg) = PgSchema::create() else {
        return;
    };
    // Bound any regression to a test failure instead of leaving database workers deadlocked.
    let database = pg.url.replace(
        "&application_name=",
        "%20-c%20statement_timeout%3D3000&application_name=",
    );
    let setup = store::open(&database).unwrap();
    let owner = setup
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 21,
            },
            "contributor21",
        )
        .unwrap();
    let (token, _, _) = mint_bound_agent_token(
        &*setup,
        &owner.id,
        "finish-race-agent",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap();
    let now = unix_now();
    let started = setup
        .work_start(
            &token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("postgres-finish-race".into()),
                title: "Finish while the live session heartbeats".into(),
                session: "finish-race-session".into(),
                role: WorkRole::Lead,
                scope: vec!["src/finish-race.rs".into()],
                branch: Some("feat/postgres-finish-race".into()),
                worktree: Some("postgres-finish-race".into()),
                next_action: Some("finish after the final heartbeat".into()),
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
            },
            now,
        )
        .unwrap()
        .item;
    let initial_touch = setup
        .touch_session(
            &token,
            &SessionCall {
                session: Some("finish-race-session".into()),
                via: "mcp".into(),
                task: Some("Finish while the live session heartbeats".into()),
                paths: vec!["src/finish-race.rs".into()],
                deny: false,
                report: false,
            },
            now,
        )
        .unwrap();
    let intent_id = initial_touch.intent.unwrap().id;
    drop(setup);

    // Hold the intent row so the heartbeat is observably inside its transaction with the
    // repository lock. The finish must wait behind it before appending its event.
    let mut blocker_client = postgres::Client::connect(&database, NoTls).unwrap();
    let mut blocker = blocker_client.transaction().unwrap();
    blocker
        .query_one(
            "SELECT id FROM hub_intents WHERE id = $1 FOR UPDATE",
            &[&intent_id],
        )
        .unwrap();

    let touch_store = store::open(&database).unwrap();
    let finish_store = store::open(&database).unwrap();
    let start_touch = Arc::new(Barrier::new(2));
    let touch_gate = Arc::clone(&start_touch);
    let touch_token = token.clone();
    let touch = thread::spawn(move || {
        touch_gate.wait();
        touch_store.touch_session(
            &touch_token,
            &SessionCall {
                session: Some("finish-race-session".into()),
                via: "hook:codex".into(),
                task: None,
                paths: vec!["src/finish-race.rs".into()],
                deny: false,
                report: false,
            },
            now + 1,
        )
    });
    start_touch.wait();
    thread::sleep(Duration::from_millis(100));
    assert!(
        !touch.is_finished(),
        "heartbeat did not wait on the held intent row"
    );

    let finish_token = token.clone();
    let work_id = started.id.clone();
    let finish = thread::spawn(move || {
        finish_store.work_checkpoint(
            &finish_token,
            &WorkCheckpoint {
                work_id,
                operation_id: "finish-after-heartbeat".into(),
                generation: started.generation,
                session: "finish-race-session".into(),
                expected_updated_seq: Some(started.updated_seq),
                kind: CheckpointKind::Finish,
                summary: "Final heartbeat observed; work is complete".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: Vec::new(),
                depends_on: Vec::new(),
                related_work: Vec::new(),
            },
            now + 2,
        )
    });
    thread::sleep(Duration::from_millis(50));
    assert!(!touch.is_finished());
    assert!(!finish.is_finished());
    blocker.commit().unwrap();

    touch.join().unwrap().unwrap();
    let finished = finish.join().unwrap().unwrap();
    assert_eq!(
        finished.item.state,
        agentscm::hub::work::WorkState::Finished
    );
    assert!(
        store::open(&database)
            .unwrap()
            .live_intents(REPOSITORY, now + 2, 100)
            .unwrap()
            .iter()
            .all(|intent| intent.id != intent_id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_hook_adopts_default_work_and_enforces_its_lifecycle() {
    let hub = Hub::start().await;
    let original = hub
        .token(23, "native-hook-original", REPOSITORY, false)
        .await;
    let successor = hub
        .token(23, "native-hook-successor", REPOSITORY, false)
        .await;
    let original_session = "11111111-2222-3333-4444-555555555555";
    let successor_session = "66666666-7777-8888-9999-000000000000";
    let hook_path = "/v1/hub/hook/codex";

    let started = hub
        .mcp(
            &original,
            "agentscm_start",
            json!({
                "task": "Exercise native hook lifecycle enforcement",
                "request_key": "native-hook-default-work",
                "paths": ["src/native-hook.rs"],
                "branch": "feat/native-hook-lifecycle",
                "next_action": "Edit through the native hook"
            }),
        )
        .await;
    assert_eq!(started["isError"], false, "{started}");
    let started_item = &started["structuredContent"]["work"]["item"];
    assert_eq!(started_item["lead_session"], "default", "{started}");
    let work_id = started_item["id"].as_str().unwrap().to_owned();

    let session_start = json!({
        "session_id": original_session,
        "transcript_path": "/work/.codex/sessions/native-hook-original.jsonl",
        "cwd": "/work/acme-widgets",
        "hook_event_name": "SessionStart",
        "model": "synthetic-model",
        "permission_mode": "default",
        "source": "startup"
    });
    let (status, startup) = hub
        .call(Method::POST, hook_path, &original, Some(session_start))
        .await;
    assert_eq!(status, 200, "{startup}");
    assert!(
        startup["hookSpecificOutput"]["additionalContext"].is_string(),
        "{startup}"
    );

    let edit = |session: &str, path: &str, call: &str| {
        json!({
            "session_id": session,
            "transcript_path": format!("/work/.codex/sessions/{call}.jsonl"),
            "cwd": "/work/acme-widgets",
            "hook_event_name": "PreToolUse",
            "model": "synthetic-model",
            "permission_mode": "default",
            "turn_id": "turn-synthetic",
            "tool_name": "apply_patch",
            "tool_use_id": call,
            "tool_input": {
                "command": format!(
                    "*** Begin Patch\n*** Update File: {path}\n@@\n-old\n+new\n*** End Patch\n"
                )
            }
        })
    };
    let denied = |answer: &Value| {
        answer["hookSpecificOutput"]["permissionDecision"] == "deny"
            && answer["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .is_some_and(|reason| {
                    reason.contains("not covered by a current unfinished assignment")
                })
    };

    let second_unknown_session = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let (status, second_unknown) = hub
        .call(
            Method::POST,
            hook_path,
            &original,
            Some(edit(
                second_unknown_session,
                "src/native-hook.rs",
                "rejected-second-unknown-session",
            )),
        )
        .await;
    assert_eq!(status, 200, "{second_unknown}");
    assert_eq!(
        second_unknown["hookSpecificOutput"]["permissionDecision"], "deny",
        "{second_unknown}"
    );
    assert_eq!(
        second_unknown["hookSpecificOutput"]["hookEventName"], "PreToolUse",
        "{second_unknown}"
    );
    assert!(
        second_unknown["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .is_some_and(|reason| reason.contains("could not be linked to a current assignment")),
        "{second_unknown}"
    );

    let (status, allowed) = hub
        .call(
            Method::POST,
            hook_path,
            &original,
            Some(edit(
                original_session,
                "src/native-hook.rs",
                "allowed-active-edit",
            )),
        )
        .await;
    assert_eq!(status, 200, "{allowed}");
    assert!(!denied(&allowed), "{allowed}");
    assert!(
        hub.store
            .live_intents(REPOSITORY, unix_now(), 100)
            .unwrap()
            .iter()
            .any(|intent| intent.paths.contains(&"src/native-hook.rs".to_owned()))
    );

    let (status, outside) = hub
        .call(
            Method::POST,
            hook_path,
            &original,
            Some(edit(
                original_session,
                "src/outside-native-hook.rs",
                "rejected-outside-edit",
            )),
        )
        .await;
    assert_eq!(status, 200, "{outside}");
    assert!(denied(&outside), "{outside}");

    let session_end = json!({
        "session_id": original_session,
        "transcript_path": "/work/.codex/sessions/native-hook-original.jsonl",
        "cwd": "/work/acme-widgets",
        "hook_event_name": "SessionEnd",
        "reason": "other"
    });
    let (status, ended) = hub
        .call(Method::POST, hook_path, &original, Some(session_end))
        .await;
    assert_eq!(status, 200, "{ended}");
    assert_eq!(ended, json!({}));

    let work_path = format!("/v1/hub/standup/work/{work_id}");
    let (status, paused) = hub.call(Method::GET, &work_path, &original, None).await;
    assert_eq!(status, 200, "{paused}");
    assert_eq!(paused["state"], "paused", "{paused}");
    assert_eq!(paused["lead_session"], "default", "{paused}");
    let (status, paused_edit) = hub
        .call(
            Method::POST,
            hook_path,
            &original,
            Some(edit(
                original_session,
                "src/native-hook.rs",
                "rejected-paused-edit",
            )),
        )
        .await;
    assert_eq!(status, 200, "{paused_edit}");
    assert!(denied(&paused_edit), "{paused_edit}");

    let handed_over = hub
        .mcp(
            &original,
            "agentscm_work_checkpoint",
            checkpoint(
                &work_id,
                "native-hook-handover",
                "default",
                paused["updated_seq"].as_i64().unwrap(),
                "handover",
                "Ready for a successor",
            ),
        )
        .await;
    assert_eq!(handed_over["isError"], false, "{handed_over}");

    let taken = hub
        .mcp(
            &successor,
            "agentscm_start",
            json!({
                "task": "Take over native hook lifecycle enforcement",
                "work_id": work_id,
                "takeover_generation": 1,
                "paths": ["src/native-hook.rs"],
                "branch": "feat/native-hook-lifecycle",
                "next_action": "Finish through the successor session"
            }),
        )
        .await;
    assert_eq!(taken["isError"], false, "{taken}");
    let taken_item = &taken["structuredContent"]["work"]["item"];
    assert_eq!(taken["structuredContent"]["work"]["took_over"], true);
    assert_eq!(taken_item["generation"], 2);
    assert_eq!(taken_item["lead_session"], "default");

    let (status, fenced_edit) = hub
        .call(
            Method::POST,
            hook_path,
            &original,
            Some(edit(
                original_session,
                "src/native-hook.rs",
                "rejected-fenced-edit",
            )),
        )
        .await;
    assert_eq!(status, 200, "{fenced_edit}");
    assert!(denied(&fenced_edit), "{fenced_edit}");

    let successor_start = json!({
        "session_id": successor_session,
        "transcript_path": "/work/.codex/sessions/native-hook-successor.jsonl",
        "cwd": "/work/acme-widgets",
        "hook_event_name": "SessionStart",
        "model": "synthetic-model",
        "permission_mode": "default",
        "source": "startup"
    });
    let (status, successor_started) = hub
        .call(Method::POST, hook_path, &successor, Some(successor_start))
        .await;
    assert_eq!(status, 200, "{successor_started}");

    let finished = hub
        .mcp(
            &successor,
            "agentscm_work_checkpoint",
            json!({
                "work_id": work_id,
                "operation_id": "native-hook-finish",
                "generation": 2,
                "expected_updated_seq": taken_item["updated_seq"],
                "session": "default",
                "kind": "finish",
                "summary": "Native hook lifecycle enforcement is complete"
            }),
        )
        .await;
    assert_eq!(finished["isError"], false, "{finished}");
    assert_eq!(finished["structuredContent"]["item"]["state"], "finished");

    let (status, finished_edit) = hub
        .call(
            Method::POST,
            hook_path,
            &successor,
            Some(edit(
                successor_session,
                "src/native-hook.rs",
                "rejected-finished-edit",
            )),
        )
        .await;
    assert_eq!(status, 200, "{finished_edit}");
    assert!(denied(&finished_edit), "{finished_edit}");
}

fn assert_managed_touch_transitions_are_atomic(database: &str) {
    let setup = store::open(database).unwrap();
    let owner = setup
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 31,
            },
            "contributor31",
        )
        .unwrap();
    let (original, _, _) = mint_bound_agent_token(
        &*setup,
        &owner.id,
        "managed-original",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap();
    let (successor, _, _) = mint_bound_agent_token(
        &*setup,
        &owner.id,
        "managed-successor",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap();
    let now = unix_now();

    let takeover_item = setup
        .work_start(
            &original,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("managed-takeover-race".into()),
                title: "Managed takeover race".into(),
                session: "managed-old-session".into(),
                role: WorkRole::Lead,
                scope: vec!["src/managed-takeover.rs".into()],
                branch: Some("feat/managed-takeover".into()),
                worktree: Some("managed-takeover".into()),
                next_action: Some("handover safely".into()),
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
            },
            now,
        )
        .unwrap()
        .item;
    let precheck = setup
        .work_scope_check(
            &original,
            "managed-old-session",
            &["src/managed-takeover.rs".into()],
            now,
        )
        .unwrap();
    assert!(precheck.uncovered_scope.is_empty());
    let handed_over = setup
        .work_checkpoint(
            &original,
            &WorkCheckpoint {
                work_id: takeover_item.id.clone(),
                operation_id: "prepare-managed-takeover".into(),
                generation: takeover_item.generation,
                session: "managed-old-session".into(),
                expected_updated_seq: Some(takeover_item.updated_seq),
                kind: CheckpointKind::Handover,
                summary: "Ready for the next lead".into(),
                next_action: None,
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
        .unwrap()
        .item;
    let handover_touch = setup.touch_session(
        &original,
        &SessionCall {
            session: Some("managed-old-session".into()),
            via: "hook:codex".into(),
            task: None,
            paths: vec!["src/managed-takeover.rs".into()],
            deny: true,
            report: true,
        },
        now + 2,
    );
    assert!(matches!(
        handover_touch,
        Err(agentscm::hub::HubError::Invalid(ref message))
            if message.contains("not covered by a current unfinished assignment")
    ));
    assert!(
        setup
            .live_intents(REPOSITORY, now + 2, 100)
            .unwrap()
            .iter()
            .all(|intent| !intent.paths.contains(&"src/managed-takeover.rs".to_owned()))
    );
    let resumed_takeover = setup
        .work_checkpoint(
            &original,
            &WorkCheckpoint {
                work_id: takeover_item.id.clone(),
                operation_id: "resume-managed-takeover".into(),
                generation: takeover_item.generation,
                session: "managed-old-session".into(),
                expected_updated_seq: Some(handed_over.updated_seq),
                kind: CheckpointKind::Progress,
                summary: "Resume before the atomic takeover race".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: Vec::new(),
                depends_on: Vec::new(),
                related_work: Vec::new(),
            },
            now + 3,
        )
        .unwrap()
        .item;
    drop(setup);

    let takeover_at = now + 3 + CLAIM_STALE_SECONDS + 1;

    let barrier = Arc::new(Barrier::new(3));
    let old_store = store::open(database).unwrap();
    let old_barrier = Arc::clone(&barrier);
    let old_token = original.clone();
    let touch = thread::spawn(move || {
        old_barrier.wait();
        old_store.touch_session(
            &old_token,
            &SessionCall {
                session: Some("managed-old-session".into()),
                via: "hook:codex".into(),
                task: None,
                paths: vec!["src/managed-takeover.rs".into()],
                deny: true,
                report: true,
            },
            takeover_at,
        )
    });
    let takeover_store = store::open(database).unwrap();
    let takeover_barrier = Arc::clone(&barrier);
    let takeover_token = successor.clone();
    let takeover_id = resumed_takeover.id.clone();
    let takeover = thread::spawn(move || {
        takeover_barrier.wait();
        takeover_store.work_start(
            &takeover_token,
            &WorkStart {
                work_id: Some(takeover_id),
                source: None,
                request_key: None,
                title: "Managed takeover race".into(),
                session: "managed-new-session".into(),
                role: WorkRole::Lead,
                scope: vec!["src/managed-takeover.rs".into()],
                branch: Some("feat/managed-takeover".into()),
                worktree: Some("managed-takeover".into()),
                next_action: Some("handover safely".into()),
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: Some(resumed_takeover.generation),
            },
            takeover_at,
        )
    });
    barrier.wait();
    let touched = touch.join().unwrap();
    if let Err(error) = &touched {
        assert!(
            matches!(error, agentscm::hub::HubError::Invalid(message)
                if message.contains("not covered by a current unfinished assignment")),
            "unexpected managed touch error: {error:?}"
        );
    }
    assert!(takeover.join().unwrap().unwrap().took_over);

    let after_takeover = store::open(database).unwrap();
    assert!(
        after_takeover
            .live_intents(REPOSITORY, takeover_at + 1, 100)
            .unwrap()
            .iter()
            .all(|intent| !intent.paths.contains(&"src/managed-takeover.rs".to_owned()))
    );
    let fenced = after_takeover.touch_session(
        &original,
        &SessionCall {
            session: Some("managed-old-session".into()),
            via: "hook:codex".into(),
            task: None,
            paths: vec!["src/managed-takeover.rs".into()],
            deny: true,
            report: true,
        },
        takeover_at + 1,
    );
    assert!(matches!(
        fenced,
        Err(agentscm::hub::HubError::Invalid(ref message))
            if message.contains("not covered by a current unfinished assignment")
    ));
    let fenced_scope = after_takeover
        .work_scope_check(
            &original,
            "managed-old-session",
            &["src/managed-takeover.rs".into()],
            takeover_at + 1,
        )
        .unwrap();
    assert!(fenced_scope.item.is_some());
    assert_eq!(
        fenced_scope.uncovered_scope,
        vec!["src/managed-takeover.rs"]
    );

    let finish_item = after_takeover
        .work_start(
            &original,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("managed-finish-race".into()),
                title: "Managed finish race".into(),
                session: "managed-finish-session".into(),
                role: WorkRole::Lead,
                scope: vec!["src/managed-finish.rs".into()],
                branch: Some("feat/managed-finish".into()),
                worktree: Some("managed-finish".into()),
                next_action: Some("finish safely".into()),
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
            },
            takeover_at + 2,
        )
        .unwrap()
        .item;
    let finish_precheck = after_takeover
        .work_scope_check(
            &original,
            "managed-finish-session",
            &["src/managed-finish.rs".into()],
            takeover_at + 2,
        )
        .unwrap();
    assert!(finish_precheck.uncovered_scope.is_empty());
    let outside = after_takeover.touch_session(
        &original,
        &SessionCall {
            session: Some("managed-finish-session".into()),
            via: "hook:codex".into(),
            task: None,
            paths: vec!["src/not-declared.rs".into()],
            deny: true,
            report: true,
        },
        takeover_at + 2,
    );
    assert!(matches!(outside, Err(agentscm::hub::HubError::Invalid(_))));
    let paused = after_takeover
        .work_checkpoint(
            &original,
            &WorkCheckpoint {
                work_id: finish_item.id.clone(),
                operation_id: "pause-managed-finish".into(),
                generation: finish_item.generation,
                session: "managed-finish-session".into(),
                expected_updated_seq: Some(finish_item.updated_seq),
                kind: CheckpointKind::Pause,
                summary: "Pause before resuming the finish race".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: Vec::new(),
                depends_on: Vec::new(),
                related_work: Vec::new(),
            },
            takeover_at + 3,
        )
        .unwrap()
        .item;
    let paused_touch = after_takeover.touch_session(
        &original,
        &SessionCall {
            session: Some("managed-finish-session".into()),
            via: "hook:codex".into(),
            task: None,
            paths: vec!["src/managed-finish.rs".into()],
            deny: true,
            report: true,
        },
        takeover_at + 4,
    );
    assert!(matches!(
        paused_touch,
        Err(agentscm::hub::HubError::Invalid(ref message))
            if message.contains("not covered by a current unfinished assignment")
    ));
    assert!(
        after_takeover
            .live_intents(REPOSITORY, takeover_at + 4, 100)
            .unwrap()
            .iter()
            .all(|intent| !intent.paths.contains(&"src/managed-finish.rs".to_owned()))
    );
    let resumed_finish = after_takeover
        .work_checkpoint(
            &original,
            &WorkCheckpoint {
                work_id: finish_item.id.clone(),
                operation_id: "resume-managed-finish".into(),
                generation: finish_item.generation,
                session: "managed-finish-session".into(),
                expected_updated_seq: Some(paused.updated_seq),
                kind: CheckpointKind::Progress,
                summary: "Resume before the atomic finish race".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: Vec::new(),
                depends_on: Vec::new(),
                related_work: Vec::new(),
            },
            takeover_at + 5,
        )
        .unwrap()
        .item;
    drop(after_takeover);

    let barrier = Arc::new(Barrier::new(3));
    let touch_store = store::open(database).unwrap();
    let touch_barrier = Arc::clone(&barrier);
    let touch_token = original.clone();
    let touch = thread::spawn(move || {
        touch_barrier.wait();
        touch_store.touch_session(
            &touch_token,
            &SessionCall {
                session: Some("managed-finish-session".into()),
                via: "hook:codex".into(),
                task: None,
                paths: vec!["src/managed-finish.rs".into()],
                deny: true,
                report: true,
            },
            takeover_at + 6,
        )
    });
    let finish_store = store::open(database).unwrap();
    let finish_barrier = Arc::clone(&barrier);
    let finish_token = original.clone();
    let finish = thread::spawn(move || {
        finish_barrier.wait();
        finish_store.work_checkpoint(
            &finish_token,
            &WorkCheckpoint {
                work_id: resumed_finish.id,
                operation_id: "finish-managed-race".into(),
                generation: resumed_finish.generation,
                session: "managed-finish-session".into(),
                expected_updated_seq: Some(resumed_finish.updated_seq),
                kind: CheckpointKind::Finish,
                summary: "Managed work finished".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: Vec::new(),
                depends_on: Vec::new(),
                related_work: Vec::new(),
            },
            takeover_at + 6,
        )
    });
    barrier.wait();
    let touched = touch.join().unwrap();
    if let Err(error) = &touched {
        assert!(
            matches!(error, agentscm::hub::HubError::Invalid(message)
                if message.contains("not covered by a current unfinished assignment")),
            "unexpected managed touch error: {error:?}"
        );
    }
    assert_eq!(
        finish.join().unwrap().unwrap().item.state,
        agentscm::hub::work::WorkState::Finished
    );

    let finished_store = store::open(database).unwrap();
    assert!(
        finished_store
            .live_intents(REPOSITORY, takeover_at + 7, 100)
            .unwrap()
            .iter()
            .all(|intent| !intent.paths.contains(&"src/managed-finish.rs".to_owned()))
    );
    let finished_touch = finished_store.touch_session(
        &original,
        &SessionCall {
            session: Some("managed-finish-session".into()),
            via: "hook:codex".into(),
            task: None,
            paths: vec!["src/managed-finish.rs".into()],
            deny: true,
            report: true,
        },
        takeover_at + 7,
    );
    assert!(matches!(
        finished_touch,
        Err(agentscm::hub::HubError::Invalid(ref message))
            if message.contains("not covered by a current unfinished assignment")
    ));
    let finished_scope = finished_store
        .work_scope_check(
            &original,
            "managed-finish-session",
            &["src/managed-finish.rs".into()],
            takeover_at + 7,
        )
        .unwrap();
    assert!(finished_scope.item.is_some());
    assert_eq!(
        finished_scope.uncovered_scope,
        vec!["src/managed-finish.rs"]
    );
}

#[test]
fn sqlite_managed_touches_are_atomic_with_takeover_and_finish() {
    let directory = TempDir::new().unwrap();
    let database = format!(
        "sqlite://{}",
        directory.path().join("managed-touch.sqlite").display()
    );
    assert_managed_touch_transitions_are_atomic(&database);
}

#[test]
fn postgres_managed_touches_are_atomic_with_takeover_and_finish() {
    let Some(pg) = PgSchema::create() else {
        return;
    };
    let database = pg.url.replace(
        "&application_name=",
        "%20-c%20statement_timeout%3D3000&application_name=",
    );
    assert_managed_touch_transitions_are_atomic(&database);
}
