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

    /// Bob edits the file alice's agent holds: refused once, then repeated.
    fn contest(&self) {
        self.edit(&self.alice, "a");
        assert!(!self.edit(&self.bob, "b").denied.is_empty());
        assert!(!self.edit(&self.bob, "b").repeated.is_empty());
    }

    /// The direct messages sent so far, as (Slack user, text).
    fn sent(&self) -> Vec<(String, String)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/api/chat.postMessage")
            .map(|(_, body)| {
                let text = body["blocks"][0]["text"]["text"].as_str().unwrap();
                (
                    body["channel"].as_str().unwrap().to_owned(),
                    text.to_owned(),
                )
            })
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

    /// The `chat.update` a button click sent; it runs after the click is acknowledged.
    async fn update(&self) -> Value {
        for _ in 0..100 {
            let update = self
                .calls
                .lock()
                .unwrap()
                .iter()
                .find(|(path, _)| path == "/api/chat.update")
                .map(|(_, body)| body.clone());
            if let Some(update) = update {
                return update;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no chat.update was sent");
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
        let button = message["blocks"][1]["elements"]
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
        "bob's agent codex edited src/widget.rs in acme/widgets after AgentSCM said your agent claude is working on it. Hand the file over, or keep it and agree with them who goes first."
    );
    let repeater = &sent.iter().find(|(user, _)| user == "U2").unwrap().1;
    assert_eq!(
        repeater,
        "Your agent codex edited src/widget.rs in acme/widgets after AgentSCM said alice's agent claude is working on it. Agree with them who goes first."
    );
    // Only the holder's message carries the handoff buttons; both link the Hub.
    let buttons = |user: &str| {
        f.sent_body(user)["blocks"][1]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["action_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(buttons("U1"), [YES, NO, "agentscm_open_hub"]);
    assert_eq!(buttons("U2"), ["agentscm_open_hub"]);

    // Bob cannot answer for alice's agent with her button.
    let alice_message = f.sent_body("U1");
    let value = &alice_message["blocks"][1]["elements"][0]["value"];
    assert_eq!(
        f.click_value("U2", YES, value, &alice_message).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(f.click("U1", YES).await, StatusCode::OK);
    let update = f.update().await;
    assert_eq!(update["channel"], "D1");
    assert_eq!(update["ts"], "1.2");
    assert_eq!(
        update["blocks"][1]["elements"][0]["text"],
        "You handed src/widget.rs to bob's agent codex; their agent is told on its next call."
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
    // A second click finds nothing open; the same events never message again.
    assert_eq!(f.click("U1", NO).await, StatusCode::OK);
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
        update["blocks"][1]["elements"][0]["text"],
        "You handed src/widget.rs to bob's agent codex; their agent is told on its next call."
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
        f.update().await["blocks"][1]["elements"][0]["text"],
        "You handed src/widget.rs to bob's agent codex; their agent is told on its next call."
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
        "bob's agent codex edited src/gadget.rs, src/widget.rs in acme/widgets after AgentSCM said your agent claude is working on them. Hand the files over, or keep them and agree with them who goes first."
    );
    let buttons: Vec<_> = f.sent_body("U1")["blocks"][1]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(buttons, ["agentscm_open_hub"]);
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
        "alice's agent claude kept src/widget.rs in acme/widgets, which your agent codex asked for (reason: \"mid-refactor\"). Agree with them who goes first.".into()
    )));
    assert!(sent.contains(&(
        "U1".into(),
        "Your agent claude kept src/widget.rs in acme/widgets, which bob's agent codex asked for (reason: \"mid-refactor\"). Agree with them who goes first.".into()
    )));
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
        "Your agent codex started \"switch migrations to filename-based discovery\" in acme/widgets, which reads like alice's agent claude \"make db migrations filename based\"; open pull request #9. Check whether it is the same work.".into()
    )));
    assert!(sent.contains(&(
        "U1".into(),
        "bob's agent codex started \"switch migrations to filename-based discovery\" in acme/widgets, which reads like your agent claude's work \"make db migrations filename based\". Check with them whether it is the same work.".into()
    )));
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

#[test]
fn message_text_cannot_mention_or_link() {
    let body = message(
        "<!channel> & <http://x|y>",
        None,
        "https://hub.example.test/ui/",
    );
    assert_eq!(body["text"], "&lt;!channel&gt; &amp; &lt;http://x|y&gt;");
    assert_eq!(body["blocks"][0]["text"]["type"], "plain_text");
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
