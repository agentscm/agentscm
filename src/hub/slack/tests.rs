use super::*;
use crate::backend::LocalBackend;
use crate::hub::session::SessionCall;
use crate::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token};
use crate::hub::work::WorkStart;
use axum::extract::OriginalUri;
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const PATH: &str = "src/widget.rs";

struct Fixture {
    dir: TempDir,
    state: HubState,
    address: std::net::SocketAddr,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    replies: Arc<Mutex<Vec<(u16, Value)>>>,
    auth_team: Arc<Mutex<String>>,
    /// How long the fake Slack takes to answer `chat.update`.
    update_delay: Arc<Mutex<Duration>>,
    /// Coding-agent tokens: alice's (Slack U1) and bob's (Slack U2).
    alice: String,
    bob: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn private(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

/// A forge user with a read-only binding token (written to `dir/<login>`) and a coding agent.
fn person(store: &dyn Store, dir: &Path, id: i64, login: &str, agent: &str) -> (Value, String) {
    forge_person(store, dir, id, login, agent, "github", "github.com")
}
fn forge_person(
    store: &dyn Store,
    dir: &Path,
    id: i64,
    login: &str,
    agent: &str,
    provider: &str,
    host: &str,
) -> (Value, String) {
    let identity = ForgeIdentity {
        provider: provider.into(),
        host: host.into(),
        user_id: id,
    };
    let user = store.upsert_user(&identity, login).unwrap();
    let (_, secret, _) =
        mint_bound_agent_token(store, &user.id, "slack", REPO, 101, 3600, true).unwrap();
    private(&dir.join(login), secret.expose());
    let (token, _, _) =
        mint_bound_agent_token(store, &user.id, agent, REPO, 101, 3600, false).unwrap();
    let binding = json!({"slack_user": format!("U{id}"), "provider": provider,
        "host": host, "forge_user_id": id, "repository": REPO, "repository_id": 101,
        "agent_token_file": dir.join(login)});
    (binding, token)
}

/// A message's header, fields (`Label: value`) and context lines, joined by ` | `.
fn rendered(body: &Value) -> String {
    let mut parts = Vec::new();
    for block in body["blocks"].as_array().unwrap() {
        let texts: Vec<&Value> = match block["type"].as_str().unwrap() {
            "header" => vec![&block["text"]],
            "section" => block["fields"].as_array().unwrap().iter().collect(),
            "context" => block["elements"].as_array().unwrap().iter().collect(),
            _ => Vec::new(),
        };
        for text in texts {
            assert_eq!(text["type"], "plain_text");
            parts.push(text["text"].as_str().unwrap().replacen('\n', ": ", 1));
        }
    }
    parts.join(" | ")
}

impl Fixture {
    async fn new() -> Self {
        Self::configured(None, false).await
    }

    async fn configured(policy: Option<Value>, other_forge: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        let store = store::open(&url).unwrap();
        let (mut alice_binding, alice) = person(&*store, dir.path(), 1, "alice", "claude");
        let (bob_binding, bob) = if other_forge {
            forge_person(
                &*store,
                dir.path(),
                2,
                "bob",
                "codex",
                "gitlab",
                "gitlab.example.test",
            )
        } else {
            person(&*store, dir.path(), 2, "bob", "codex")
        };
        if let Some(policy) = policy {
            alice_binding["reminders"] = policy;
        }
        private(&dir.path().join("signing"), "synthetic-signing-secret");
        private(&dir.path().join("bot"), "xoxb-synthetic");
        let config = json!({"version":1,"app_id":"A123","bot_id":"B123","team_id":"T123",
            "hub_url":"https://hub.example.test/","signing_secret_file":dir.path().join("signing"),
            "bot_token_file":dir.path().join("bot"),"database":dir.path().join("slack.sqlite"),
            "bindings":[alice_binding, bob_binding]});
        private(&dir.path().join("config"), &config.to_string());

        let calls = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let replies = Arc::new(Mutex::new(Vec::<(u16, Value)>::new()));
        let auth_team = Arc::new(Mutex::new("T123".to_owned()));
        let update_delay = Arc::new(Mutex::new(Duration::ZERO));
        let (capture, outcomes, team, delay) = (
            calls.clone(),
            replies.clone(),
            auth_team.clone(),
            update_delay.clone(),
        );
        let fake = Router::new().route(
            "/api/{method}",
            post(move |OriginalUri(uri): OriginalUri, body: Bytes| {
                let (capture, outcomes, team) = (capture.clone(), outcomes.clone(), team.clone());
                let delay = *delay.lock().unwrap();
                async move {
                    if uri.path() == "/api/chat.update" {
                        tokio::time::sleep(delay).await;
                    }
                    if uri.path() == "/api/auth.test" {
                        let team = team.lock().unwrap().clone();
                        return (
                            StatusCode::OK,
                            axum::Json(json!({"ok":true,"team_id":team,"bot_id":"B123"})),
                        );
                    }
                    capture.lock().unwrap().push((
                        uri.path().to_owned(),
                        serde_json::from_slice(&body).unwrap(),
                    ));
                    let (status, reply) = outcomes
                        .lock()
                        .unwrap()
                        .pop()
                        .unwrap_or((200, json!({"ok":true})));
                    (StatusCode::from_u16(status).unwrap(), axum::Json(reply))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = listener.local_addr().unwrap();
        let fake_task = tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        let mut slack = Slack::from_file(&dir.path().join("config")).unwrap();
        slack.api = format!("http://{fake_address}/api/");
        let state = HubState::new(
            store,
            Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
        )
        .with_slack(slack);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = super::super::router(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let fixture = Self {
            dir,
            state,
            address,
            calls,
            replies,
            auth_team,
            update_delay,
            alice,
            bob,
            tasks: vec![fake_task, server],
        };
        // The first pass only sets the cursor: everything before it is history.
        fixture.run().await;
        fixture
    }

    fn slack(&self) -> Arc<Slack> {
        self.state.slack.clone().unwrap()
    }

    fn store(&self) -> &Arc<dyn Store> {
        &self.state.store
    }

    /// One pass of the worker: collect events, then send every due message.
    async fn run(&self) {
        let (slack, state) = (self.slack(), self.state.clone());
        tokio::task::block_in_place(|| slack.collect(&state)).unwrap();
        loop {
            let before = self.calls.lock().unwrap().len();
            self.slack().deliver(&self.state).await.unwrap();
            if self.calls.lock().unwrap().len() == before {
                break;
            }
        }
    }

    /// A hook-style edit of `PATH` by `token` in session `session`.
    fn edit(&self, token: &str, session: &str) -> crate::hub::session::Touched {
        let call = SessionCall {
            session: Some(session.into()),
            via: "hook:claude".into(),
            task: None,
            paths: vec![PATH.into()],
            deny: true,
            report: false,
        };
        tokio::task::block_in_place(|| {
            self.store()
                .touch_session(token, &call, unix_now())
                .unwrap()
        })
    }

    /// A touch that collects the handoff answers waiting for the session.
    fn listen(&self, token: &str, session: &str) -> crate::hub::session::Touched {
        let call = SessionCall {
            session: Some(session.into()),
            via: "hook:claude".into(),
            task: None,
            paths: Vec::new(),
            deny: false,
            report: true,
        };
        tokio::task::block_in_place(|| {
            self.store()
                .touch_session(token, &call, unix_now())
                .unwrap()
        })
    }

    /// Bob edits the file alice's agent holds: refused once, then repeated.
    fn contest(&self) {
        self.edit(&self.alice, "a");
        assert!(!self.edit(&self.bob, "b").denied.is_empty());
        assert!(!self.edit(&self.bob, "b").repeated.is_empty());
    }

    /// The direct messages sent so far, as (Slack user, rendered text).
    fn sent(&self) -> Vec<(String, String)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/api/chat.postMessage")
            .map(|(_, body)| (body["channel"].as_str().unwrap().to_owned(), rendered(body)))
            .collect()
    }

    fn sent_body(&self, user: &str) -> Value {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(path, body)| path == "/api/chat.postMessage" && body["channel"] == user)
            .unwrap()
            .1
            .clone()
    }

    /// The first `method` call (`/api/chat.update`...) the fake Slack saw; a click's follow-up
    /// runs after the click is acknowledged.
    async fn call(&self, method: &str) -> Value {
        for _ in 0..100 {
            let found = self
                .calls
                .lock()
                .unwrap()
                .iter()
                .find(|(path, _)| path == method)
                .map(|(_, body)| body.clone());
            if let Some(found) = found {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no {method} was sent");
    }

    /// The `chat.update` a button click sent.
    async fn update(&self) -> Value {
        self.call("/api/chat.update").await
    }

    /// The Hub user id of binding `index` (0 is alice's, 1 bob's).
    fn owner(&self, index: usize) -> String {
        tokio::task::block_in_place(|| {
            self.store()
                .authenticate(&self.slack().bindings[index].digest, unix_now())
                .unwrap()
                .unwrap()
                .owner
                .id
        })
    }

    /// Where the Open Hub button of `user`'s latest message goes.
    fn link(&self, user: &str) -> String {
        let message = self.sent_body(user);
        let elements = message["blocks"][2]["elements"].as_array().unwrap();
        let open = elements.iter().find(|e| e["action_id"] == OPEN).unwrap();
        open["url"].as_str().unwrap().to_owned()
    }

    fn state_of_messages(&self) -> Vec<String> {
        let db = self.slack();
        let db = db.database().unwrap();
        let mut query = db
            .prepare("SELECT state FROM messages ORDER BY id")
            .unwrap();
        query
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    async fn post(&self, body: String, json_body: bool) -> (StatusCode, Value) {
        let timestamp = unix_now().to_string();
        let signature = crate::hub::slack_protocol::sign_request(
            "synthetic-signing-secret",
            &timestamp,
            body.as_bytes(),
        );
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/hub/slack", self.address))
            .header(
                "content-type",
                if json_body {
                    "application/json"
                } else {
                    "application/x-www-form-urlencoded"
                },
            )
            .header("x-slack-request-timestamp", timestamp)
            .header("x-slack-signature", signature)
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.bytes().await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// `user` clicks the button `action_id` of their latest direct message.
    async fn click(&self, user: &str, action_id: &str) -> StatusCode {
        let message = self.sent_body(user);
        let button = message["blocks"][2]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["action_id"] == action_id)
            .unwrap()
            .clone();
        self.click_value(user, action_id, &button["value"], &message)
            .await
    }

    async fn click_value(
        &self,
        user: &str,
        action_id: &str,
        value: &Value,
        message: &Value,
    ) -> StatusCode {
        let payload = json!({"type":"block_actions","api_app_id":"A123","team":{"id":"T123"},
            "user":{"id":user},"channel":{"id":"D1"},
            "message":{"ts":"1.2","blocks":message["blocks"]},
            "actions":[{"action_id":action_id,"value":value}]});
        let form =
            reqwest::Url::parse_with_params("http://localhost", [("payload", payload.to_string())])
                .unwrap();
        self.post(form.query().unwrap().into(), false).await.0
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_checks_signature_workspace_app_and_request_shape() {
    let f = Fixture::new().await;
    let url = format!("http://{}/v1/hub/slack", f.address);
    let unsigned = reqwest::Client::new()
        .post(&url)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status(), StatusCode::UNAUTHORIZED);
    let challenge = json!({"type":"url_verification","challenge":"synthetic"}).to_string();
    assert_eq!(f.post(challenge, true).await.1["challenge"], "synthetic");
    let foreign = json!({"type":"event_callback","team_id":"T999","api_app_id":"A123",
        "event":{"type":"app_uninstalled"}});
    assert_eq!(
        f.post(foreign.to_string(), true).await.0,
        StatusCode::FORBIDDEN
    );
    // Slash commands are gone: a form without an interaction payload is refused.
    let command = "team_id=T123&api_app_id=A123&user_id=U1&command=%2Fagentscm&text=status";
    assert_eq!(
        f.post(command.into(), false).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.post("x".repeat(65537), true).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(f.slack().active().is_ok());
    assert!(f.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeat_between_two_sessions_of_one_token_messages_the_person_once() {
    let f = Fixture::new().await;
    f.edit(&f.alice, "a");
    assert!(!f.edit(&f.alice, "b").denied.is_empty());
    assert!(!f.edit(&f.alice, "b").repeated.is_empty());
    f.run().await;
    let key = &crate::hub::store::session_key(&f.alice, "a")[..8];
    // Never a message about the repeater's own session, and not one to the holder as well.
    assert_eq!(
        f.sent(),
        [(
            "U1".to_owned(),
            format!(
                "Your Claude Code edited src/widget.rs, which alice’s Claude Code session {key} holds | File: src/widget.rs | Repository: acme/widgets | Your agent: Claude Code | Their agent: alice’s Claude Code session {key} | The Hub warned your Claude Code once; a repeat edit goes ahead by design. Files are never locked. Agree with them who goes first."
            )
        )]
    );
    // A session repeating on its own file alone says nothing.
    let quiet = Fixture::new().await;
    quiet.edit(&quiet.alice, "a");
    quiet.edit(&quiet.alice, "a");
    quiet.run().await;
    assert!(quiet.sent().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeater_is_not_told_about_its_own_mcp_session() {
    let f = Fixture::new().await;
    f.edit(&f.bob, "b");
    let mcp = SessionCall {
        session: Some("m".into()),
        via: "mcp".into(),
        task: None,
        paths: vec![PATH.into()],
        deny: false,
        report: false,
    };
    tokio::task::block_in_place(|| f.store().touch_session(&f.alice, &mcp, unix_now())).unwrap();
    assert!(!f.edit(&f.alice, "a").denied.is_empty());
    assert!(!f.edit(&f.alice, "a").repeated.is_empty());
    f.run().await;
    let sent = f.sent();
    let told: Vec<_> = sent.iter().filter(|(user, _)| user == "U1").collect();
    assert_eq!(told.len(), 1, "{sent:?}");
    assert!(
        !told[0].1.contains("other agents") && !told[0].1.contains("session"),
        "{sent:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_holder_sessions_of_one_token_message_their_person_once() {
    let f = Fixture::new().await;
    // A REST intent with the lowest id holds the file too, and no session could answer for it.
    let rest = crate::hub::intent::NewIntent {
        id: format!("int_{:032x}", 0),
        task: "rest".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec![PATH.into()],
        ttl_seconds: 3600,
    };
    tokio::task::block_in_place(|| f.store().announce_intent(&f.alice, &rest, unix_now())).unwrap();
    f.edit(&f.alice, "a");
    // c's refusal leaves its own request open on a: the button still answers bob, who it names.
    f.edit(&f.alice, "c");
    f.edit(&f.alice, "c");
    f.run().await;
    let before = f.sent().len();
    let refused = f.edit(&f.bob, "b");
    assert!(!refused.denied.is_empty());
    let asked = refused.requested[0].1.session.clone().unwrap();
    assert!(!f.edit(&f.bob, "b").repeated.is_empty());
    f.run().await;
    let sent = f.sent();
    let holder: Vec<_> = sent[before..]
        .iter()
        .filter(|(user, text)| user == "U1" && text.contains("which your"))
        .collect();
    assert_eq!(holder.len(), 1, "{sent:?}");
    // Two sessions of one token read alike by label, so each side names them.
    let key = |session: &str| crate::hub::store::session_key(&f.alice, session)[..8].to_owned();
    let other = if asked == "a" { "c" } else { "a" };
    assert!(
        holder[0]
            .1
            .contains(&format!("Your agent: Claude Code session {}", key(&asked)))
            && !holder[0].1.contains(&key(other)),
        "{sent:?}"
    );
    let repeater = sent[before..]
        .iter()
        .find(|(user, _)| user == "U2")
        .map(|(_, text)| text.as_str())
        .unwrap();
    assert!(
        repeater.contains(&key("a")) && repeater.contains(&key("c")),
        "{sent:?}"
    );
    // The one button answers for the session the request was filed against.
    assert_eq!(f.click("U1", YES).await, StatusCode::OK);
    assert_eq!(
        f.update().await["blocks"][3]["elements"][0]["text"],
        "Handed src/widget.rs to bob’s Codex. They see it on their next edit."
    );
}

/// An MCP-only session may be the same agent as a hook session of its token: one agent, one name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holders_hook_and_mcp_sessions_keep_the_single_agent_wording() {
    let f = Fixture::new().await;
    let mcp = SessionCall {
        session: Some("m".into()),
        via: "mcp".into(),
        task: None,
        paths: vec![PATH.into()],
        deny: false,
        report: false,
    };
    tokio::task::block_in_place(|| f.store().touch_session(&f.alice, &mcp, unix_now())).unwrap();
    f.edit(&f.alice, "a");
    f.run().await;
    let before = f.sent().len();
    assert!(!f.edit(&f.bob, "b").denied.is_empty());
    assert!(!f.edit(&f.bob, "b").repeated.is_empty());
    f.run().await;
    let sent = f.sent();
    assert!(
        sent[before..]
            .iter()
            .any(|(user, text)| user == "U1" && text.contains("which your Claude Code holds")),
        "{sent:?}"
    );
    assert!(
        sent[before..]
            .iter()
            .all(|(_, text)| !text.contains(" session ")),
        "{sent:?}"
    );
    assert!(
        sent[before..].iter().any(|(user, text)| user == "U2"
            && text.contains("which alice’s Claude Code holds")
            && text.contains("Their agent: alice’s Claude Code |")),
        "{sent:?}"
    );
}

/// Hook a and hook c are distinct, whichever session is oldest: an older MCP session of the token
/// must not hide that from the holder's message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_mcp_session_does_not_hide_two_hook_holders() {
    let f = Fixture::new().await;
    let mcp = SessionCall {
        session: Some("m".into()),
        via: "mcp".into(),
        task: None,
        paths: vec![PATH.into()],
        deny: false,
        report: false,
    };
    tokio::task::block_in_place(|| f.store().touch_session(&f.alice, &mcp, unix_now() - 100))
        .unwrap();
    f.edit(&f.alice, "a");
    f.edit(&f.alice, "c");
    tokio::task::block_in_place(|| {
        f.store()
            .answer_handoff(&f.alice, Some("a"), PATH, false, None, unix_now())
            .unwrap()
    });
    f.edit(&f.alice, "c");
    f.run().await;
    let before = f.sent().len();
    assert!(!f.edit(&f.bob, "b").denied.is_empty());
    assert!(!f.edit(&f.bob, "b").repeated.is_empty());
    f.run().await;
    let sent = f.sent();
    let holder: Vec<_> = sent[before..]
        .iter()
        .filter(|(user, text)| user == "U1" && text.contains("which your"))
        .collect();
    assert_eq!(holder.len(), 1, "{sent:?}");
    assert!(holder[0].1.contains(" session "), "{sent:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_decline_under_one_token_names_the_requesters_session() {
    let f = Fixture::new().await;
    f.edit(&f.alice, "a");
    assert!(!f.edit(&f.alice, "b").denied.is_empty());
    tokio::task::block_in_place(|| {
        f.store()
            .answer_handoff(&f.alice, Some("a"), PATH, false, None, unix_now())
            .unwrap()
    });
    f.run().await;
    let key = &crate::hub::store::session_key(&f.alice, "b")[..8];
    assert_eq!(
        f.sent(),
        [(
            "U1".to_owned(),
            format!(
                "Another agent kept a file your agent asked for | File: src/widget.rs | Repository: acme/widgets | Your agent: Claude Code session {key} | Their agent: alice’s Claude Code | Agree with them who goes first."
            )
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeated_edit_messages_both_people_and_the_holder_hands_over_with_a_button() {
    let f = Fixture::new().await;
    f.contest();
    f.run().await;
    let sent = f.sent();
    assert_eq!(sent.len(), 2, "{sent:?}");
    let holder = &sent.iter().find(|(user, _)| user == "U1").unwrap().1;
    assert_eq!(
        holder,
        "bob’s Codex edited src/widget.rs, which your Claude Code holds | File: src/widget.rs | Repository: acme/widgets | Your agent: Claude Code | Their agent: bob’s Codex | The Hub warned bob’s Codex once; a repeat edit goes ahead by design. Files are never locked. Hand it over, or keep it and agree who goes first."
    );
    let repeater = &sent.iter().find(|(user, _)| user == "U2").unwrap().1;
    assert_eq!(
        repeater,
        "Your Codex edited src/widget.rs, which alice’s Claude Code holds | File: src/widget.rs | Repository: acme/widgets | Your agent: Codex | Their agent: alice’s Claude Code | The Hub warned your Codex once; a repeat edit goes ahead by design. Files are never locked. Agree with them who goes first."
    );
    // The notification names the file once.
    assert_eq!(
        f.sent_body("U1")["text"],
        "bob’s Codex edited src/widget.rs, which your Claude Code holds"
    );
    // Only the holder's message carries the handoff buttons; both link the Hub.
    let buttons = |user: &str| {
        f.sent_body(user)["blocks"][2]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["action_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(buttons("U1"), [YES, NO, OPEN]);
    assert_eq!(buttons("U2"), [OPEN]);
    // Both buttons are the same colour, and the link goes to the contested file's box.
    let sent_buttons = f.sent_body("U1")["blocks"][2]["elements"].clone();
    assert!(
        sent_buttons
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b.get("style").is_none())
    );
    assert_eq!(f.link("U1"), "https://hub.example.test/ui/#box-contested");
    assert_eq!(f.link("U2"), "https://hub.example.test/ui/#box-contested");

    // Bob cannot answer for alice's agent with her button: only he sees why, nothing changes.
    let alice_message = f.sent_body("U1");
    let value = &alice_message["blocks"][2]["elements"][0]["value"];
    assert_eq!(
        f.click_value("U2", YES, value, &alice_message).await,
        StatusCode::OK
    );
    let notice = f.call("/api/chat.postEphemeral").await;
    assert_eq!(
        (notice["channel"].as_str(), notice["user"].as_str()),
        (Some("D1"), Some("U2"))
    );
    assert_eq!(
        notice["text"],
        "The Hub did not accept this: it is not your agent, or it is no longer connected. Nothing changed. Answer for your own agent in the Hub."
    );
    assert!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(path, _)| path != "/api/chat.update")
    );
    assert_eq!(f.click("U1", YES).await, StatusCode::OK);
    let update = f.update().await;
    assert_eq!(update["channel"], "D1");
    assert_eq!(update["ts"], "1.2");
    // The header, fields and link to the Hub stay; the handoff buttons and guidance become the
    // outcome.
    let kinds: Vec<_> = update["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["header", "section", "actions", "context"]);
    assert_eq!(
        update["blocks"][2]["elements"],
        json!([open_button("https://hub.example.test/ui/#box-contested")])
    );
    assert_eq!(
        rendered(&update),
        "bob’s Codex edited src/widget.rs, which your Claude Code holds | File: src/widget.rs | Repository: acme/widgets | Your agent: Claude Code | Their agent: bob’s Codex | Handed src/widget.rs to bob’s Codex. They see it on their next edit."
    );
    assert_eq!(
        update["text"],
        "Handed src/widget.rs to bob’s Codex. They see it on their next edit."
    );
    let holders: Vec<String> = tokio::task::block_in_place(|| {
        f.store()
            .live_intents(REPO, unix_now(), 10)
            .unwrap()
            .into_iter()
            .filter(|i| i.paths.contains(&PATH.to_owned()))
            .map(|i| i.token)
            .collect()
    });
    assert_eq!(holders, std::slice::from_ref(&f.bob));
    // The Hub records it as answered from Slack, not by the agent or from its web pages.
    let alice = f.owner(0);
    let handoffs = tokio::task::block_in_place(|| {
        f.store()
            .handoffs_of_owner(&alice, REPO, unix_now(), 10)
            .unwrap()
    });
    assert_eq!(handoffs.len(), 1);
    assert_eq!(handoffs[0].state, "granted");
    assert!(handoffs[0].answered_via_slack && !handoffs[0].answered_via_hub);
    // Bob's agent is told at its next edit, and by whom.
    let told = f.listen(&f.bob, "b").handoffs;
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].via, Via::Slack);
    // A second click finds nothing open and says nothing changed; the same events never
    // message again.
    assert_eq!(f.click("U1", NO).await, StatusCode::OK);
    for _ in 0..100 {
        if f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/api/chat.update")
            .count()
            == 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let updates: Vec<Value> = f
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "/api/chat.update")
        .map(|(_, body)| body.clone())
        .collect();
    assert_eq!(
        updates[1]["text"],
        "Nothing changed. There is no open request for src/widget.rs any more: it was answered or lapsed."
    );
    f.run().await;
    assert_eq!(f.sent().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_journal_holds_events_until_queued_messages_are_sent() {
    let f = Fixture::new().await;
    {
        let slack = f.slack();
        let mut db = slack.database().unwrap();
        let tx = db.transaction().unwrap();
        for n in 0..CAPACITY {
            tx.execute(
                "INSERT INTO messages(id,user,body,state,created,due) VALUES(?1,'U9','{}','queued',?2,?3)",
                params![format!("waiting{n}"), unix_now(), unix_now() + 86400],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    f.contest();
    // Events before the repeat need no message and pass; the repeat waits at the cursor.
    let (slack, state) = (f.slack(), f.state.clone());
    let _ = tokio::task::block_in_place(|| slack.collect(&state));
    let queued: i64 = f
        .slack()
        .database()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM messages WHERE id NOT LIKE 'waiting%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(queued, 0);
    assert!(f.slack().full.load(std::sync::atomic::Ordering::Relaxed));
    // Sent rows no longer count: the held event is collected, not lost.
    f.slack()
        .database()
        .unwrap()
        .execute(
            "UPDATE messages SET state='done' WHERE id LIKE 'waiting%'",
            [],
        )
        .unwrap();
    f.run().await;
    assert_eq!(f.sent().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_holder_button_answers_for_the_session_that_holds_the_file() {
    let f = Fixture::new().await;
    f.edit(&f.alice, "a");
    // A second hook session of alice's token: the token's default session is no longer "a".
    let other = SessionCall {
        session: Some("c".into()),
        via: "hook:claude".into(),
        task: None,
        paths: Vec::new(),
        deny: true,
        report: false,
    };
    tokio::task::block_in_place(|| f.store().touch_session(&f.alice, &other, unix_now())).unwrap();
    assert!(!f.edit(&f.bob, "b").denied.is_empty());
    assert!(!f.edit(&f.bob, "b").repeated.is_empty());
    f.run().await;
    assert_eq!(f.click("U1", YES).await, StatusCode::OK);
    let update = f.update().await;
    assert_eq!(
        update["blocks"][3]["elements"][0]["text"],
        "Handed src/widget.rs to bob’s Codex. They see it on their next edit."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_message_update_does_not_delay_the_button_acknowledgement() {
    let f = Fixture::new().await;
    f.contest();
    f.run().await;
    // Within the API client's timeout, so the update completes, but only after the click.
    *f.update_delay.lock().unwrap() = Duration::from_millis(1500);
    assert_eq!(f.click("U1", YES).await, StatusCode::OK);
    // The fake records an update once it answers; the click was acknowledged before that.
    assert!(
        !f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|(path, _)| path == "/api/chat.update")
    );
    assert_eq!(
        f.update().await["blocks"][3]["elements"][0]["text"],
        "Handed src/widget.rs to bob’s Codex. They see it on their next edit."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_past_a_restored_event_log_restarts_at_its_newest_event() {
    let f = Fixture::new().await;
    // As if the Hub database were restored from a backup older than the Slack journal.
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE cursor SET seq=1000000 WHERE id=1", [])
        .unwrap();
    f.run().await;
    f.contest();
    f.run().await;
    assert_eq!(f.sent().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeated_edit_of_several_held_files_is_one_message_without_buttons() {
    let f = Fixture::new().await;
    let edit = |token: &str, session: &str| {
        let call = SessionCall {
            session: Some(session.into()),
            via: "hook:claude".into(),
            task: None,
            paths: vec![PATH.into(), "src/gadget.rs".into()],
            deny: true,
            report: false,
        };
        tokio::task::block_in_place(|| f.store().touch_session(token, &call, unix_now()).unwrap())
    };
    edit(&f.alice, "a");
    assert!(!edit(&f.bob, "b").denied.is_empty());
    assert!(!edit(&f.bob, "b").repeated.is_empty());
    f.run().await;
    let holder: Vec<_> = f
        .sent()
        .into_iter()
        .filter(|(user, _)| user == "U1")
        .collect();
    assert_eq!(holder.len(), 1, "{holder:?}");
    assert_eq!(
        holder[0].1,
        "bob’s Codex edited 2 files your Claude Code holds | Files: src/gadget.rs\nsrc/widget.rs | Repository: acme/widgets | Your agent: Claude Code | Their agent: bob’s Codex | The Hub warned bob’s Codex once; a repeat edit goes ahead by design. Files are never locked. Open the Hub to hand them over or keep them, or agree who goes first."
    );
    let buttons: Vec<_> = f.sent_body("U1")["blocks"][2]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(buttons, [OPEN]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declined_handoff_messages_both_people_with_the_reason() {
    let f = Fixture::new().await;
    f.edit(&f.alice, "a");
    assert!(!f.edit(&f.bob, "b").denied.is_empty());
    f.run().await;
    assert!(f.sent().is_empty(), "a refusal alone is for the agents");
    tokio::task::block_in_place(|| {
        f.store()
            .answer_handoff(
                &f.alice,
                None,
                PATH,
                false,
                Some("mid-refactor"),
                unix_now(),
            )
            .unwrap()
    });
    f.run().await;
    let sent = f.sent();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent.contains(&(
        "U2".into(),
        "Another agent kept a file your agent asked for | File: src/widget.rs | Repository: acme/widgets | Your agent: Codex | Their agent: alice’s Claude Code | Reason: mid-refactor | Agree with them who goes first.".into()
    )));
    assert!(sent.contains(&(
        "U1".into(),
        "Your agent kept a file | File: src/widget.rs | Repository: acme/widgets | Your agent: Claude Code | Their agent: bob’s Codex | Reason: mid-refactor | Agree with them who goes first.".into()
    )));
    // Both messages open the Inbox, where the request's thread is.
    assert_eq!(f.link("U1"), "https://hub.example.test/ui/#inbox");
    assert_eq!(f.link("U2"), "https://hub.example.test/ui/#inbox");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn similar_work_messages_the_starter_and_the_lead() {
    let f = Fixture::new().await;
    let start: WorkStart = serde_json::from_value(json!({
        "title": "make db migrations filename based", "session": "a",
        "work_id": null, "source": null, "request_key": null, "branch": null,
        "worktree": null, "next_action": null, "takeover_generation": null}))
    .unwrap();
    let work = tokio::task::block_in_place(|| {
        f.store()
            .work_start_auto(&f.alice, &start, unix_now())
            .unwrap()
    });
    let data = json!({"task": "switch migrations to filename-based discovery",
        "work": [work.item.id], "pulls": [9]})
    .to_string();
    tokio::task::block_in_place(|| {
        f.store()
            .append_notices("task_overlap", &[(f.bob.clone(), data)])
            .unwrap()
    });
    f.run().await;
    let sent = f.sent();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent.contains(&(
        "U2".into(),
        "Your agent started similar work | Task: switch migrations to filename-based discovery | Repository: acme/widgets | Your agent: Codex | Reads like: alice’s Claude Code: make db migrations filename based\nOpen pull request #9 | Check whether it is the same work.".into()
    )));
    assert!(sent.contains(&(
        "U1".into(),
        "Similar work started | Task: switch migrations to filename-based discovery | Repository: acme/widgets | Their agent: bob’s Codex | Your agent: Claude Code | Your work: make db migrations filename based | Check with them whether it is the same work.".into()
    )));
    // Each message opens the work it is about.
    let page = format!("https://hub.example.test/ui/#work/{}", work.item.id);
    assert_eq!(f.link("U1"), page);
    assert_eq!(f.link("U2"), page);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_to_an_idle_lead_messages_its_person_once() {
    let f = Fixture::new().await;
    let start = |title: &str, session: &str, path: &str| -> WorkStart {
        serde_json::from_value(json!({"title": title, "session": session, "scope": [path],
            "work_id": null, "source": null, "request_key": null, "branch": null,
            "worktree": null, "next_action": null, "takeover_generation": null}))
        .unwrap()
    };
    let now = unix_now();
    let requests = tokio::task::block_in_place(|| {
        let store = f.store();
        // Alice's agent stopped checking in on one task long enough for its claim to go stale,
        // though its reservation's lease has not lapsed, and is still on the other.
        let idle = store
            .work_start_auto(
                &f.alice,
                &start("readable catch up", "a1", "ui/catchup.tsx"),
                now - 1000,
            )
            .unwrap();
        store
            .work_start_auto(&f.alice, &start("dashboard", "a2", "ui/dashboard.tsx"), now)
            .unwrap();
        let live = store
            .work_start_auto(
                &f.bob,
                &start("dashboard too", "b1", "ui/dashboard.tsx"),
                now,
            )
            .unwrap();
        let asked = store
            .work_start_auto(&f.bob, &start("catch up too", "b2", "ui/catchup.tsx"), now)
            .unwrap();
        assert_eq!(
            asked.coordination_request.as_ref().unwrap().targets[0].work_id,
            idle.item.id
        );
        [live.coordination_request, asked.coordination_request]
    });
    assert!(requests.iter().all(Option::is_some));
    f.run().await;
    assert_eq!(f.sent(), vec![(
        "U1".into(),
        "Another agent asked for files your agent holds | File: ui/catchup.tsx | Repository: acme/widgets | Their agent: bob’s Codex | Your agent: Claude Code | Your work: readable catch up | Your agent isn’t running, so it can’t answer. Answer for it in the Hub Inbox.".into()
    )]);
    assert_eq!(f.link("U1"), "https://hub.example.test/ui/#inbox");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limits_retry_and_a_wrong_bot_or_revoked_binding_sends_nothing() {
    let f = Fixture::new().await;
    f.contest();
    f.replies.lock().unwrap().push((429, json!({"ok":false})));
    let (slack, state) = (f.slack(), f.state.clone());
    tokio::task::block_in_place(|| slack.collect(&state)).unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    assert_eq!(f.state_of_messages(), ["queued", "queued"]);

    // A bot token for another workspace never sends.
    *f.auth_team.lock().unwrap() = "T999".into();
    *f.slack().verified_until.lock().await = 0;
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE messages SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    assert!(f.state_of_messages().contains(&"failed".to_owned()));

    // Revoking a person's binding token fails their queued message instead of sending it.
    let waiting: String = f
        .slack()
        .database()
        .unwrap()
        .query_row("SELECT user FROM messages WHERE state='queued'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let binding = f
        .slack()
        .bindings
        .iter()
        .position(|b| b.config.slack_user == waiting)
        .unwrap();
    let tokens = tokio::task::block_in_place(|| {
        let owner = f
            .store()
            .authenticate(&f.slack().bindings[binding].digest, unix_now());
        let owner = owner.unwrap().unwrap().owner.id;
        f.store().list_agent_tokens(&owner).unwrap()
    });
    let binding = tokens.iter().find(|t| t.name == "slack").unwrap();
    tokio::task::block_in_place(|| f.store().revoke_agent_token(&binding.id).unwrap());
    *f.auth_team.lock().unwrap() = "T123".into();
    *f.slack().verified_until.lock().await = 0;
    let calls = f.calls.lock().unwrap().len();
    f.slack().deliver(&f.state).await.unwrap();
    assert_eq!(f.state_of_messages(), ["failed", "failed"]);
    assert_eq!(f.calls.lock().unwrap().len(), calls);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uninstall_stops_messages_and_survives_restart() {
    let f = Fixture::new().await;
    f.contest();
    let (slack, state) = (f.slack(), f.state.clone());
    tokio::task::block_in_place(|| slack.collect(&state)).unwrap();
    let event = json!({"type":"event_callback","team_id":"T123","api_app_id":"A123",
        "event":{"type":"app_uninstalled"}});
    assert_eq!(f.post(event.to_string(), true).await.0, StatusCode::OK);
    assert_eq!(f.state_of_messages(), ["failed", "failed"]);
    let reopened = Slack::from_file(&f.dir.path().join("config")).unwrap();
    assert!(reopened.active().is_err());
    assert!(f.sent().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keeping_a_file_from_slack_tells_the_requester_but_not_the_person_who_clicked() {
    let f = Fixture::new().await;
    f.contest();
    f.run().await;
    assert_eq!(f.sent().len(), 2);
    assert_eq!(f.click("U1", NO).await, StatusCode::OK);
    let update = f.update().await;
    assert_eq!(
        update["text"],
        "Kept src/widget.rs for now. bob’s Codex sees that on their next edit."
    );
    f.run().await;
    // Bob hears that alice's agent kept the file; alice, who just said so, gets nothing new.
    let sent = f.sent();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[2].0 == "U2" && sent[2].1.starts_with("Another agent kept a file"));
    // The record says it was Slack, and bob's agent reads it as the person's answer.
    let told = f.listen(&f.bob, "b").handoffs;
    assert_eq!(told.len(), 1);
    assert_eq!((told[0].kind, told[0].via), ("decline", Via::Slack));
    let alice = f.owner(0);
    let handoffs = tokio::task::block_in_place(|| {
        f.store()
            .handoffs_of_owner(&alice, REPO, unix_now(), 10)
            .unwrap()
    });
    assert!(handoffs[0].answered_via_slack && !handoffs[0].answered_via_hub);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_the_hub_cannot_take_is_answered_privately_with_what_to_do() {
    let f = Fixture::new().await;
    f.contest();
    f.run().await;
    let message = f.sent_body("U1");
    let value = message["blocks"][2]["elements"][0]["value"].clone();

    // A button that is not one of ours.
    assert_eq!(
        f.click_value("U1", YES, &json!("not json"), &message).await,
        StatusCode::OK
    );
    let notice = f.call("/api/chat.postEphemeral").await;
    assert_eq!(
        notice["text"],
        "The Hub did not accept that answer. Nothing changed. Answer in the Hub instead."
    );

    // Alice's read-only token no longer works: the click names that and who can fix it.
    let alice = f.owner(0);
    let tokens = tokio::task::block_in_place(|| f.store().list_agent_tokens(&alice).unwrap());
    let binding = tokens.iter().find(|t| t.name == "slack").unwrap();
    tokio::task::block_in_place(|| f.store().revoke_agent_token(&binding.id).unwrap());
    f.calls.lock().unwrap().clear();
    assert_eq!(
        f.click_value("U1", YES, &value, &message).await,
        StatusCode::OK
    );
    let notice = f.call("/api/chat.postEphemeral").await;
    assert_eq!(
        notice["text"],
        "Your Slack link to the Hub has ended. Nothing changed. Ask whoever runs this Hub to renew your read-only Slack token, or answer in the Hub."
    );
    assert_eq!(
        (notice["channel"].as_str(), notice["user"].as_str()),
        (Some("D1"), Some("U1"))
    );
    // Nothing was answered: the request is still open.
    assert!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(path, _)| path != "/api/chat.update")
    );
    let handoffs = tokio::task::block_in_place(|| {
        f.store()
            .handoffs_of_owner(&alice, REPO, unix_now(), 10)
            .unwrap()
    });
    assert_eq!(handoffs[0].state, "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_message_update_still_reports_the_outcome() {
    let f = Fixture::new().await;
    f.contest();
    f.run().await;
    // The next Slack call after the click, the update, fails.
    f.replies
        .lock()
        .unwrap()
        .push((200, json!({"ok":false,"error":"message_not_found"})));
    assert_eq!(f.click("U1", YES).await, StatusCode::OK);
    // The two earlier direct messages are the first postMessage calls; the report is the third.
    let mut reported = None;
    for _ in 0..100 {
        reported = f
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/api/chat.postMessage")
            .map(|(_, body)| body.clone())
            .nth(2);
        if reported.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let reported = reported.expect("the outcome was reported in a new message");
    assert_eq!(reported["channel"], "U1");
    assert_eq!(
        reported["text"],
        "Handed src/widget.rs to bob’s Codex. They see it on their next edit. Slack could not update the original message, so its buttons may still show."
    );
}

#[test]
fn agents_are_named_the_way_the_hub_names_them() {
    for (name, said) in [
        ("claude", "Claude Code"),
        ("Claude-Code", "Claude Code"),
        ("codex", "Codex"),
        ("CURSOR", "Cursor"),
        ("copilot", "Copilot"),
        ("gemini", "Gemini"),
        ("grok", "Grok"),
        // A generated client token is told apart by its id.
        ("client-Jonnys-MacBook-Pro-d6442b23", "Agent d644"),
        ("client-mac-6A36AB", "Agent 6A36"),
        // Not a client token: too short an id, no host, or not hex.
        ("client-mac-6a36", "client-mac-6a36"),
        ("client--6a36ab", "client--6a36ab"),
        ("client-mac-6a36zz", "client-mac-6a36zz"),
        ("slack-live-test-agent", "slack-live-test-agent"),
        ("my \"agent\"\u{7}", "my 'agent'"),
    ] {
        assert_eq!(agent_name(name), said, "{name}");
    }
    assert_eq!(label("bob", "codex"), "bob’s Codex");
    assert_eq!(label("alice", "client-host-6a36ab"), "alice’s Agent 6a36");
}

#[test]
fn a_message_names_its_subject_once_and_keeps_text_as_typed() {
    let fields = [("File", "src/a.rs".to_owned())];
    let link = "https://hub.example.test/ui/#inbox";
    // A header that already names the file is the whole notification.
    assert_eq!(
        message("bob’s Codex edited src/a.rs", &fields, "g", None, link)["text"],
        "bob’s Codex edited src/a.rs"
    );
    assert_eq!(
        message("Your agent kept a file", &fields, "g", None, link)["text"],
        "Your agent kept a file: src/a.rs"
    );
    // `:rocket:` in a path or name is not turned into an emoji.
    let body = message(
        "h :rocket:",
        &[("File", ":tada:".to_owned())],
        ":smile:",
        None,
        link,
    );
    assert_eq!(body["blocks"][0]["text"]["emoji"], false);
    assert_eq!(body["blocks"][1]["fields"][0]["emoji"], false);
    assert_eq!(body["blocks"][3]["elements"][0]["emoji"], false);
}

#[test]
fn a_clicked_message_keeps_only_a_link_that_points_into_this_hub() {
    let hub = "https://hub.example.test/ui/";
    let actions = |url: &str| {
        json!({"type":"actions","elements":[
            {"type":"button","action_id":YES,"value":"{}","text":plain_text("Hand it over",30)},
            open_button(url)]})
    };
    let expected =
        json!({"type":"actions","elements":[open_button("https://hub.example.test/ui/#inbox")]});
    assert_eq!(
        kept(&actions("https://hub.example.test/ui/#inbox"), hub),
        Some(expected)
    );
    assert_eq!(
        kept(&actions("https://elsewhere.example.test/"), hub),
        Some(json!({"type":"actions","elements":[open_button(hub)]}))
    );
    // Without a link the buttons just go.
    let none = json!({"type":"actions","elements":[
        {"type":"button","action_id":NO,"value":"{}","text":plain_text("Keep it",30)}]});
    assert_eq!(kept(&none, hub), None);
}

#[test]
fn message_text_cannot_mention_or_link() {
    let body = message(
        "<!channel> & <http://x|y>",
        &[("File", "<@U1>".into())],
        "<!here>",
        None,
        "https://hub.example.test/ui/",
    );
    assert_eq!(
        body["text"],
        "&lt;!channel&gt; &amp; &lt;http://x|y&gt;: &lt;@U1&gt;"
    );
    rendered(&body);
}

#[test]
fn a_long_path_and_hostile_login_stay_plain_text_within_slack_limits() {
    let hostile = format!("<!channel>\u{7}*bold* <http://x|y> {}", "&".repeat(5000));
    let long = format!("src/{}.rs", "a/".repeat(3000));
    let paths: Vec<String> = (0..12).map(|n| format!("{long}{n}")).collect();
    let who = label(&hostile, &hostile);
    let fields = [
        files(&paths),
        ("Repository", sanitize(&hostile, TEXT_CHARS)),
        ("Your agent", sanitize(&hostile, NAME_CHARS)),
        ("Their agent", who.clone()),
    ];
    let headline = format!("Another agent edited {hostile}");
    let body = message(
        &headline,
        &fields,
        &hostile,
        Some(("token", None, &long)),
        "https://hub.example.test/ui/",
    );
    let blocks = body["blocks"].as_array().unwrap();
    let header = blocks[0]["text"]["text"].as_str().unwrap();
    assert!(header.chars().count() <= 150);
    let section = blocks[1]["fields"].as_array().unwrap();
    assert!(section.len() <= 10);
    for field in section {
        let text = field["text"].as_str().unwrap();
        assert!(text.chars().count() <= 2000);
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
    }
    assert!(section[0]["text"].as_str().unwrap().ends_with("\n+7 more"));
    // A button value past Slack's limit sends the message without the handoff buttons.
    assert_eq!(blocks[2]["elements"].as_array().unwrap().len(), 1);
    let text = body["text"].as_str().unwrap();
    assert!(!text.contains('<') && !text.contains('>'));
    assert!(!rendered(&body).contains('\u{7}'));
}

#[test]
fn manifest_requests_mentions_and_private_replies_without_history() {
    let manifest: Value =
        serde_json::from_str(include_str!("../../../examples/slack-manifest.json")).unwrap();
    assert_eq!(
        manifest["oauth_config"]["scopes"],
        json!({"bot":["chat:write","app_mentions:read"]})
    );
    assert_eq!(
        manifest["settings"]["event_subscriptions"]["bot_events"],
        json!(["app_mention", "tokens_revoked", "app_uninstalled"])
    );
    assert!(manifest["features"].get("slash_commands").is_none());
}

fn status_event(id: &str, user: &str, command: &str) -> Value {
    json!({"type":"event_callback","team_id":"T123","api_app_id":"A123","event_id":id,
        "event":{"type":"app_mention","user":user,"channel":"CSHARED","text":format!("<@UAPP> {command}")}})
}
fn status_text(f: &Fixture) -> Vec<(String, String)> {
    f.calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "/api/chat.postMessage")
        .map(|(_, b)| {
            (
                b["channel"].as_str().unwrap().into(),
                b["text"].as_str().unwrap().into(),
            )
        })
        .collect()
}
fn status_work(f: &Fixture, token: &str, session: &str) -> crate::hub::work::WorkItem {
    tokio::task::block_in_place(|| {
        f.store().work_start(token,&serde_json::from_value(json!({
        "title":"Synthetic review handover", "session":session,"request_key":format!("status-{session}"),"scope":[format!("src/{session}.rs")]
    })).unwrap(),unix_now()).unwrap().item
    })
}
fn status_checkpoint(
    f: &Fixture,
    token: &str,
    item: &crate::hub::work::WorkItem,
    kind: &str,
) -> crate::hub::work::WorkItem {
    tokio::task::block_in_place(|| {
        f.store().work_checkpoint(token,&serde_json::from_value(json!({
        "work_id":item.id,"operation_id":format!("synthetic-{kind}-{}",item.updated_seq),"generation":item.generation,
        "session":item.lead_session,"expected_updated_seq":item.updated_seq,"kind":kind,"summary":"Synthetic status change",
        "next_action":"Review the synthetic request","resolve_blocker":kind=="progress"
    })).unwrap(),unix_now()).unwrap().item
    })
}
fn scan_status(f: &Fixture, now: i64) {
    let (slack, state) = (f.slack(), f.state.clone());
    tokio::task::block_in_place(|| slack.collect_status_binding(&state, &slack.bindings[0], now))
        .unwrap();
}
fn policy() -> Value {
    json!({"enabled":true,"interval_seconds":900,"max_reminders":1,"escalation_user":"U2","escalation_after_seconds":900,"send_resolution":true})
}

fn restarted_slack(f: &Fixture) -> Arc<Slack> {
    let mut slack = Slack::from_file(&f.dir.path().join("config")).unwrap();
    slack.api = f.slack().api.clone();
    Arc::new(slack)
}

fn entry_states(f: &Fixture) -> Vec<String> {
    f.slack()
        .database()
        .unwrap()
        .prepare("SELECT state FROM status_jobs WHERE kind='entry' ORDER BY created,id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

async fn consume_entry(f: &Fixture) {
    assert!(f.slack().deliver_status(&f.state).await.unwrap());
    assert_eq!(entry_states(f), ["done"]);
    assert_eq!(status_text(f).len(), 1);
    f.calls.lock().unwrap().clear();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_blocked_is_once_per_episode_even_after_restart_and_job_expiry() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "entry-blocked");
    let blocked = status_checkpoint(&f, &f.alice, &work, "blocker");
    let now = unix_now();
    scan_status(&f, now);
    scan_status(&f, now + 1);
    let repeated = status_checkpoint(&f, &f.alice, &blocked, "blocker");
    scan_status(&f, now + 2);
    assert_eq!(entry_states(&f), ["queued"]);
    let (job, episode, cycle): (String, String, i64) = f
        .slack()
        .database()
        .unwrap()
        .query_row(
            "SELECT id,subject,cycle FROM status_jobs WHERE kind='entry'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(job, format!("{episode}:{cycle}:entry"));
    f.slack().deliver_status(&f.state).await.unwrap();
    let sent = status_text(&f);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "U1");
    assert!(sent[0].1.contains("Your agent's work is blocked"));
    {
        let calls = f.calls.lock().unwrap();
        let message = &calls
            .iter()
            .find(|(path, _)| path == "/api/chat.postMessage")
            .unwrap()
            .1;
        let visible = rendered(message);
        assert!(visible.contains("Blocked on: Synthetic status change"));
        assert!(visible.contains("Answer the blocker where your agent is running"));
        assert!(visible.contains(REPO));
        assert!(visible.contains("Claude Code session "));
        assert_eq!(
            message["blocks"][2]["elements"][0]["url"],
            format!("https://hub.example.test/ui/#work/{}", repeated.id)
        );
        assert_eq!(
            message["blocks"][2]["elements"].as_array().unwrap().len(),
            1
        );
    }
    let restarted = restarted_slack(&f);
    tokio::task::block_in_place(|| {
        restarted.collect_status_binding(&f.state, &restarted.bindings[0], now + 3)
    })
    .unwrap();
    assert!(!restarted.deliver_status(&f.state).await.unwrap());
    assert_eq!(entry_states(&f), ["done"]);
    restarted
        .database()
        .unwrap()
        .execute(
            "UPDATE status_jobs SET created=?1 WHERE kind='entry'",
            [now - 86401],
        )
        .unwrap();
    tokio::task::block_in_place(|| restarted.collect_status(&f.state, now + 61)).unwrap();
    assert!(entry_states(&f).is_empty());
    tokio::task::block_in_place(|| {
        restarted.collect_status_binding(&f.state, &restarted.bindings[0], now + 62)
    })
    .unwrap();
    assert!(entry_states(&f).is_empty());
    assert_eq!(status_text(&f).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_handover_uses_shared_owner_not_the_new_lead_or_other_readers() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "entry-owner");
    let offered = status_checkpoint(&f, &f.alice, &work, "handover");
    let claimed = tokio::task::block_in_place(|| f.store().work_start(&f.bob, &serde_json::from_value(json!({
        "work_id":offered.id,"takeover_generation":offered.generation,"title":offered.title,"session":"entry-new-lead","role":"lead"
    })).unwrap(), unix_now())).unwrap().item;
    assert_eq!(claimed.owner, work.owner);
    let offered = status_checkpoint(&f, &f.bob, &claimed, "handover");
    let mut slack = Slack::from_file(&f.dir.path().join("config")).unwrap();
    slack.api = f.slack().api.clone();
    slack.bindings[1].config.reminders.enabled = true;
    let slack = Arc::new(slack);
    for binding in &slack.bindings {
        tokio::task::block_in_place(|| slack.collect_status_binding(&f.state, binding, unix_now()))
            .unwrap();
    }
    assert_eq!(entry_states(&f), ["queued"]);
    slack.deliver_status(&f.state).await.unwrap();
    assert_eq!(
        status_text(&f),
        vec![(
            "U1".into(),
            "Work is offered for handover: Synthetic review handover".into()
        )]
    );
    let calls = f.calls.lock().unwrap();
    let body = &calls
        .iter()
        .find(|(path, _)| path == "/api/chat.postMessage")
        .unwrap()
        .1;
    assert!(rendered(body).contains("Handover note: Synthetic status change"));
    assert!(rendered(body).contains("Codex session "));
    assert!(rendered(body).contains("Pick an agent to take it over"));
    assert_eq!(
        body["blocks"][2]["elements"][0]["url"],
        format!("https://hub.example.test/ui/#work/{}", offered.id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_disabled_never_touches_the_status_journal() {
    let f = Fixture::new().await;
    let work = status_work(&f, &f.alice, "entry-disabled");
    status_checkpoint(&f, &f.alice, &work, "blocker");
    scan_status(&f, unix_now());
    f.run().await;
    let db = f.slack();
    let db = db.database().unwrap();
    for table in [
        "status_episodes",
        "status_jobs",
        "status_scans",
        "status_maintenance",
        "status_resolution_pending",
    ] {
        assert_eq!(
            db.query_row::<i64, _, _>(&format!("SELECT count(*) FROM {table}"), [], |row| row
                .get(0))
                .unwrap(),
            0,
            "{table}"
        );
    }
    assert!(status_text(&f).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_obsolete_state_and_generation_never_send() {
    for change in ["progress", "handover", "generation", "retry"] {
        let f = Fixture::configured(Some(policy()), false).await;
        let work = status_work(&f, &f.alice, "entry-obsolete");
        let blocked = status_checkpoint(&f, &f.alice, &work, "blocker");
        scan_status(&f, unix_now());
        if change == "retry" {
            f.replies.lock().unwrap().push((429, json!({"ok":false})));
            assert!(f.slack().deliver_status(&f.state).await.unwrap());
            f.slack()
                .database()
                .unwrap()
                .execute("UPDATE status_jobs SET due=0", [])
                .unwrap();
            f.calls.lock().unwrap().clear();
        }
        if change == "generation" {
            let offered = status_checkpoint(&f, &f.alice, &blocked, "handover");
            let claimed = tokio::task::block_in_place(|| f.store().work_start(&f.alice, &serde_json::from_value(json!({
                "work_id":offered.id,"takeover_generation":offered.generation,"title":offered.title,"session":"entry-takeover","role":"lead"
            })).unwrap(), unix_now())).unwrap().item;
            assert!(claimed.generation > blocked.generation);
            status_checkpoint(&f, &f.alice, &claimed, "blocker");
        } else {
            status_checkpoint(
                &f,
                &f.alice,
                &blocked,
                if change == "retry" {
                    "progress"
                } else {
                    change
                },
            );
        }
        assert!(
            !f.slack().deliver_status(&f.state).await.unwrap(),
            "{change}"
        );
        assert_eq!(entry_states(&f), ["suppressed"], "{change}");
        assert!(status_text(&f).is_empty(), "{change}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_rechecks_cycle_episode_opt_in_user_and_shared_recipient() {
    for fence in ["cycle", "episode", "opt-in", "user", "recipient"] {
        let f = Fixture::configured(Some(policy()), false).await;
        let work = status_work(&f, &f.alice, "entry-fences");
        status_checkpoint(&f, &f.alice, &work, "blocker");
        scan_status(&f, unix_now());
        let mut slack = Slack::from_file(&f.dir.path().join("config")).unwrap();
        slack.api = f.slack().api.clone();
        if fence == "opt-in" {
            slack.bindings[0].config.reminders.enabled = false;
        }
        if fence == "recipient" {
            slack.bindings[1].config.reminders.enabled = true;
        }
        let db = slack.database().unwrap();
        match fence {
            "cycle" => {
                db.execute("UPDATE status_jobs SET cycle=cycle+1", [])
                    .unwrap();
            }
            "episode" => {
                db.execute("UPDATE status_episodes SET state='resolved'", [])
                    .unwrap();
            }
            "opt-in" => {}
            "user" => {
                db.execute("UPDATE status_jobs SET user='U2'", []).unwrap();
            }
            "recipient" => {
                let key = status::binding_key(&slack.bindings[1]);
                db.execute("UPDATE status_jobs SET binding=?1,user='U2'", [&key])
                    .unwrap();
                db.execute("UPDATE status_episodes SET binding=?1", [&key])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        drop(db);
        assert!(
            !Arc::new(slack).deliver_status(&f.state).await.unwrap(),
            "{fence}"
        );
        assert_eq!(entry_states(&f), ["suppressed"], "{fence}");
        assert!(status_text(&f).is_empty(), "{fence}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_revoked_rotated_or_removed_binding_never_sends() {
    for change in ["revoke", "rotate", "remove"] {
        let f = Fixture::configured(Some(policy()), false).await;
        let work = status_work(&f, &f.alice, "entry-binding");
        status_checkpoint(&f, &f.alice, &work, "blocker");
        scan_status(&f, unix_now());
        let mut slack = Slack::from_file(&f.dir.path().join("config")).unwrap();
        if change == "remove" {
            slack.bindings.clear();
        } else {
            let principal =
                tokio::task::block_in_place(|| slack.principal(&f.state, &slack.bindings[0]))
                    .unwrap();
            if change == "revoke" {
                tokio::task::block_in_place(|| f.store().revoke_agent_token(&principal.token_id))
                    .unwrap();
            } else {
                let owner = f.owner(0);
                let (_, secret, _) = tokio::task::block_in_place(|| {
                    mint_bound_agent_token(&**f.store(), &owner, "rotated", REPO, 101, 3600, true)
                })
                .unwrap();
                private(&f.dir.path().join("alice"), secret.expose());
                slack = Slack::from_file(&f.dir.path().join("config")).unwrap();
            }
        }
        slack.api = f.slack().api.clone();
        assert!(
            !Arc::new(slack).deliver_status(&f.state).await.unwrap(),
            "{change}"
        );
        assert_eq!(entry_states(&f), ["suppressed"], "{change}");
        assert!(f.calls.lock().unwrap().is_empty(), "{change}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_unbound_lead_and_nonwaiting_work_never_queue() {
    let f = Fixture::configured(Some(policy()), false).await;
    let (_, unbound) =
        tokio::task::block_in_place(|| person(&**f.store(), f.dir.path(), 3, "charlie", "cursor"));
    let work = status_work(&f, &unbound, "entry-unbound");
    status_checkpoint(&f, &unbound, &work, "blocker");
    for kind in ["progress", "pause", "finish"] {
        let work = status_work(&f, &f.alice, &format!("entry-{kind}"));
        status_checkpoint(&f, &f.alice, &work, kind);
    }
    let now = unix_now();
    scan_status(&f, now);
    scan_status(&f, now + 1800);
    assert!(entry_states(&f).is_empty());
    assert_eq!(
        f.slack()
            .database()
            .unwrap()
            .query_row::<i64, _, _>("SELECT count(*) FROM status_episodes", [], |row| row.get(0))
            .unwrap(),
        0
    );
    assert!(!f.slack().deliver_status(&f.state).await.unwrap());
    assert!(status_text(&f).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_retries_rate_limits_once_and_bounds_permanent_failure() {
    for fails in [1, MAX_ATTEMPTS] {
        let f = Fixture::configured(Some(policy()), false).await;
        let work = status_work(&f, &f.alice, "entry-retry");
        status_checkpoint(&f, &f.alice, &work, "blocker");
        scan_status(&f, unix_now());
        for _ in 0..fails {
            f.replies
                .lock()
                .unwrap()
                .push((if fails == 1 { 429 } else { 503 }, json!({"ok":false})));
            assert!(f.slack().deliver_status(&f.state).await.unwrap());
            f.slack()
                .database()
                .unwrap()
                .execute("UPDATE status_jobs SET due=0", [])
                .unwrap();
        }
        if fails == 1 {
            assert_eq!(entry_states(&f), ["queued"]);
            assert!(restarted_slack(&f).deliver_status(&f.state).await.unwrap());
            assert_eq!(entry_states(&f), ["done"]);
        } else {
            assert_eq!(entry_states(&f), ["failed"]);
        }
        let attempts = status_text(&f).len();
        assert_eq!(attempts, if fails == 1 { 2 } else { MAX_ATTEMPTS as usize });
        scan_status(&f, unix_now());
        assert!(!restarted_slack(&f).deliver_status(&f.state).await.unwrap());
        assert_eq!(status_text(&f).len(), attempts);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_new_state_and_confirmed_reentry_start_new_cycles() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "entry-cycles");
    let blocked = status_checkpoint(&f, &f.alice, &work, "blocker");
    let now = unix_now();
    scan_status(&f, now);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0 WHERE state='queued'", [])
        .unwrap();
    f.slack().deliver_status(&f.state).await.unwrap();
    let active = status_checkpoint(&f, &f.alice, &blocked, "progress");
    scan_status(&f, now + 1);
    let blocked = status_checkpoint(&f, &f.alice, &active, "blocker");
    scan_status(&f, now + 2);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0 WHERE state='queued'", [])
        .unwrap();
    f.slack().deliver_status(&f.state).await.unwrap();
    let offered = status_checkpoint(&f, &f.alice, &blocked, "handover");
    scan_status(&f, now + 3);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0 WHERE state='queued'", [])
        .unwrap();
    f.slack().deliver_status(&f.state).await.unwrap();
    status_checkpoint(&f, &f.alice, &offered, "handover");
    scan_status(&f, now + 4);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0 WHERE state='queued'", [])
        .unwrap();
    assert!(!f.slack().deliver_status(&f.state).await.unwrap());
    assert_eq!(status_text(&f).len(), 3);
    assert_eq!(entry_states(&f), ["done", "done", "done"]);
    assert_eq!(
        f.slack()
            .database()
            .unwrap()
            .query_row::<(i64, i64), _, _>(
                "SELECT count(*),max(cycle) FROM status_episodes",
                [],
                |row| Ok((row.get(0)?, row.get(1)?))
            )
            .unwrap(),
        (2, 2)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_does_not_reset_reminder_quota_clock_or_escalation() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "entry-cadence");
    status_checkpoint(&f, &f.alice, &work, "blocker");
    let now = unix_now();
    scan_status(&f, now);
    f.slack().deliver_status(&f.state).await.unwrap();
    assert_eq!(
        f.slack()
            .database()
            .unwrap()
            .query_row::<(i64, i64, bool), _, _>(
                "SELECT delivered,next_due,escalated FROM status_episodes",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            )
            .unwrap(),
        (0, now + 900, false)
    );
    scan_status(&f, now + 899);
    assert!(!f.slack().deliver_status(&f.state).await.unwrap());
    scan_status(&f, now + 900);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver_status(&f.state).await.unwrap();
    assert!(status_text(&f)[1].1.contains("Outstanding action reminder"));
    scan_status(&f, now + 901);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver_status(&f.state).await.unwrap();
    assert_eq!(status_text(&f)[2].0, "U2");
    assert!(status_text(&f)[2].1.contains("Escalation requested"));
    scan_status(&f, now + 1801);
    assert!(!f.slack().deliver_status(&f.state).await.unwrap());
    assert_eq!(status_text(&f).len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_preserves_legacy_active_episodes_without_retroactive_messages() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "entry-legacy");
    status_checkpoint(&f, &f.alice, &work, "blocker");
    scan_status(&f, unix_now());
    f.slack()
        .database()
        .unwrap()
        .execute("DELETE FROM status_jobs WHERE kind='entry'", [])
        .unwrap();
    let slack = restarted_slack(&f);
    tokio::task::block_in_place(|| {
        slack.collect_status_binding(&f.state, &slack.bindings[0], unix_now())
    })
    .unwrap();
    assert!(entry_states(&f).is_empty());
    assert!(!slack.deliver_status(&f.state).await.unwrap());
    assert!(status_text(&f).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_full_job_journal_rolls_back_episode_and_recovers_atomically() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "entry-capacity");
    status_checkpoint(&f, &f.alice, &work, "blocker");
    let slack = f.slack();
    let now = unix_now();
    {
        let mut db = slack.database().unwrap();
        let tx = db.transaction().unwrap();
        for number in 0..CAPACITY {
            tx.execute("INSERT INTO status_jobs(id,binding,user,kind,state,created,due) VALUES(?1,'synthetic','U1','query','done',?2,?2)", params![format!("history-{number}"),now]).unwrap();
        }
        tx.commit().unwrap();
    }
    assert!(matches!(
        tokio::task::block_in_place(|| slack.collect_status_binding(
            &f.state,
            &slack.bindings[0],
            now
        )),
        Err(HubError::Busy)
    ));
    assert_eq!(
        slack
            .database()
            .unwrap()
            .query_row::<i64, _, _>("SELECT count(*) FROM status_episodes", [], |row| row.get(0))
            .unwrap(),
        0
    );
    assert!(!slack.deliver_status(&f.state).await.unwrap());
    assert!(entry_states(&f).is_empty());
    slack
        .database()
        .unwrap()
        .execute("DELETE FROM status_jobs WHERE id='history-0'", [])
        .unwrap();
    scan_status(&f, now + 1);
    scan_status(&f, now + 2);
    assert_eq!(entry_states(&f), ["queued"]);
    slack
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    slack.deliver_status(&f.state).await.unwrap();
    assert_eq!(entry_states(&f), ["done"]);
    assert_eq!(status_text(&f).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_shared_status_failure_defers_and_recovers_without_duplicate_delivery() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "entry-unavailable");
    status_checkpoint(&f, &f.alice, &work, "blocker");
    scan_status(&f, unix_now());
    tokio::task::block_in_place(|| {
        f.store()
            .append_notices("edit_repeated", &[(f.alice.clone(), "not-json".into())])
    })
    .unwrap();
    assert!(!f.slack().deliver_status(&f.state).await.unwrap());
    assert!(f.calls.lock().unwrap().is_empty());
    assert_eq!(entry_states(&f), ["queued"]);
    let (attempts, due): (i64, i64) = f
        .slack()
        .database()
        .unwrap()
        .query_row(
            "SELECT attempts,due FROM status_jobs WHERE kind='entry'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(attempts, 1);
    assert!(due > unix_now());
    tokio::task::block_in_place(|| {
        f.store()
            .append_notices("edit_repeated", &vec![(f.alice.clone(), "{}".into()); 100])
    })
    .unwrap();
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    assert!(f.slack().deliver_status(&f.state).await.unwrap());
    assert_eq!(entry_states(&f), ["done"]);
    assert!(!restarted_slack(&f).deliver_status(&f.state).await.unwrap());
    assert_eq!(status_text(&f).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_unobserved_clear_and_reentry_remains_one_episode() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "entry-between-scans");
    let blocked = status_checkpoint(&f, &f.alice, &work, "blocker");
    scan_status(&f, unix_now());
    let active = status_checkpoint(&f, &f.alice, &blocked, "progress");
    status_checkpoint(&f, &f.alice, &active, "blocker");
    scan_status(&f, unix_now());
    assert_eq!(entry_states(&f), ["queued"]);
    assert_eq!(
        f.slack()
            .database()
            .unwrap()
            .query_row::<i64, _, _>("SELECT cycle FROM status_episodes", [], |row| row.get(0))
            .unwrap(),
        1
    );
    assert!(f.slack().deliver_status(&f.state).await.unwrap());
    assert!(!f.slack().deliver_status(&f.state).await.unwrap());
    assert_eq!(status_text(&f).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mention_status_is_private_useful_deduplicated_and_independent_of_prose() {
    let f = Fixture::new().await;
    let work = status_work(&f, &f.alice, "status-query");
    status_checkpoint(&f, &f.alice, &work, "handover");
    let event = status_event("EvSTATUS1", "U1", "status");
    assert_eq!(f.post(event.to_string(), true).await.0, StatusCode::OK);
    assert_eq!(f.post(event.to_string(), true).await.0, StatusCode::OK);
    f.run().await;
    let sent = status_text(&f);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "U1");
    assert!(sent[0].1.contains("awaiting a named party"));
    assert!(sent[0].1.contains("Review the handover"));
    assert!(sent[0].1.contains("Evidence revision"));
    assert!(sent[0].1.contains("Coverage incomplete"));
    assert!(!sent[0].1.contains("waiting for another agent"));
    let db = f.slack();
    let db = db.database().unwrap();
    let columns: Vec<String> = db
        .prepare("PRAGMA table_info(status_jobs)")
        .unwrap()
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(!columns.iter().any(|c| c == "text" || c == "body"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mention_rejects_unbound_wrong_scope_and_bot_without_channel_disclosure() {
    let f = Fixture::new().await;
    for (id, user, command) in [
        ("EvUNKNOWN", "U9", "status"),
        ("EvWRONG", "U1", "status acme/private"),
        ("EvCHAT", "U1", "tell me a story"),
    ] {
        assert_eq!(
            f.post(status_event(id, user, command).to_string(), true)
                .await
                .0,
            StatusCode::OK
        );
    }
    let mut bot = status_event("EvBOT", "U1", "status");
    bot["event"]["bot_id"] = json!("BOTHER");
    assert_eq!(f.post(bot.to_string(), true).await.0, StatusCode::OK);
    let mut wrong = status_event("EvTEAM", "U1", "status");
    wrong["team_id"] = json!("TOTHER");
    assert_eq!(
        f.post(wrong.to_string(), true).await.0,
        StatusCode::FORBIDDEN
    );
    f.run().await;
    assert!(status_text(&f).is_empty());
    assert!(
        !f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|(_, body)| body["channel"] == "CSHARED")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_retry_recomputes_facts_and_rechecks_binding() {
    let f = Fixture::new().await;
    let work = status_work(&f, &f.alice, "status-retry");
    let blocked = status_checkpoint(&f, &f.alice, &work, "blocker");
    f.post(status_event("EvRETRY", "U1", "status").to_string(), true)
        .await;
    f.replies.lock().unwrap().push((429, json!({"ok":false})));
    f.slack().deliver(&f.state).await.unwrap();
    status_checkpoint(&f, &f.alice, &blocked, "progress");
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    let sent = status_text(&f);
    assert_eq!(sent.len(), 2);
    assert!(sent[0].1.contains("[blocked;"));
    assert!(sent[1].1.contains("[active;"));
    f.post(status_event("EvREVOKE", "U1", "status").to_string(), true)
        .await;
    let owner = f.owner(0);
    let token = tokio::task::block_in_place(|| f.store().list_agent_tokens(&owner))
        .unwrap()
        .into_iter()
        .find(|t| t.name == "slack")
        .unwrap();
    tokio::task::block_in_place(|| f.store().revoke_agent_token(&token.id)).unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    assert_eq!(status_text(&f).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opt_in_reminders_retry_deduplicate_escalate_and_resolve() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "reminder");
    let blocked = status_checkpoint(&f, &f.alice, &work, "blocker");
    let now = unix_now();
    scan_status(&f, now);
    consume_entry(&f).await;
    scan_status(&f, now + 901);
    scan_status(&f, now + 901);
    // Synthetic clock advances scheduling only; make the queued jobs due for this worker.
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.replies.lock().unwrap().push((429, json!({"ok":false})));
    f.slack().deliver(&f.state).await.unwrap();
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    scan_status(&f, now + 902);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    let sent = status_text(&f);
    assert_eq!(sent.len(), 3); // first API failure, successful retry, one escalation
    assert_eq!(sent.iter().filter(|(u, _)| u == "U2").count(), 1);
    scan_status(&f, now + 903);
    f.slack().deliver(&f.state).await.unwrap();
    assert_eq!(status_text(&f).len(), 3);
    status_checkpoint(&f, &f.alice, &blocked, "progress");
    scan_status(&f, now + 904);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    let sent = status_text(&f);
    assert_eq!(sent.len(), 4);
    assert!(
        sent.last()
            .unwrap()
            .1
            .contains("does not establish task success")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_escalation_recipient_does_not_block_later_status_queries() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "revoked-escalation");
    status_checkpoint(&f, &f.alice, &work, "blocker");
    let now = unix_now();
    scan_status(&f, now);
    consume_entry(&f).await;
    scan_status(&f, now + 901);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    scan_status(&f, now + 902);
    tokio::task::block_in_place(|| {
        let slack = f.slack();
        let target = slack.principal(&f.state, &slack.bindings[1]).unwrap();
        assert!(f.state.store.revoke_agent_token(&target.token_id).unwrap());
    });
    f.slack()
        .database()
        .unwrap()
        .execute(
            "UPDATE status_jobs SET due=0,created=?1 WHERE kind='escalation'",
            [now - 1],
        )
        .unwrap();
    f.post(
        status_event("EvAFTERREVOKE", "U1", "status").to_string(),
        true,
    )
    .await;
    let sent_before = status_text(&f).len();
    f.slack().deliver(&f.state).await.unwrap();
    assert_eq!(status_text(&f).len(), sent_before);
    assert_eq!(
        f.slack()
            .database()
            .unwrap()
            .query_row::<String, _, _>(
                "SELECT state FROM status_jobs WHERE kind='escalation'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        "suppressed"
    );
    f.slack().deliver(&f.state).await.unwrap();
    let sent = status_text(&f);
    assert_eq!(sent.len(), sent_before + 1);
    assert_eq!(sent.last().unwrap().0, "U1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_action_suppresses_reminder_before_delivery_and_quiet_work_is_not_reminded() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "suppressed");
    let blocked = status_checkpoint(&f, &f.alice, &work, "blocker");
    let now = unix_now();
    scan_status(&f, now);
    scan_status(&f, now + 901);
    status_checkpoint(&f, &f.alice, &blocked, "progress");
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    assert!(status_text(&f).is_empty());
    let quiet = status_work(&f, &f.alice, "quiet");
    status_checkpoint(&f, &f.alice, &quiet, "pause");
    scan_status(&f, now + 2000);
    scan_status(&f, now + 2500);
    assert_eq!(
        f.slack()
            .database()
            .unwrap()
            .query_row::<i64, _, _>("SELECT count(*) FROM status_episodes", [], |r| r.get(0))
            .unwrap(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_name_forge_bindings_do_not_hide_the_second_forge_request() {
    let f = Fixture::configured(None, true).await;
    let work = status_work(&f, &f.bob, "other-forge");
    let paused = status_checkpoint(&f, &f.bob, &work, "pause");
    let messages = tokio::task::block_in_place(|| {
        let slack = f.slack();
        let people = slack.people(&f.state).unwrap();
        slack.asked(&f.state,&people,&json!({"requester":f.alice,"targets":[paused.id],"proposal":{"scope":["src/other-forge.rs"]}})).unwrap()
    });
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, "U2");
    f.post(status_event("EvFORGE", "U1", "status").to_string(), true)
        .await;
    f.slack().deliver(&f.state).await.unwrap();
    assert!(!status_text(&f)[0].1.contains("Synthetic review handover"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_queued_messages_without_exact_binding_identity_are_suppressed() {
    let f = Fixture::new().await;
    f.slack().database().unwrap().execute("INSERT INTO messages(id,user,body,state,created,due) VALUES('legacy','U1','{}','queued',0,0)",[]).unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    assert!(f.calls.lock().unwrap().is_empty());
    assert_eq!(f.state_of_messages(), vec!["failed"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reminders_are_off_by_default_and_failed_retry_does_not_outlive_its_action() {
    let f = Fixture::new().await;
    let work = status_work(&f, &f.alice, "default-off");
    status_checkpoint(&f, &f.alice, &work, "blocker");
    f.run().await;
    assert_eq!(
        f.slack()
            .database()
            .unwrap()
            .query_row::<i64, _, _>("SELECT count(*) FROM status_episodes", [], |r| r.get(0))
            .unwrap(),
        0
    );
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "retry-obsolete");
    let blocked = status_checkpoint(&f, &f.alice, &work, "blocker");
    let now = unix_now();
    scan_status(&f, now);
    consume_entry(&f).await;
    scan_status(&f, now + 901);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.replies.lock().unwrap().push((429, json!({"ok":false})));
    f.slack().deliver(&f.state).await.unwrap();
    status_checkpoint(&f, &f.alice, &blocked, "progress");
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    assert_eq!(status_text(&f).len(), 1);
    assert_eq!(
        f.slack()
            .database()
            .unwrap()
            .query_row::<String, _, _>(
                "SELECT state FROM status_jobs WHERE kind='reminder'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        "suppressed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_binding_rotation_suppresses_old_coordination_jobs_and_attempts_are_bounded() {
    let f = Fixture::new().await;
    f.contest();
    tokio::task::block_in_place(|| f.slack().collect(&f.state)).unwrap();
    let owner = f.owner(0);
    let (_, secret, _) = tokio::task::block_in_place(|| {
        mint_bound_agent_token(&*f.state.store, &owner, "rotated", REPO, 101, 3600, true)
    })
    .unwrap();
    private(&f.dir.path().join("alice"), secret.expose());
    let changed = Slack::from_file(&f.dir.path().join("config")).unwrap();
    // The other person's queued job may be selected first. Drain until the rotated recipient
    // is encountered without making network requests.
    for _ in 0..2 {
        let _ = tokio::task::block_in_place(|| changed.next_message(&f.state)).unwrap();
    }
    let state: String = changed
        .database()
        .unwrap()
        .query_row("SELECT state FROM messages WHERE user='U1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "failed");
    f.post(status_event("EvLIMIT", "U2", "status").to_string(), true)
        .await;
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET attempts=8", [])
        .unwrap();
    assert!(
        tokio::task::block_in_place(|| f.slack().prepare_status_job(&f.state, unix_now()))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.slack()
            .database()
            .unwrap()
            .query_row::<String, _, _>("SELECT state FROM status_jobs", [], |r| r.get(0))
            .unwrap(),
        "failed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reminder_policy_requires_bounded_cadence_and_exact_authorized_escalation_scope() {
    let f = Fixture::new().await;
    let mut config: SlackConfig =
        serde_json::from_str(&fs::read_to_string(f.dir.path().join("config")).unwrap()).unwrap();
    config.bindings[0].reminders.interval_seconds = 0;
    assert!(validate_config(&config).is_err());
    config.bindings[0].reminders = serde_json::from_value(policy()).unwrap();
    config.bindings[1].host = "other.example.test".into();
    assert!(validate_config(&config).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directed_request_reminder_resolves_only_on_exact_retained_withdrawal() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "request-target");
    status_checkpoint(&f, &f.alice, &work, "pause");
    let request=tokio::task::block_in_place(||f.store().work_start_auto(&f.bob,&serde_json::from_value(json!({
        "title":"Synthetic directed requester","session":"requesting","scope":["src/request-target.rs"]
    })).unwrap(),unix_now())).unwrap().coordination_request.unwrap();
    let now = unix_now();
    scan_status(&f, now);
    assert!(entry_states(&f).is_empty());
    scan_status(&f, now + 901);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    assert!(
        status_text(&f)[0]
            .1
            .contains("directed coordination request")
    );
    tokio::task::block_in_place(|| {
        f.store().coordination_withdraw(
            &f.bob,
            &crate::hub::coordination::CoordinationWithdraw {
                request_id: request.id,
                operation_id: "synthetic-withdraw".into(),
                session: "requesting".into(),
                expected_updated_seq: request.updated_seq,
                target: None,
                via_hub: false,
            },
            unix_now(),
        )
    })
    .unwrap();
    scan_status(&f, now + 902);
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    assert!(
        status_text(&f)
            .last()
            .unwrap()
            .1
            .contains("no longer outstanding")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_lead_generation_suppresses_the_former_recipients_reminder() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "new-recipient");
    let blocked = status_checkpoint(&f, &f.alice, &work, "blocker");
    let now = unix_now();
    scan_status(&f, now);
    scan_status(&f, now + 901);
    let offered = status_checkpoint(&f, &f.alice, &blocked, "handover");
    let claimed=tokio::task::block_in_place(||f.store().work_start(&f.bob,&serde_json::from_value(json!({
        "work_id":offered.id,"takeover_generation":offered.generation,"title":"Synthetic review handover","session":"new-lead","role":"lead"
    })).unwrap(),unix_now())).unwrap().item;
    assert_eq!(claimed.lead_token, f.bob);
    status_checkpoint(&f, &f.bob, &claimed, "blocker");
    f.slack()
        .database()
        .unwrap()
        .execute("UPDATE status_jobs SET due=0", [])
        .unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    f.slack().deliver(&f.state).await.unwrap();
    assert!(status_text(&f).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_status_answers_unknown_privately_and_does_not_block_coordination_collection() {
    let f = Fixture::configured(Some(policy()), false).await;
    f.contest();
    f.post(
        status_event("EvUNAVAILABLE", "U1", "status").to_string(),
        true,
    )
    .await;
    // Synthetic malformed retained input exercises a failed evidence read, not a clear result.
    tokio::task::block_in_place(|| {
        f.store()
            .append_notices("edit_repeated", &[(f.alice.clone(), "not-json".into())])
    })
    .unwrap();
    f.run().await;
    let sent = status_text(&f);
    assert!(sent.iter().any(|(u, text)| u == "U1"
        && text.contains("Shared status is unavailable")
        && text.contains("No clear status")));
    assert!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|(_, body)| body["blocks"][0]["type"] == "header")
    );
    assert!(!sent.iter().any(|(u, _)| u == "CSHARED"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_status_drops_resolved_repeated_edits_and_keeps_new_live_contention() {
    use crate::hub::work_http::actionable_status;
    for resolution in ["grant", "release", "end"] {
        let f = Fixture::new().await;
        f.contest();
        let read = || {
            tokio::task::block_in_place(|| {
                let slack = f.slack();
                let who = slack.principal(&f.state, &slack.bindings[0]).unwrap();
                actionable_status::read(&**f.store(), &f.state.onboarding, &who, unix_now())
                    .unwrap()
            })
        };
        assert!(
            read()
                .actions
                .iter()
                .any(|a| a.kind == "file_conflict" && a.needs_you)
        );
        tokio::task::block_in_place(|| match resolution {
            "grant" => {
                f.store()
                    .answer_handoff(&f.alice, Some("a"), PATH, true, None, unix_now())
                    .unwrap();
            }
            "release" => {
                let held = f.store().live_intents(REPO, unix_now(), 100).unwrap();
                f.store()
                    .release_intent(
                        &f.alice,
                        &held.iter().find(|i| i.token == f.alice).unwrap().id,
                        unix_now(),
                    )
                    .unwrap();
            }
            _ => {
                f.store()
                    .end_session(&f.bob, Some("b"), unix_now())
                    .unwrap();
            }
        });
        assert!(
            !read().actions.iter().any(|a| a.kind == "file_conflict"),
            "obsolete repeat after {resolution}"
        );
        tokio::task::block_in_place(|| {
            f.store()
                .end_session(&f.alice, Some("a"), unix_now())
                .unwrap();
            f.store()
                .end_session(&f.bob, Some("b"), unix_now())
                .unwrap();
        });
        f.contest();
        assert!(
            read()
                .actions
                .iter()
                .any(|a| a.kind == "file_conflict" && a.needs_you),
            "new repeat after {resolution}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn episode_maintenance_rotates_past_one_hundred_outstanding_subjects() {
    let f = Fixture::configured(Some(policy()), false).await;
    let now = unix_now();
    let slack = f.slack();
    let key = status::binding_key(&slack.bindings[0]);
    // Retained subjects are confirmed individually even when the status list is bounded.
    for n in 0..101 {
        let work = status_work(&f, &f.alice, &format!("fair-{n}"));
        let current = if n < 100 {
            status_checkpoint(&f, &f.alice, &work, "blocker")
        } else {
            work
        };
        let subject = json!({"kind":"work","id":current.id,"generation":current.generation,"state":"blocked"}).to_string();
        slack.database().unwrap().execute("INSERT INTO status_episodes(id,binding,subject,cycle,state,first_seen,next_due) VALUES(?1,?2,?3,1,'active',?4,?5)",params![format!("{n:04}"),key,subject,now,now+86400]).unwrap();
    }
    scan_status(&f, now);
    let state = || {
        slack
            .database()
            .unwrap()
            .query_row::<String, _, _>(
                "SELECT state FROM status_episodes WHERE id='0100'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(state(), "active");
    scan_status(&f, now + 1);
    assert_eq!(state(), "resolved");
    assert_eq!(
        slack
            .database()
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM status_episodes WHERE length(id)=4 AND id<'0100' AND state='active'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        100
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_episode_journal_resolves_before_new_subject_admission_and_recovers_after_retention() {
    let f = Fixture::configured(Some(policy()), false).await;
    let old = status_work(&f, &f.alice, "old-cleared");
    let fresh = status_work(&f, &f.alice, "new-blocked");
    status_checkpoint(&f, &f.alice, &fresh, "blocker");
    let now = unix_now();
    let slack = f.slack();
    let key = status::binding_key(&slack.bindings[0]);
    let subject = json!({"kind":"work","id":old.id,"generation":old.generation,"state":"blocked"})
        .to_string();
    {
        let mut db = slack.database().unwrap();
        let tx = db.transaction().unwrap();
        for n in 0..4096 {
            let encoded = if n == 0 {
                subject.clone()
            } else {
                json!({"kind":"work","id":format!("unknown-{n}"),"generation":1,"state":"blocked"})
                    .to_string()
            };
            tx.execute("INSERT INTO status_episodes(id,binding,subject,cycle,state,first_seen,next_due) VALUES(?1,?2,?3,1,'active',?4,?5)",params![format!("{n:04}"),key,encoded,now,now+86400]).unwrap();
        }
        tx.commit().unwrap();
    }
    let result = tokio::task::block_in_place(|| {
        slack.collect_status_binding(&f.state, &slack.bindings[0], now)
    });
    assert!(matches!(result, Err(HubError::Busy)));
    assert!(entry_states(&f).is_empty());
    assert!(!slack.deliver_status(&f.state).await.unwrap());
    assert!(status_text(&f).is_empty());
    assert_eq!(
        slack
            .database()
            .unwrap()
            .query_row::<String, _, _>(
                "SELECT state FROM status_episodes WHERE id='0000'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        "resolved"
    );
    // Retained history still consumes its quota until expiry; Unknown subjects stay active.
    assert_eq!(
        slack
            .database()
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM status_episodes WHERE state='active'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        4095
    );
    // Advance only the local journal's retention age; forge credentials keep their real clock.
    slack
        .database()
        .unwrap()
        .execute(
            "UPDATE status_episodes SET first_seen=?1 WHERE id='0000'",
            [now - 8 * 86400],
        )
        .unwrap();
    tokio::task::block_in_place(|| slack.collect_status(&f.state, now + 61)).unwrap();
    assert_eq!(
        slack
            .database()
            .unwrap()
            .query_row::<i64, _, _>("SELECT count(*) FROM status_episodes", [], |r| r.get(0))
            .unwrap(),
        4096
    );
    assert_eq!(
        slack
            .database()
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM status_episodes WHERE id='0000'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_status_queue_retains_resolution_progress_without_crossing_reactivated_cycles() {
    let f = Fixture::configured(Some(policy()), false).await;
    let work = status_work(&f, &f.alice, "queue-resolution");
    let blocked = status_checkpoint(&f, &f.alice, &work, "blocker");
    let now = unix_now();
    scan_status(&f, now);
    consume_entry(&f).await;
    let slack = f.slack();
    let key = status::binding_key(&slack.bindings[0]);
    {
        let mut db = slack.database().unwrap();
        let tx = db.transaction().unwrap();
        // Synthetic completed jobs still occupy dedup history until its retention window ends.
        for n in 1..CAPACITY {
            tx.execute("INSERT INTO status_jobs(id,binding,user,kind,state,created,due) VALUES(?1,?2,'U1','query','done',?3,?3)",params![format!("history-{n}"),key,now]).unwrap();
        }
        tx.execute("UPDATE status_episodes SET delivered=1", [])
            .unwrap();
        tx.commit().unwrap();
    }
    let active = status_checkpoint(&f, &f.alice, &blocked, "progress");
    scan_status(&f, now + 1);
    let db = slack.database().unwrap();
    assert_eq!(
        db.query_row::<String, _, _>("SELECT state FROM status_episodes", [], |r| r.get(0))
            .unwrap(),
        "resolved"
    );
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM status_resolution_pending", [], |r| r
            .get(0))
            .unwrap(),
        1
    );
    drop(db);
    status_checkpoint(&f, &f.alice, &active, "blocker");
    assert!(matches!(
        tokio::task::block_in_place(|| slack.collect_status_binding(
            &f.state,
            &slack.bindings[0],
            now + 2
        )),
        Err(HubError::Busy)
    ));
    let db = slack.database().unwrap();
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT cycle FROM status_episodes", [], |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM status_resolution_pending", [], |r| r
            .get(0))
            .unwrap(),
        1
    );
    db.execute("DELETE FROM status_jobs WHERE id='history-1'", [])
        .unwrap();
    drop(db);
    scan_status(&f, now + 3);
    assert_eq!(
        slack
            .database()
            .unwrap()
            .query_row::<i64, _, _>("SELECT cycle FROM status_episodes", [], |row| row.get(0))
            .unwrap(),
        2
    );
    assert_eq!(
        slack
            .database()
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM status_resolution_pending",
                [],
                |row| row.get(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(entry_states(&f), ["done", "queued"]);
    assert_eq!(
        slack
            .database()
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM status_jobs WHERE kind='resolution'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        0
    );
}
