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
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id: id,
    };
    let user = store.upsert_user(&identity, login).unwrap();
    let (_, secret, _) =
        mint_bound_agent_token(store, &user.id, "slack", REPO, 101, 3600, true).unwrap();
    private(&dir.join(login), secret.expose());
    let (token, _, _) =
        mint_bound_agent_token(store, &user.id, agent, REPO, 101, 3600, false).unwrap();
    let binding = json!({"slack_user": format!("U{id}"), "provider": "github",
        "host": "github.com", "forge_user_id": id, "repository": REPO, "repository_id": 101,
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
        let dir = tempfile::tempdir().unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        let store = store::open(&url).unwrap();
        let (alice_binding, alice) = person(&*store, dir.path(), 1, "alice", "claude");
        let (bob_binding, bob) = person(&*store, dir.path(), 2, "bob", "codex");
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
fn manifest_requests_only_direct_messages() {
    let manifest: Value =
        serde_json::from_str(include_str!("../../../examples/slack-manifest.json")).unwrap();
    assert_eq!(
        manifest["oauth_config"]["scopes"],
        json!({"bot":["chat:write"]})
    );
    assert_eq!(
        manifest["settings"]["event_subscriptions"]["bot_events"],
        json!(["tokens_revoked", "app_uninstalled"])
    );
    assert!(manifest["features"].get("slash_commands").is_none());
}
