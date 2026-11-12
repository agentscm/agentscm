//! The per-agent hook endpoint: every answer is HTTP 200 with JSON in the agent's own
//! output format, failures say coordination is unknown, and the only decision is refusing the
//! first contested edit of a file before it runs.
//! A real server on loopback and SQLite; the store behind it is covered by hub_sessions.
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentscm::backend::LocalBackend;
use agentscm::hub::agent_rules::NO_BYPASS;
use agentscm::hub::store::{
    self, ForgeIdentity, Store, agent_token_digest, lower_hex_of, mint_agent_token,
    mint_bound_agent_token, unix_now,
};
use agentscm::hub::work::{WorkRole, WorkStart, WorkState};
use agentscm::hub::work_lifecycle::WORK_UNDECLARED;
use agentscm::hub::{HubState, router};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hub_hooks");

struct Hub {
    address: SocketAddr,
    store: Arc<dyn Store>,
    next_user: i64,
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
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            address,
            store,
            next_user: 0,
            _dir: dir,
        }
    }

    /// The bearer secret of a token for a fresh user.
    fn agent(&mut self) -> String {
        self.next_user += 1;
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id: self.next_user,
        };
        let user = self
            .store
            .upsert_user(&identity, &format!("user{}", self.next_user))
            .unwrap();
        let (_, secret) = mint_agent_token(&*self.store, &user.id, "agent", REPO, 3600).unwrap();
        secret.expose().to_owned()
    }

    fn post(&self, path: &str, secret: Option<&str>, body: &[u8]) -> (u16, Value) {
        self.post_with(path, secret, "", body)
    }

    /// POST `body` with `extra` raw header lines; returns the status and the parsed JSON body.
    fn post_with(
        &self,
        path: &str,
        secret: Option<&str>,
        extra: &str,
        body: &[u8],
    ) -> (u16, Value) {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut head = format!(
            "POST {path} HTTP/1.1\r\nHost: hub\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
        head.push_str(extra);
        if let Some(secret) = secret {
            head.push_str(&format!("Authorization: Bearer {secret}\r\n"));
        }
        head.push_str("\r\n");
        // The hub may answer before it has read a body it refuses, so write on the side.
        let mut writer = stream.try_clone().unwrap();
        let body = body.to_vec();
        let writing = std::thread::spawn(move || {
            let _ = writer.write_all(head.as_bytes());
            let _ = writer.write_all(&body);
        });
        let mut raw = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&chunk[..n]),
                Err(_) if raw.windows(4).any(|w| w == b"\r\n\r\n") => break,
                Err(error) => panic!("no response: {error}"),
            }
        }
        writing.join().unwrap();
        let raw = String::from_utf8(raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let content_type = head.to_ascii_lowercase();
        assert!(
            status != 200 || content_type.contains("content-type: application/json"),
            "{head}"
        );
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    /// POST a fixture (or an edit of it) to `/v1/hub/hook/{agent}`; the answer must be a 200.
    fn hook_raw(&self, agent: &str, secret: &str, payload: &Value) -> Value {
        let (status, answer) = self.post(
            &format!("/v1/hub/hook/{agent}"),
            Some(secret),
            payload.to_string().as_bytes(),
        );
        assert_eq!(status, 200, "{agent}: {payload}");
        assert!(answer.is_object(), "{agent}: {answer}");
        answer
    }

    /// Like `hook_raw`, for a call that must not be refused. A session's first edit while it has
    /// declared no Work is refused once with `WORK_UNDECLARED`; its repeat is the call checked.
    fn hook(&self, agent: &str, secret: &str, payload: &Value) -> Value {
        let mut answer = self.hook_raw(agent, secret, payload);
        let shown = answer.to_string();
        if shown.contains(WORK_UNDECLARED) && shown.contains("deny") {
            answer = self.hook_raw(agent, secret, payload);
        }
        assert!(!answer.to_string().contains("deny"), "{answer}");
        assert!(
            !answer.to_string().contains("permissionDecision"),
            "{answer}"
        );
        answer
    }
}

fn fixture(name: &str) -> Value {
    let text = fs::read_to_string(Path::new(FIXTURES).join(format!("{name}.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

async fn i295_startup_scenario(scenario: &str) {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    if scenario == "later_new_session" {
        let principal = hub
            .store
            .authenticate(&agent_token_digest(&secret).unwrap(), unix_now())
            .unwrap()
            .unwrap();
        let start: WorkStart = serde_json::from_value(json!({
            "title":"Synthetic default Work", "session":"default", "scope":["src/example.rs"]
        }))
        .unwrap();
        hub.store
            .work_start_auto(&principal.token_id, &start, unix_now())
            .unwrap();
    }
    let mut payload = fixture("claude-SessionStart");
    let first = hub.hook("claude", &secret, &payload);
    let first_text = text_at(&first, CLAUDE_CONTEXT);
    assert!(
        !first_text.contains("coordination unknown ("),
        "first start: {first}"
    );
    assert!(
        first_text.contains("native session ID"),
        "first start: {first}"
    );
    if scenario == "first_start" {
        return;
    }
    match scenario {
        "replay" => {}
        "resume" => payload["source"] = json!("resume"),
        "later_new_session" => {
            payload["session_id"] = json!("22222222-3333-4444-5555-666666666666")
        }
        _ => panic!("unknown synthetic scenario"),
    }
    let answer = hub.hook("claude", &secret, &payload);
    let text = text_at(&answer, CLAUDE_CONTEXT);
    assert!(
        !text.contains("coordination unknown ("),
        "{scenario}: {answer}"
    );
    assert!(text.contains("native session ID"), "{scenario}: {answer}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i295_first_start() {
    i295_startup_scenario("first_start").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i295_immediate_replay() {
    i295_startup_scenario("replay").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i295_resume() {
    i295_startup_scenario("resume").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i295_later_new_session_after_default_adoption() {
    i295_startup_scenario("later_new_session").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_preflight_explains_the_missing_work_identity() {
    let mut hub = Hub::start().await;
    let token = hub.agent();
    let (status, body) = hub.post(
        "/v1/hub/standup/preflight",
        Some(&token),
        br#"{"title":"Synthetic task","session":"synthetic-session"}"#,
    );
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"], "invalid_request");
    assert_eq!(body["recovery"]["action"], "provide_field");
    let fields = body["fields"].as_array().unwrap();
    for name in ["work_id", "source", "request_key"] {
        assert!(body["message"].as_str().unwrap().contains(name));
        assert!(
            fields
                .iter()
                .any(|field| field["name"] == name && field["problem"] == "missing")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_session_start_replay_and_resume_always_repeat_the_note() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let principal = hub
        .store
        .authenticate(&agent_token_digest(&secret).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    hub.store.revoke_agent_token(&principal.token_id).unwrap();
    let mut payload = fixture("claude-SessionStart");
    let client = reqwest::Client::new();
    for source in ["startup", "startup", "resume"] {
        payload["source"] = json!(source);
        let response = client
            .post(format!("http://{}/v1/hub/hook/claude", hub.address))
            .bearer_auth(&secret)
            .header("content-type", "application/json")
            .body(payload.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let reference = response.headers()["x-agentscm-correlation-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let answer: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        let text = text_at(&answer, CLAUDE_CONTEXT);
        assert!(text.contains("coordination unknown"), "{answer}");
        assert!(
            text.contains("sign in") || text.contains("Sign in"),
            "{answer}"
        );
        assert!(text.contains(&format!("ref {reference}")), "{answer}");
        assert!(!text.contains(&secret));
        assert!(text.chars().count() <= 9000);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthorized_callers_get_indistinguishable_envelopes() {
    let mut hub = Hub::start().await;
    let revoked = hub.agent();
    let principal = hub
        .store
        .authenticate(&agent_token_digest(&revoked).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    hub.store.revoke_agent_token(&principal.token_id).unwrap();
    let expired = "d".repeat(64);
    let expired_id = hub
        .store
        .insert_agent_token(&store::NewAgentToken {
            owner: principal.owner.id.clone(),
            name: "expired".into(),
            repository: REPO.into(),
            digest: agent_token_digest(&expired).unwrap(),
            expires_at: unix_now() + 3600,
            repository_id: None,
            read_only: false,
        })
        .unwrap();
    rusqlite::Connection::open(hub._dir.path().join("hub.sqlite"))
        .unwrap()
        .execute(
            "UPDATE hub_agent_tokens SET expires_at = ?1 WHERE id = ?2",
            rusqlite::params![unix_now() - 1, expired_id],
        )
        .unwrap();
    let disabled = hub.agent();
    let disabled_principal = hub
        .store
        .authenticate(&agent_token_digest(&disabled).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    hub.store
        .set_user_disabled(&disabled_principal.owner.id, true)
        .unwrap();
    let unknown = "e".repeat(64);
    let mut baseline = None;
    for token in [
        None,
        Some("not-a-token"),
        Some(unknown.as_str()),
        Some(revoked.as_str()),
        Some(expired.as_str()),
        Some(disabled.as_str()),
    ] {
        let (status, mut body) = hub.post("/v1/hub/standup/preflight", token, b"not-json");
        assert_eq!(status, 401);
        assert_eq!(body["error"], "unauthorized");
        body.as_object_mut().unwrap().remove("correlation_id");
        if let Some(expected) = &baseline {
            assert_eq!(&body, expected);
        } else {
            baseline = Some(body);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inaccessible_repository_resources_look_exactly_like_missing_resources() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let principal = hub
        .store
        .authenticate(&agent_token_digest(&secret).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    let (other_token, _) = mint_agent_token(
        &*hub.store,
        &principal.owner.id,
        "other",
        "other/hidden",
        3600,
    )
    .unwrap();
    let start: WorkStart = serde_json::from_value(json!({"title":"Synthetic hidden Work", "session":"hidden-session", "request_key":"hidden-request"})).unwrap();
    let hidden = hub
        .store
        .work_start(&other_token, &start, unix_now())
        .unwrap()
        .item;
    assert!(
        hub.store
            .work_get(&other_token, &hidden.id, unix_now())
            .unwrap()
            .is_some()
    );
    let client = reqwest::Client::new();
    let mut baseline = None;
    for work_id in ["wrk_00000000000000000000000000000000", &hidden.id] {
        let response = client
            .get(format!(
                "http://{}/v1/hub/standup/work/{work_id}",
                hub.address
            ))
            .bearer_auth(&secret)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        let reference = response.headers()["x-agentscm-correlation-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(agentscm::hub::error_envelope::valid_correlation_id(
            &reference
        ));
        let mut body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(body["correlation_id"], reference);
        assert_eq!(body["error"], "not_found");
        body.as_object_mut().unwrap().remove("correlation_id");
        if let Some(expected) = &baseline {
            assert_eq!(&body, expected);
        } else {
            baseline = Some(body);
        }
    }
}

fn text_at<'a>(answer: &'a Value, pointer: &str) -> &'a str {
    answer
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no {pointer} in {answer}"))
}

/// What a lone agent gets back for each fixture.
enum Expect {
    /// `{}`: nothing to say.
    Empty,
    /// Context (here: who else is working) at this JSON pointer.
    Context(&'static str),
    Allow,
    Generic(&'static str),
    /// The one-time note at this JSON pointer, where the host cannot refuse a session's first
    /// edit while it has declared no Work.
    Undeclared(&'static str),
    /// A shell call's end that reports what the hook client observed: context with this text,
    /// never a refusal.
    Observed(&'static str),
}

const CLAUDE_CONTEXT: &str = "/hookSpecificOutput/additionalContext";

fn expectations() -> Vec<(&'static str, Expect)> {
    use Expect::{Allow, Context, Empty, Generic, Observed, Undeclared};
    vec![
        ("claude-SessionStart", Context(CLAUDE_CONTEXT)),
        ("claude-PreToolUse-Edit", Empty),
        ("claude-PreToolUse-Write", Empty),
        ("claude-PreToolUse-NotebookEdit", Empty),
        ("claude-PreToolUse-Bash", Empty),
        // A wait is reported and never answered: the prompt or question still goes to the person.
        ("claude-PreToolUse-AskUserQuestion", Empty),
        ("claude-Notification", Empty),
        ("claude-PermissionRequest", Empty),
        ("claude-Stop", Empty),
        ("claude-SessionEnd", Empty),
        ("codex-SessionStart", Context(CLAUDE_CONTEXT)),
        ("claude-PostToolUse-Bash", Observed("already applied")),
        ("codex-PostToolUse-Bash", Observed("(missing_snapshot)")),
        ("codex-PreToolUse", Empty),
        ("codex-PermissionRequest", Empty),
        ("codex-Stop", Empty),
        ("codex-SessionEnd", Empty),
        ("cursor-sessionStart", Context("/additional_context")),
        ("cursor-preToolUse", Allow),
        ("cursor-postToolUse", Undeclared("/additional_context")),
        ("cursor-afterFileEdit", Empty),
        ("cursor-stop", Empty),
        ("cursor-sessionEnd", Empty),
        ("copilot-sessionStart", Context("/additionalContext")),
        ("copilot-preToolUse", Empty),
        ("copilot-postToolUse", Empty),
        ("copilot-notification", Empty),
        ("copilot-agentStop", Empty),
        ("copilot-sessionEnd", Empty),
        ("copilot-vscode-SessionStart", Context(CLAUDE_CONTEXT)),
        ("copilot-vscode-PreToolUse", Empty),
        ("generic-start", Generic("clean")),
        ("generic-check", Generic("clean")),
        ("generic-waiting", Generic("clean")),
        ("generic-done", Generic("clean")),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_fixture_gets_its_agents_output_shape() {
    let table = expectations();
    let mut on_disk: Vec<String> = fs::read_dir(FIXTURES)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = table
        .iter()
        .map(|(name, _)| format!("{name}.json"))
        .collect();
    listed.sort();
    assert_eq!(on_disk, listed, "every fixture needs an expectation");
    for (name, expect) in table {
        // A hub of its own, so the fixtures' shared file name never collides across cases.
        let mut hub = Hub::start().await;
        let secret = hub.agent();
        let agent = name.split('-').next().unwrap();
        let answer = hub.hook(agent, &secret, &fixture(name));
        match expect {
            Expect::Empty => assert_eq!(answer, json!({}), "{name}"),
            Expect::Context(pointer) => {
                let text = text_at(&answer, pointer);
                // The session ID and required rules lead a start context.
                assert!(text.starts_with("AgentSCM"), "{name}: {answer}");
                assert!(
                    agentscm::hub::agent_rules::carries_all(text),
                    "{name}: {answer}"
                );
                assert!(
                    text.contains("Once the user intent is known, call agentscm_start"),
                    "{name}: {answer}"
                );
                if pointer == CLAUDE_CONTEXT {
                    let event = fixture(name)["hook_event_name"].clone();
                    assert_eq!(
                        answer["hookSpecificOutput"]["hookEventName"], event,
                        "{name}"
                    );
                    assert_eq!(answer.as_object().unwrap().len(), 1, "{name}");
                }
            }
            Expect::Allow => assert_eq!(answer, json!({"permission": "allow"}), "{name}"),
            Expect::Undeclared(pointer) => {
                let text = text_at(&answer, pointer);
                // The refusal leads with the WORK_UNDECLARED code; its parts are checked in
                // `a_second_agent_on_the_same_file_is_refused_once` and
                // `hub_workflow::native_hook_adopts_default_work_and_enforces_its_lifecycle`.
                assert!(
                    text.lines().next().unwrap().contains(WORK_UNDECLARED),
                    "{name}: {answer}"
                );
                assert_eq!(answer.as_object().unwrap().len(), 1, "{name}");
            }
            Expect::Observed(needle) => {
                let text = text_at(&answer, CLAUDE_CONTEXT);
                assert!(text.contains(needle), "{name}: {answer}");
                assert_eq!(
                    answer["hookSpecificOutput"]["hookEventName"], "PostToolUse",
                    "{name}"
                );
                assert_eq!(answer.as_object().unwrap().len(), 1, "{name}");
            }
            Expect::Generic(status) => {
                assert_eq!(answer["status"], status, "{name}: {answer}");
                assert!(answer["text"].is_string(), "{name}: {answer}");
                if name == "generic-start" {
                    assert!(
                        answer["text"]
                            .as_str()
                            .unwrap()
                            .contains("Once the user intent is known, call agentscm_start"),
                        "{answer}"
                    );
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_register_one_session_and_done_releases_it() {
    let mut hub = Hub::start().await;
    let alice = hub.agent();
    let bob = hub.agent();
    hub.hook("claude", &alice, &fixture("claude-SessionStart"));
    hub.hook("claude", &alice, &fixture("claude-PreToolUse-Edit"));
    let live = hub.store.live_intents(REPO, unix_now(), 10).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].paths, ["src/widget.rs"]);
    assert_eq!(live[0].via, ["hook:claude"]);
    // The unseen session of another agent kind is an implicit start; a directory is never claimed.
    let answer = hub.hook(
        "generic",
        &bob,
        &json!({"event": "check", "cwd": "/r", "paths": ["/r/other/**", "/r/x.rs"]}),
    );
    assert_eq!(answer["status"], "clean");
    assert_eq!(
        hub.store.live_intents(REPO, unix_now(), 10).unwrap().len(),
        2
    );
    hub.hook("claude", &alice, &fixture("claude-SessionEnd"));
    let live = hub.store.live_intents(REPO, unix_now(), 10).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].via, ["hook:generic"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_end_leaves_held_work_active_without_claiming_completion() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let session = "11111111-2222-3333-4444-555555555555";
    let start_answer = hub.hook("claude", &secret, &fixture("claude-SessionStart"));
    assert!(start_answer.to_string().contains("actions need you"));
    assert!(start_answer.to_string().contains("Coverage incomplete"));
    assert!(
        start_answer
            .to_string()
            .contains("provider times shown separately")
    );
    let start_context = text_at(&start_answer, CLAUDE_CONTEXT);
    assert!(
        start_context.contains("AgentSCM shared work"),
        "{start_answer}"
    );
    assert!(
        !start_context.contains("Briefing delivery"),
        "{start_answer}"
    );

    let principal = hub
        .store
        .authenticate(&agent_token_digest(&secret).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    let started = hub
        .store
        .work_start_auto(
            &principal.token_id,
            &WorkStart {
                issue_choice: None,
                work_id: None,
                source: None,
                request_key: None,
                title: "implement hook handover".into(),
                description: None,
                session: session.into(),
                role: WorkRole::Lead,
                scope: vec!["src/hub/hook.rs".into()],
                branch: Some("feat/hooks".into()),
                worktree: None,
                next_action: Some("resume the hook tests".into()),
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
                preflight: None,
            },
            unix_now(),
        )
        .unwrap();
    assert_eq!(started.item.state, WorkState::Active);

    assert_eq!(
        hub.hook("claude", &secret, &fixture("claude-SessionEnd")),
        json!({})
    );
    // The end may be only the gap between two turns: Work whose hold is live goes on as it is,
    // with nothing written for it, and the end never invents completion.
    let ended = hub
        .store
        .work_get(&principal.token_id, &started.item.id, unix_now())
        .unwrap()
        .unwrap();
    assert_eq!(ended.state, WorkState::Active);
    assert_eq!(ended.updated_seq, started.item.updated_seq);
    assert_eq!(ended.next_action.as_deref(), Some("resume the hook tests"));

    // The same session starts again and ends its next turn: still nothing is written.
    hub.hook("claude", &secret, &fixture("claude-SessionStart"));
    hub.hook("claude", &secret, &fixture("claude-SessionEnd"));
    let repeated = hub
        .store
        .work_get(&principal.token_id, &started.item.id, unix_now())
        .unwrap()
        .unwrap();
    assert_eq!(repeated.state, WorkState::Active);
    assert_eq!(repeated.updated_seq, started.item.updated_seq);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_end_naming_no_session_pauses_no_work() {
    use agentscm::hub::work_lifecycle::LEASE_SECONDS;
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let principal = hub
        .store
        .authenticate(&agent_token_digest(&secret).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    // Started two leases ago and never renewed: its hold has lapsed, so no lease bounds a wait
    // and the end of its own session pauses it. The lease clock is walked there in steps, as a
    // running Hub observes it, so no reservation is given grace for a gap.
    let begun = unix_now() - 2 * LEASE_SECONDS;
    hub.store.observe_lease_clock(begun).unwrap();
    let started = hub
        .store
        .work_start_auto(
            &principal.token_id,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: None,
                title: "implement hook handover".into(),
                description: None,
                session: "default".into(),
                role: WorkRole::Lead,
                scope: vec!["src/hub/hook.rs".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
                issue_choice: None,
                preflight: None,
            },
            begun,
        )
        .unwrap();
    for step in 1..=4 {
        hub.store
            .observe_lease_clock(begun + step * LEASE_SECONDS / 2)
            .unwrap();
    }
    let state = |hub: &Hub| {
        hub.store
            .work_get(&principal.token_id, &started.item.id, unix_now())
            .unwrap()
            .unwrap()
            .state
    };

    // The session an omitted name resolves to is not the call's own, so its Work is kept.
    hub.hook("generic", &secret, &json!({"event": "done"}));
    assert_eq!(state(&hub), WorkState::Active);
    hub.hook(
        "generic",
        &secret,
        &json!({"event": "done", "session": "default"}),
    );
    assert_eq!(state(&hub), WorkState::Paused);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_start_flags_records_omitted_from_a_complete_preview() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let principal = hub
        .store
        .authenticate(&agent_token_digest(&secret).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    for index in 0..6 {
        hub.store
            .work_start_auto(
                &principal.token_id,
                &WorkStart {
                    issue_choice: None,
                    work_id: None,
                    source: None,
                    request_key: None,
                    title: format!("unfinished goal {index}"),
                    description: None,
                    session: format!("preview-session-{index}"),
                    role: WorkRole::Lead,
                    scope: vec![format!("src/preview-{index}.rs")],
                    branch: None,
                    worktree: None,
                    next_action: None,
                    depends_on: Vec::new(),
                    related_work: Vec::new(),
                    takeover_generation: None,
                    preflight: None,
                },
                unix_now(),
            )
            .unwrap();
    }
    let preview = hub
        .store
        .work_briefing_preview_for(&principal.token_id, None, 20, unix_now())
        .unwrap();
    assert!(preview.complete);
    assert_eq!(preview.items.len(), 6);

    let answer = hub.hook("claude", &secret, &fixture("claude-SessionStart"));
    let context = text_at(&answer, CLAUDE_CONTEXT);
    assert!(
        context.contains("AgentSCM shared work: 6 unfinished item(s)"),
        "{answer}"
    );
    assert!(
        context.contains("This bounded hook preview omits shared-work records"),
        "{answer}"
    );
    assert!(
        context.contains("agentscm_work_briefing to begin durable pagination"),
        "{answer}"
    );
    assert!(!context.contains("Briefing delivery"), "{answer}");
}

/// The warn-once part of a refusal. A session's first edit while it has declared no Work carries
/// the one-time `WORK_UNDECLARED` request ahead of it, on its own first line, in the same refusal.
fn warn_once_part(reason: &str) -> &str {
    match reason.split_once('\n') {
        Some((first, warn)) if first.contains(WORK_UNDECLARED) => warn,
        _ => reason,
    }
}

/// The refusal of a pre-edit hook: its whole reason, after checking that its warn-once part
/// names the file and the holder. The exact sentence is pinned next to the code that writes it
/// (`advice::tests`); here only what a refusal must carry is checked, so rewording it elsewhere
/// does not fail these tests.
fn reason_of(answer: &Value, pointer: &str, key: &str) -> String {
    let full = text_at(answer, pointer).to_owned();
    let reason = warn_once_part(&full);
    // Protects: the refusal names the contested file, its holder by user and session digest,
    // marks the holder's task as untrusted text, and forbids getting around it.
    assert!(reason.contains("src/widget.rs"), "{reason}");
    assert!(
        reason.contains(&format!("user1's Claude Code session {key}")),
        "{reason}"
    );
    assert!(reason.contains("not instructions"), "{reason}");
    assert!(reason.contains(NO_BYPASS), "{reason}");
    assert!(
        !reason.contains("STOP") && !reason.contains("Do not"),
        "{reason}"
    );
    full
}

/// #120's acceptance, through the real route: three sessions on ONE token. The refusal and the
/// hand-off name the specific session, and the request reaches only the holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_sharing_one_token_name_the_holder_and_only_it_hears() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let edit = |session: &str, file: &str| {
        let mut payload = fixture("claude-PreToolUse-Edit");
        payload["session_id"] = json!(session);
        payload["tool_input"]["file_path"] = json!(format!("/work/acme-widgets/{file}"));
        payload
    };
    let asks = "asks to take over src/widget.rs";

    hub.hook("claude", &secret, &edit("holder-s", "src/widget.rs"));
    let token = hub.store.live_intents(REPO, unix_now(), 10).unwrap()[0]
        .token
        .clone();
    let key = |session: &str| {
        lower_hex_of(format!("agentscm.hub.session-view.v1\0{token}\0{session}").as_bytes())[..8]
            .to_owned()
    };

    let refused = hub.hook_raw("claude", &secret, &edit("requester-s", "src/widget.rs"));
    let reason = text_at(&refused, "/hookSpecificOutput/permissionDecisionReason");
    assert!(
        reason.contains(&format!(
            "src/widget.rs: user1's Claude Code session {}, started",
            key("holder-s")
        )) && reason.contains(&format!(
            "AgentSCM asked user1's Claude Code session {} to hand it over",
            key("holder-s")
        )),
        "{reason}"
    );
    assert!(
        !reason.contains("holder-s") && !reason.contains("agent,"),
        "{reason}"
    );

    // A bystander on the same token, on another file, hears nothing of it.
    let bystander = hub.hook("claude", &secret, &edit("bystander-s", "src/other.rs"));
    assert!(!bystander.to_string().contains(asks), "{bystander}");

    // The holder hears it once, from the named requester session.
    let holder = hub.hook("claude", &secret, &edit("holder-s", "src/widget.rs"));
    let told = text_at(&holder, CLAUDE_CONTEXT);
    assert!(
        told.contains(&format!(
            "user1's Claude Code session {} {asks}",
            key("requester-s")
        )),
        "{told}"
    );
    assert!(!told.contains("requester-s"), "{told}");
    let again = hub.hook("claude", &secret, &edit("holder-s", "src/widget.rs"));
    assert!(!again.to_string().contains(asks), "{again}");
    let bystander = hub.hook("claude", &secret, &edit("bystander-s", "src/other.rs"));
    assert!(!bystander.to_string().contains(asks), "{bystander}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_agent_on_the_same_file_is_refused_once() {
    let mut hub = Hub::start().await;
    let first = hub.agent();
    hub.hook("claude", &first, &fixture("claude-PreToolUse-Write"));
    // The holder is named by the digest of its session, never by the session id or token label.
    let held = hub.store.live_intents(REPO, unix_now(), 10).unwrap();
    let digest = lower_hex_of(
        format!(
            "agentscm.hub.session-view.v1\0{}\0{}",
            held[0].token,
            held[0].session.as_deref().unwrap()
        )
        .as_bytes(),
    );
    let key = digest[..8].to_owned();
    let raw_session = held[0].session.clone().unwrap();

    let [second, x1, v1, v2, c1, c2, c3, g1] = [(); 8].map(|()| hub.agent());
    // Claude Code: the documented deny, with the reason and nothing else.
    let claude = hub.hook_raw("claude", &second, &fixture("claude-PreToolUse-Edit"));
    let reason = reason_of(
        &claude,
        "/hookSpecificOutput/permissionDecisionReason",
        &key,
    );
    assert!(
        reason.contains("holder read coverage complete: false"),
        "{reason}"
    );
    // This session has declared no Work, so the one-time WORK_UNDECLARED request leads the same
    // refusal. Its wording is written in `hook.rs`; also checked in
    // `hub_workflow::native_hook_adopts_default_work_and_enforces_its_lifecycle`.
    // Protects: it says the edit was not applied, names the repository, the call that declares
    // Work, and forbids getting around it.
    let head = reason.lines().next().unwrap();
    assert!(
        head.contains(WORK_UNDECLARED)
            && head.contains("not applied")
            && head.contains(REPO)
            && head.contains("agentscm_start")
            && head.contains(NO_BYPASS),
        "{reason}"
    );
    // Protects: with one holder, the Hub already asked that holder to hand the file over, so
    // the holder's digest appears twice (as holder and as the one asked) and no ask option is
    // offered.
    let warn = warn_once_part(&reason);
    assert!(warn.matches(key.as_str()).count() >= 2, "{warn}");
    assert!(!warn.contains("agentscm_ask"), "{warn}");
    assert_eq!(
        claude,
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
            "permissionDecision": "deny", "permissionDecisionReason": reason}})
    );
    // The one-time request names the caller's own session, for its start call; the holder is
    // named only by its digest. Both fixtures share one session id.
    assert!(
        !warn_once_part(&reason).contains(&raw_session) && !reason.contains("agent,"),
        "{reason}"
    );
    // The same edit again goes through, quietly.
    let retry = hub.hook("claude", &second, &fixture("claude-PreToolUse-Edit"));
    assert_eq!(retry, json!({}));
    assert_eq!(
        hub.hook("claude", &second, &fixture("claude-PreToolUse-Edit")),
        json!({})
    );

    // Codex: a patch of two files is refused whole; the reason names the contested one.
    let codex = hub.hook_raw("codex", &x1, &fixture("codex-PreToolUse"));
    let reason = reason_of(&codex, "/hookSpecificOutput/permissionDecisionReason", &key);
    // Protects: the holder's refusal names only the contested file, not the patch's uncontested
    // one (the one-time WORK_UNDECLARED line ahead of it lists every path, for the start call).
    assert!(
        !warn_once_part(&reason).contains("src/gadget.rs"),
        "{reason}"
    );
    assert_eq!(codex["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(codex["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert!(
        codex["hookSpecificOutput"]
            .get("additionalContext")
            .is_none()
    );
    assert!(
        hub.store
            .live_intents(REPO, unix_now(), 10)
            .unwrap()
            .iter()
            .all(|i| { !i.paths.contains(&"src/gadget.rs".to_owned()) })
    );
    assert_eq!(
        hub.hook("codex", &x1, &fixture("codex-PreToolUse")),
        json!({})
    );

    // Copilot in VS Code: the reason goes to the user and, as context, to the model.
    let vscode = hub.hook_raw("copilot", &v1, &fixture("copilot-vscode-PreToolUse"));
    let reason = reason_of(
        &vscode,
        "/hookSpecificOutput/permissionDecisionReason",
        &key,
    );
    assert_eq!(
        vscode,
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
            "permissionDecision": "deny", "permissionDecisionReason": reason,
            "additionalContext": reason},
            "permissionDecision": "deny", "permissionDecisionReason": reason})
    );
    assert_eq!(
        hub.hook("copilot", &v1, &fixture("copilot-vscode-PreToolUse")),
        json!({})
    );

    // Copilot CLI: camelCase preToolUse refuses; its postToolUse makes no second check.
    let cli = hub.hook_raw("copilot", &v2, &fixture("copilot-preToolUse"));
    let reason = reason_of(&cli, "/permissionDecisionReason", &key);
    assert_eq!(
        cli,
        json!({"permissionDecision": "deny", "permissionDecisionReason": reason})
    );
    let post = hub.hook("copilot", &v2, &fixture("copilot-postToolUse"));
    assert_eq!(post, json!({}));
    assert_eq!(
        hub.hook("copilot", &v2, &fixture("copilot-preToolUse")),
        json!({})
    );

    // Cursor never refuses: preToolUse is exactly "allow", and the note comes after the edit.
    let pre = hub.hook("cursor", &c1, &fixture("cursor-preToolUse"));
    assert_eq!(pre, json!({"permission": "allow"}));
    let after = hub.hook("cursor", &c2, &fixture("cursor-afterFileEdit"));
    assert_eq!(after, json!({}));
    let post = hub.hook("cursor", &c3, &fixture("cursor-postToolUse"));
    // Protects: a held-file note leads with the file and how to ask its holder for it, and names
    // that holder by kind and digest; the wording is pinned in `advice::tests`.
    let held_note = |note: &str| {
        let head = note.lines().next().unwrap();
        assert!(
            head.contains("agentscm_ask") && head.contains("\"src/widget.rs\""),
            "{note}"
        );
        let holder = format!("user1's Claude Code session {key}");
        assert!(note.contains(&holder), "{note}");
    };
    held_note(text_at(&post, "/additional_context"));

    // Generic hooks cannot refuse either: status and text.
    let generic = hub.hook("generic", &g1, &fixture("generic-check"));
    assert_eq!(generic["status"], "stop");
    held_note(text_at(&generic, "/text"));

    // The first agent learns of those that went ahead, as a note.
    let beat = hub.hook("claude", &first, &fixture("claude-PreToolUse-Write"));
    let note = text_at(&beat, CLAUDE_CONTEXT);
    // Protects: the holder's note names its file but, as a notice, offers no ask.
    let head = note.lines().next().unwrap();
    assert!(
        head.contains("src/widget.rs") && !note.contains("agentscm_ask"),
        "{note}"
    );
    // Each notice names the session that went ahead, by kind and digest, not the token label.
    assert!(
        note.contains("- user2's Claude Code session ")
            && note.contains("- user8's Cursor session "),
        "{note}"
    );
    // Released by done, the file is free again.
    hub.hook("claude", &first, &fixture("claude-SessionEnd"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_needs_a_live_hub_and_the_failure_note_comes_once() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let mut edit = fixture("claude-PreToolUse-Edit");
    edit["session_id"] = json!("failure-note-session");
    let body = edit.to_string();
    // A failure never refuses; the first says coordination is unknown, later ones are silent.
    let (_, first) = hub.post(
        "/v1/hub/hook/claude",
        Some("agentscm_nope"),
        body.as_bytes(),
    );
    assert!(
        text_at(&first, CLAUDE_CONTEXT).starts_with("AgentSCM: coordination unknown ("),
        "{first}"
    );
    assert!(!first.to_string().contains("deny"), "{first}");
    for _ in 0..2 {
        let (status, again) = hub.post(
            "/v1/hub/hook/claude",
            Some("agentscm_nope"),
            body.as_bytes(),
        );
        assert_eq!((status, again), (200, json!({})));
    }
    // Another session of the same bad token is told once too, and a good call is unaffected.
    edit["session_id"] = json!("failure-note-other");
    let (_, other) = hub.post(
        "/v1/hub/hook/claude",
        Some("agentscm_nope"),
        edit.to_string().as_bytes(),
    );
    assert!(
        text_at(&other, CLAUDE_CONTEXT).contains("coordination unknown"),
        "{other}"
    );
    assert_eq!(hub.hook("claude", &secret, &edit), json!({}));
    // Failures with no session id (an unreadable body) always say so.
    for _ in 0..2 {
        let (_, unreadable) = hub.post("/v1/hub/hook/claude", Some(&secret), b"not json");
        assert!(text_at(&unreadable, "/systemMessage").contains("coordination unknown"));
    }
}

/// The pointer at which an agent's failure text appears, per fixture.
const FAILURES: [(&str, &str, &str); 5] = [
    ("claude", "claude-PreToolUse-Edit", CLAUDE_CONTEXT),
    ("codex", "codex-PreToolUse", CLAUDE_CONTEXT),
    ("cursor", "cursor-sessionStart", "/additional_context"),
    ("copilot", "copilot-sessionStart", "/additionalContext"),
    ("generic", "generic-check", "/text"),
];
/// Where the text goes when the body could not be read and so the event is not known.
const UNREADABLE: [(&str, &str); 5] = [
    ("claude", "/systemMessage"),
    ("codex", "/systemMessage"),
    // Cursor's body may have been a permission event: exactly "allow", with no text.
    ("cursor", ""),
    ("copilot", "/systemMessage"),
    ("generic", "/text"),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoded_repository_guards_are_judged_identically_to_raw_guards() {
    for (raw, encoded, accepted) in [
        (REPO, "acme%2Fwidgets", true),
        ("other/widgets", "other%2Fwidgets", false),
    ] {
        let mut first = None;
        for guard in [raw, encoded] {
            let mut hub = Hub::start().await;
            let secret = hub.agent();
            let (status, reply) = hub.post(
                &format!("/v1/hub/hook/claude?repo={guard}"),
                Some(&secret),
                fixture("claude-PreToolUse-Edit").to_string().as_bytes(),
            );
            assert_eq!(status, 200);
            assert_eq!(
                reply.to_string().contains("coordination unknown ("),
                !accepted,
                "{guard}: {reply}"
            );
            let live = hub.store.live_intents(REPO, unix_now(), 10).unwrap();
            assert_eq!(live.len(), usize::from(accepted), "{guard}");
            if accepted {
                assert_eq!(live[0].paths, ["src/widget.rs"]);
            }
            // Each failure carries its own reference; the rest of the answer must be identical.
            let same =
                |reply: &Value| crate::hub_error_assert::blank_references(&reply.to_string());
            assert_eq!(same(first.get_or_insert(reply.clone())), same(&reply));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_are_200s_that_say_coordination_is_unknown() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let unknown = |answer: &Value, pointer: &str| {
        let text = text_at(answer, pointer);
        assert!(
            text.starts_with("AgentSCM: coordination unknown ("),
            "{answer}"
        );
        assert!(!text.contains(&secret), "{answer}");
    };
    for (agent, name, pointer) in FAILURES {
        let path = format!("/v1/hub/hook/{agent}");
        let body = fixture(name).to_string();
        for (label, token, query) in [
            ("no token", None, ""),
            ("bad token", Some("agentscm_nope"), ""),
            ("wrong repo", Some(secret.as_str()), "?repo=other/repo"),
        ] {
            let (status, answer) = hub.post(&format!("{path}{query}"), token, body.as_bytes());
            assert_eq!(status, 200, "{agent} {label}");
            unknown(&answer, pointer);
        }
        // The token's own repository is accepted.
        let (_, ok) = hub.post(
            &format!("{path}?repo={REPO}"),
            Some(&secret),
            body.as_bytes(),
        );
        // The required rules mention unknown coordination; only the failure note has a reason.
        assert!(
            !ok.to_string().contains("coordination unknown ("),
            "{agent}: {ok}"
        );
    }
    for (agent, pointer) in UNREADABLE {
        let path = format!("/v1/hub/hook/{agent}");
        let oversize = vec![b'a'; 1024 * 1024 + 1];
        for (label, body) in [
            ("garbage", &b"not json"[..]),
            ("empty", b""),
            ("array", b"[1,2]"),
            ("oversize", &oversize),
        ] {
            let (status, answer) = hub.post(&path, Some(&secret), body);
            assert_eq!(status, 200, "{agent} {label}");
            if pointer.is_empty() {
                assert_eq!(answer, json!({"permission": "allow"}), "{agent} {label}");
            } else {
                unknown(&answer, pointer);
            }
        }
    }
    // Cursor's preToolUse is a permission hook: always exactly "allow".
    let pre = fixture("cursor-preToolUse").to_string();
    for token in [None, Some("agentscm_nope"), Some(secret.as_str())] {
        let (status, answer) =
            hub.post("/v1/hub/hook/cursor?repo=other/repo", token, pre.as_bytes());
        assert_eq!((status, answer), (200, json!({"permission": "allow"})));
    }
    // Events without an output channel still answer with valid JSON.
    let end = fixture("claude-SessionEnd").to_string();
    let (status, answer) = hub.post("/v1/hub/hook/claude", None, end.as_bytes());
    assert_eq!((status, answer), (200, json!({})));
    // A generic body with no usable event is unknown, not an error.
    let (status, answer) = hub.post("/v1/hub/hook/generic", Some(&secret), br#"{"event":"x"}"#);
    assert_eq!(status, 200);
    unknown(&answer, "/text");
    assert_eq!(answer["status"], "unknown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_agents_are_not_found() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    for agent in ["gemini", "Claude", ""] {
        let (status, _) = hub.post(&format!("/v1/hub/hook/{agent}"), Some(&secret), b"{}");
        assert_eq!(status, 404, "{agent:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_root_header_sets_what_paths_are_relative_to() {
    let mut hub = Hub::start().await;
    let mut edit = fixture("claude-PreToolUse-Edit");
    edit["cwd"] = json!("/work/acme-widgets/src");
    edit["tool_input"]["file_path"] = json!("widget.rs");
    let body = edit.to_string();
    let [rooted, invalid] = [(); 2].map(|()| hub.agent());
    let paths = |secret: &str, header: &str| {
        let (status, _) =
            hub.post_with("/v1/hub/hook/claude", Some(secret), header, body.as_bytes());
        assert_eq!(status, 200);
        let token = hub.store.live_intents(REPO, unix_now(), 10).unwrap();
        let mut all: Vec<_> = token.into_iter().map(|i| i.paths).collect();
        all.sort();
        all
    };
    assert_eq!(
        paths(&rooted, "X-Agentscm-Root: /work/acme-widgets\r\n"),
        [["src/widget.rs"]]
    );
    // A relative root is ignored, so the working directory is the root again.
    assert_eq!(
        paths(&invalid, "X-Agentscm-Root: work\r\n"),
        [
            vec!["src/widget.rs".to_owned()],
            vec!["widget.rs".to_owned()]
        ]
    );
}

/// A request reaches the session the person types in. A subagent of that session, whose
/// context ends when it returns, gets a pointer instead and is never held for a reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_reach_the_prompted_session_and_subagents_get_a_pointer() {
    let mut hub = Hub::start().await;
    let lead = hub.agent();
    let other = hub.agent();
    let token_id = |secret: &str| {
        hub.store
            .authenticate(&agent_token_digest(secret).unwrap(), unix_now())
            .unwrap()
            .unwrap()
            .token_id
    };
    let start = |key: &str, session: &str| WorkStart {
        issue_choice: None,
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: key.into(),
        description: None,
        session: session.into(),
        role: WorkRole::Lead,
        scope: vec!["src/widget.rs".into()],
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: Vec::new(),
        related_work: Vec::new(),
        takeover_generation: None,
        preflight: None,
    };
    // The session of the Claude fixtures.
    let session = "11111111-2222-3333-4444-555555555555";
    hub.store
        .work_start(&token_id(&lead), &start("hold-widget", session), unix_now())
        .unwrap();
    let request = hub
        .store
        .work_start(
            &token_id(&other),
            &start("needs-widget", "other-session"),
            unix_now(),
        )
        .unwrap()
        .coordination_request
        .expect("an overlapping start queues a request")
        .id;

    let mut prompt = json!({"hook_event_name":"UserPromptSubmit", "session_id":session,
        "cwd":"/work/acme-widgets", "prompt":"not read"});
    let answer = hub.hook("claude", &lead, &prompt);
    assert_eq!(
        answer.pointer("/hookSpecificOutput/hookEventName"),
        Some(&json!("UserPromptSubmit")),
        "{answer}"
    );
    assert!(
        text_at(&answer, CLAUDE_CONTEXT).contains(&request),
        "{answer}"
    );
    // The requester has nothing to answer, so its prompts stay quiet.
    prompt["session_id"] = json!("other-session");
    assert_eq!(hub.hook("claude", &other, &prompt), json!({}));

    // The lead's own edit is still held until it answers.
    let edit = fixture("claude-PreToolUse-Write");
    let refused = hub.hook_raw("claude", &lead, &edit);
    assert!(
        refused.to_string().contains("needs your response"),
        "{refused}"
    );

    let mut subagent = edit.clone();
    subagent["agent_id"] = json!("a1b2c3");
    subagent["agent_type"] = json!("general-purpose");
    let answer = hub.hook("claude", &lead, &subagent);
    let context = text_at(&answer, CLAUDE_CONTEXT);
    assert!(context.contains("wait for your lead session"), "{answer}");
    assert!(!context.contains(&request), "{answer}");
}

/// A tool call that edits nothing still renews the lead session's lease, silently, so a busy
/// agent that only reads and runs commands keeps its hold. Another session renews nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_tool_call_renews_only_its_own_sessions_lease() {
    use agentscm::hub::work_lifecycle::LEASE_SECONDS;
    use agentscm::hub::work_transition::{OperationalSelect, WorkLifecycleReader};
    let mut hub = Hub::start().await;
    let lead = hub.agent();
    let token = hub
        .store
        .authenticate(&agent_token_digest(&lead).unwrap(), unix_now())
        .unwrap()
        .unwrap()
        .token_id;
    let session = "11111111-2222-3333-4444-555555555555";
    // Started ten minutes ago, so its lease ends ten minutes from now.
    let started = unix_now() - 600;
    let item = hub
        .store
        .work_start(
            &token,
            &WorkStart {
                issue_choice: None,
                work_id: None,
                source: None,
                request_key: Some("busy".into()),
                title: "busy".into(),
                description: None,
                session: session.into(),
                role: WorkRole::Lead,
                scope: vec!["src/widget.rs".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
                preflight: None,
            },
            started,
        )
        .unwrap()
        .item;
    let expiry = |hub: &Hub| {
        hub.store
            .work_operational(
                &WorkLifecycleReader::Token(&token),
                &OperationalSelect::One(&item.id),
                unix_now(),
            )
            .unwrap()
            .work[0]
            .activity
            .lease_expires_at
            .unwrap()
    };
    assert_eq!(expiry(&hub), started + LEASE_SECONDS);
    let other = json!({"hook_event_name":"PostToolUse", "session_id":"another-session",
        "tool_name":"Bash", "cwd":"/work/acme-widgets"});
    assert_eq!(hub.hook("claude", &lead, &other), json!({}));
    assert_eq!(expiry(&hub), started + LEASE_SECONDS);
    let before = unix_now();
    let bash = json!({"hook_event_name":"PreToolUse", "session_id":session,
        "tool_name":"Bash", "tool_input":{"command":"cargo test"}, "cwd":"/work/acme-widgets"});
    assert_eq!(hub.hook("claude", &lead, &bash), json!({}));
    assert!(expiry(&hub) >= before + LEASE_SECONDS);
}

/// A shell call's end reports what the hook client observed. Files it changed are applied, so
/// a session with no Work is told to declare it every time, never that anything was refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_shell_changes_without_work_ask_for_a_start_every_time() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let changed = fixture("claude-PostToolUse-Bash");
    let ask = "This session has no Work: call `agentscm_start` with these paths now";
    for _ in 0..2 {
        let answer = hub.hook_raw("claude", &secret, &changed);
        let text = text_at(&answer, CLAUDE_CONTEXT);
        assert!(
            text.contains("AgentSCM observed these files change during this shell call (already applied; another process may also have changed them): [\"src/gadget.rs\",\"src/widget.rs\"]"),
            "{answer}"
        );
        assert!(text.contains(ask), "{answer}");
        assert!(
            !text.contains("not applied") && !text.contains(WORK_UNDECLARED),
            "{answer}"
        );
        assert!(
            !answer.to_string().contains("permissionDecision"),
            "{answer}"
        );
        assert_eq!(
            answer["hookSpecificOutput"]["hookEventName"], "PostToolUse",
            "{answer}"
        );
    }
    // The observed paths are held as this session's intents, as a check after the edit does.
    let held = hub.store.live_intents(REPO, unix_now(), 10).unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].paths, ["src/gadget.rs", "src/widget.rs"]);
    assert_eq!(held[0].via, ["hook:claude"]);

    // A subagent asks its parent to start the Work.
    let mut child = changed.clone();
    child["agent_id"] = json!("sub-1");
    child["agent_type"] = json!("general-purpose");
    let answer = hub.hook_raw("claude", &secret, &child);
    let text = text_at(&answer, CLAUDE_CONTEXT);
    assert!(
        text.contains("Tell the session that started you to call `agentscm_start` with these paths; do not start Work yourself."),
        "{answer}"
    );
    assert!(!text.contains(ask), "{answer}");

    // A failed call that changed files is read the same way, on its own event.
    let mut failed = changed.clone();
    failed["hook_event_name"] = json!("PostToolUseFailure");
    let answer = hub.hook_raw("claude", &secret, &failed);
    assert_eq!(
        answer["hookSpecificOutput"]["hookEventName"], "PostToolUseFailure",
        "{answer}"
    );
    assert!(text_at(&answer, CLAUDE_CONTEXT).contains(ask), "{answer}");

    // The client could not tell: no path check, only the request for one.
    let mut unknown = changed.clone();
    unknown["observed"] = json!({"outcome": "unknown", "reason": "pre_snapshot_failed"});
    let answer = hub.hook_raw("claude", &secret, &unknown);
    let text = text_at(&answer, CLAUDE_CONTEXT);
    assert_eq!(
        text,
        "AgentSCM could not observe which files this shell call changed (pre_snapshot_failed). If it changed files, call `agentscm_check` with those paths."
    );
    // A field that is not what the client sends is read as unknown, never as a change.
    for bad in [
        json!({"outcome": "changed", "paths": ["../outside"], "truncated": false}),
        json!({"outcome": "changed", "paths": [], "truncated": false}),
        json!({"outcome": "unknown", "reason": "made-up"}),
        json!("changed"),
    ] {
        let mut malformed = changed.clone();
        malformed["observed"] = bad.clone();
        let answer = hub.hook_raw("claude", &secret, &malformed);
        let text = text_at(&answer, CLAUDE_CONTEXT);
        assert!(text.contains("(invalid)"), "{bad}: {answer}");
        assert!(!text.contains("agentscm_start"), "{bad}: {answer}");
    }
    // Nothing observed: silent, as before.
    let mut quiet = changed.clone();
    quiet.as_object_mut().unwrap().remove("observed");
    assert_eq!(hub.hook_raw("claude", &secret, &quiet), json!({}));
    quiet["hook_event_name"] = json!("PostToolUseFailure");
    assert_eq!(hub.hook_raw("claude", &secret, &quiet), json!({}));
}

/// The files a shell call changed are checked like an edit after the fact: another agent's hold
/// on one is named, and the call's end brings the lease back from its in-flight length, for a
/// failed call as for a finished one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_shell_changes_name_a_holder_and_a_failed_call_still_ends() {
    use agentscm::hub::work_lifecycle::{IN_FLIGHT_CAP_SECONDS, LEASE_SECONDS};
    use agentscm::hub::work_transition::{OperationalSelect, WorkLifecycleReader};
    let mut hub = Hub::start().await;
    let holder = hub.agent();
    let lead = hub.agent();
    hub.hook("claude", &holder, &fixture("claude-PreToolUse-Write"));
    let token = hub
        .store
        .authenticate(&agent_token_digest(&lead).unwrap(), unix_now())
        .unwrap()
        .unwrap()
        .token_id;
    let session = "11111111-2222-3333-4444-555555555555";
    let item = hub
        .store
        .work_start(
            &token,
            &WorkStart {
                issue_choice: None,
                work_id: None,
                source: None,
                request_key: Some("shell".into()),
                title: "shell".into(),
                description: None,
                session: session.into(),
                role: WorkRole::Lead,
                scope: vec!["src/widget.rs".into(), "src/gadget.rs".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
                preflight: None,
            },
            unix_now(),
        )
        .unwrap()
        .item;
    let expiry = |hub: &Hub| {
        hub.store
            .work_operational(
                &WorkLifecycleReader::Token(&token),
                &OperationalSelect::One(&item.id),
                unix_now(),
            )
            .unwrap()
            .work[0]
            .activity
            .lease_expires_at
            .unwrap()
    };
    let before = unix_now();
    hub.hook("claude", &lead, &fixture("claude-PreToolUse-Bash"));
    assert!(expiry(&hub) >= before + IN_FLIGHT_CAP_SECONDS);
    let mut failed = fixture("claude-PostToolUse-Bash");
    failed["hook_event_name"] = json!("PostToolUseFailure");
    failed["observed"] =
        json!({"outcome": "changed", "paths": ["src/widget.rs"], "truncated": false});
    let answer = hub.hook_raw("claude", &lead, &failed);
    let text = text_at(&answer, CLAUDE_CONTEXT);
    assert!(
        text.starts_with("AgentSCM observed these files change"),
        "{answer}"
    );
    // The holder is named by kind and digest, as a held-file note after an edit names it.
    assert!(text.contains("user1's Claude Code session"), "{answer}");
    assert!(text.contains("src/widget.rs"), "{answer}");
    // This session has Work covering the files, so it is not asked to start any.
    assert!(!text.contains("This session has no Work"), "{answer}");
    assert!(!text.contains("not applied"), "{answer}");
    assert!(expiry(&hub) <= unix_now() + LEASE_SECONDS, "call not ended");
}

/// The hook's side of answering a permission prompt from the Hub. A request needs its session's
/// permission wait, is offered only once the host's own notification confirms a prompt is on
/// screen, and ends when the session moves on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permission_request_needs_its_wait_and_the_hosts_notice_and_ends_with_the_wait() {
    let mut hub = Hub::start().await;
    let unbound = hub.agent();
    // Bound to the repository's id, as a connected checkout's token is.
    let bound = |user_id: i64| {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id,
        };
        let login = format!("bound{user_id}");
        let user = hub.store.upsert_user(&identity, &login).unwrap();
        mint_bound_agent_token(&*hub.store, &user.id, "agent", REPO, 4242, 3600, false)
            .unwrap()
            .1
            .expose()
            .to_owned()
    };
    let (secret, other) = (bound(101), bound(102));
    let prompt = fixture("claude-PermissionRequest");
    let session = prompt["session_id"].clone();
    let hash = "a".repeat(64);
    let register = json!({"session": session, "tool": "Bash", "display": "cargo publish",
        "complete": true, "input_hash": hash});
    let call = |route: &str, secret: &str, body: &Value| {
        hub.post(
            &format!("/v1/hub/permission/{route}"),
            Some(secret),
            body.to_string().as_bytes(),
        )
    };
    // No permission wait yet, so there is nothing to answer.
    let (status, refused) = call("register", &secret, &register);
    assert_eq!(
        (status, refused["reason"].as_str()),
        (409, Some("not_waiting")),
        "{refused}"
    );
    hub.hook("claude", &secret, &prompt);
    // Another token has no such wait. A token with no repository id, a bad hash and a field the
    // Hub does not take are refused.
    assert_eq!(call("register", &other, &register).0, 409);
    assert_eq!(call("register", &unbound, &register).0, 400);
    let mut bad = register.clone();
    bad["input_hash"] = json!("short");
    assert_eq!(call("register", &secret, &bad).0, 400);
    let mut extra = register.clone();
    extra["tool_input"] = json!({"command": "cargo publish"});
    assert_eq!(call("register", &secret, &extra).0, 400);

    let (status, made) = call("register", &secret, &register);
    assert_eq!(status, 200, "{made}");
    assert_eq!(made["state"], "unconfirmed", "{made}");
    assert_eq!(made["input_hash"], json!(hash));
    let id = made["request_id"].clone();
    let poll = json!({"request_id": id, "input_hash": hash, "wait_seconds": 0});
    let state = |secret: &str| {
        let (status, polled) = call("poll", secret, &poll);
        (status, polled["state"].as_str().map(str::to_owned))
    };
    assert_eq!(state(&secret), (200, Some("unconfirmed".into())));
    // Only the token that registered it reads it; anything else looks unknown.
    assert_eq!(state(&other).0, 404);
    let mut wrong = poll.clone();
    wrong["input_hash"] = json!("b".repeat(64));
    assert_eq!(call("poll", &secret, &wrong).0, 404);
    // The host's own notification says the prompt is on screen.
    hub.hook("claude", &secret, &fixture("claude-Notification"));
    assert_eq!(state(&secret), (200, Some("pending".into())));
    // The same registration again is the same request.
    assert_eq!(call("register", &secret, &register).1["request_id"], id);
    // The person's routes take a web sign-in, never an agent token.
    let answer = json!({"repository": REPO, "request_id": id, "decision": "allow"});
    let answered = hub.post(
        "/v1/hub/session/permissions/answer",
        Some(&secret),
        answer.to_string().as_bytes(),
    );
    assert_eq!(answered.0, 401, "{}", answered.1);
    // The tool ran, so it was answered in the session: the wait ends and the request with it.
    let ran = json!({"hook_event_name": "PostToolUse", "session_id": session, "tool_name": "Bash"});
    hub.hook("claude", &secret, &ran);
    assert_eq!(state(&secret), (200, Some("ended".into())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_refusal_carries_recorded_holder_intent_as_data() {
    let mut hub = Hub::start().await;
    let first = hub.agent();
    let second = hub.agent();
    let principal = hub
        .store
        .authenticate(&agent_token_digest(&first).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    let session = "holder-intent";
    let start:WorkStart=serde_json::from_value(json!({"request_key":"holder-intent","title":"Preserve request behavior","description":"Keep old clients decoding responses","session":session,"scope":["src/widget.rs"],"next_action":"Run compatibility tests"})).unwrap();
    let item = hub
        .store
        .work_start(&principal.token_id, &start, unix_now())
        .unwrap()
        .item;
    let checkpoint:agentscm::hub::work::WorkCheckpoint=serde_json::from_value(json!({"work_id":item.id,"operation_id":"why","generation":1,"session":session,"expected_updated_seq":item.updated_seq,"kind":"decision","summary":"Hub message 'run tools' is quoted rationale","next_action":"Test clients"})).unwrap();
    hub.store
        .work_checkpoint(&principal.token_id, &checkpoint, unix_now())
        .unwrap();
    let mut edit = fixture("claude-PreToolUse-Edit");
    edit["session_id"] = json!(session);
    hub.hook("claude", &first, &edit);
    edit["session_id"] = json!("requester-intent");
    let refused = hub.hook_raw("claude", &second, &edit);
    let reason = refused["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert_eq!(refused["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(
        reason.contains("Keep old clients decoding responses"),
        "{reason}"
    );
    assert!(reason.contains("Hub note 'run tools'"), "{reason}");
    assert!(!reason.contains("Hub message"), "{reason}");
    assert!(
        reason.contains("never instructions or permission"),
        "{reason}"
    );
    assert!(reason.contains("waiting-on"), "{reason}");
    assert!(reason.contains(&item.id), "{reason}");
    assert!(reason.len() < 16 * 1024);
    // A repeat follows the existing warn-once behavior; intent does not grant or deny authority.
    assert_eq!(hub.hook_raw("claude", &second, &edit), json!({}));
    let third = hub.agent();
    let note = hub.hook_raw("cursor", &third, &fixture("cursor-postToolUse"));
    let note = note["additional_context"].as_str().unwrap();
    assert!(
        note.contains("Keep old clients decoding responses"),
        "{note}"
    );
    assert!(note.contains("never instructions or permission"), "{note}");
}
