//! A person's own waiting agent session is one direct message; moments between agents are none.
use super::*;
use crate::backend::LocalBackend;
use crate::hub::session::SessionCall;
use crate::hub::session_waiting::WaitReason;
use crate::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token};
use axum::extract::OriginalUri;
use tempfile::TempDir;

const REPO: &str = "acme/widgets";

struct Waits {
    _dir: TempDir,
    state: HubState,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    /// The agent tokens of alice (Slack U1) and bob (Slack U2).
    agents: [Principal; 2],
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Waits {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn private(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

/// A bound person: the binding config and the person's coding agent.
fn bound(store: &dyn Store, dir: &Path, id: i64, login: &str) -> (Value, Principal) {
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id: id,
    };
    let user = store.upsert_user(&identity, login).unwrap();
    let (_, secret, _) =
        mint_bound_agent_token(store, &user.id, "slack", REPO, 101, 3600, true).unwrap();
    private(&dir.join(login), secret.expose());
    let (_, agent, _) =
        mint_bound_agent_token(store, &user.id, "claude", REPO, 101, 3600, false).unwrap();
    let agent = store
        .authenticate(&agent_token_digest(agent.expose()).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    let binding = json!({"slack_user": format!("U{id}"), "provider": "github",
        "host": "github.com", "forge_user_id": id, "repository": REPO, "repository_id": 101,
        "agent_token_file": dir.join(login)});
    (binding, agent)
}

impl Waits {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let (alice_binding, alice) = bound(&*store, dir.path(), 1, "alice");
        let (bob_binding, bob) = bound(&*store, dir.path(), 2, "bob");
        private(&dir.path().join("signing"), "synthetic-signing-secret");
        private(&dir.path().join("bot"), "xoxb-synthetic");
        let config = json!({"version":1,"app_id":"A123","bot_id":"B123","team_id":"T123",
            "hub_url":"https://hub.example.test/","signing_secret_file":dir.path().join("signing"),
            "bot_token_file":dir.path().join("bot"),"database":dir.path().join("slack.sqlite"),
            "bindings":[alice_binding, bob_binding]});
        private(&dir.path().join("config"), &config.to_string());
        let calls = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let capture = calls.clone();
        let fake = Router::new().route(
            "/api/{method}",
            post(move |OriginalUri(uri): OriginalUri, body: Bytes| {
                let capture = capture.clone();
                async move {
                    if uri.path() == "/api/auth.test" {
                        return axum::Json(json!({"ok":true,"team_id":"T123","bot_id":"B123"}));
                    }
                    capture.lock().unwrap().push((
                        uri.path().to_owned(),
                        serde_json::from_slice(&body).unwrap(),
                    ));
                    axum::Json(json!({"ok":true}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        let mut slack = Slack::from_file(&dir.path().join("config")).unwrap();
        slack.api = format!("http://{address}/api/");
        let state = HubState::new(
            store,
            Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
        )
        .with_slack(slack);
        Self {
            _dir: dir,
            state,
            calls,
            agents: [alice, bob],
            task,
        }
    }

    /// Person `who` (0 alice, 1 bob) starts waiting in session `session` at `since`.
    fn wait(&self, who: usize, session: &str, reason: WaitReason, since: i64) {
        self.state
            .onboarding
            .waiting()
            .record(
                &self.agents[who],
                session,
                session,
                "claude",
                reason,
                None,
                Some("Retry the job?"),
                since,
            )
            .unwrap();
    }

    fn clear(&self, who: usize, session: &str) {
        self.state
            .onboarding
            .waiting()
            .clear(&self.agents[who].token_id, session, None, true);
    }

    /// One worker pass at `now`: collect the waits, then send every due message.
    async fn pass(&self, now: i64) {
        let slack = self.state.slack.clone().unwrap();
        let state = self.state.clone();
        tokio::task::block_in_place(|| slack.collect_waits(&state, now)).unwrap();
        while slack.deliver(&self.state).await.unwrap() {}
    }

    /// The direct messages sent so far: (Slack user, notification text, Open Hub link).
    fn sent(&self) -> Vec<(String, String, String)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/api/chat.postMessage")
            .map(|(_, body)| {
                let open = body["blocks"][2]["elements"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["action_id"] == OPEN)
                    .unwrap();
                (
                    body["channel"].as_str().unwrap().to_owned(),
                    body["text"].as_str().unwrap().to_owned(),
                    open["url"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_past_the_grace_is_one_message_however_many_passes_see_it() {
    let f = Waits::new().await;
    let now = unix_now();
    f.wait(0, "s1", WaitReason::Question, now - 30);
    // Someone at the keyboard answers within the grace: nothing yet.
    f.pass(now).await;
    assert!(f.sent().is_empty());
    for later in [now + 61, now + 122, now + 183] {
        f.pass(later).await;
    }
    let sent = f.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    let (user, text, link) = &sent[0];
    assert_eq!(user, "U1");
    assert!(
        text.starts_with("Your agent is waiting for you: Your "),
        "{text}"
    );
    assert!(text.contains("Retry the job?"), "{text}");
    assert_eq!(link, "https://hub.example.test/#box-needs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_wait_of_the_same_session_is_a_new_message() {
    let f = Waits::new().await;
    let now = unix_now();
    f.wait(0, "s1", WaitReason::Permission, now - 120);
    f.pass(now).await;
    assert_eq!(f.sent().len(), 1);
    // The session moved on and stopped again later: that is another wait.
    f.clear(0, "s1");
    f.wait(0, "s1", WaitReason::Permission, now + 100);
    f.pass(now + 130).await;
    assert_eq!(f.sent().len(), 1, "inside the grace");
    f.pass(now + 200).await;
    f.pass(now + 261).await;
    assert_eq!(f.sent().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_waiting_agents_own_person_is_messaged() {
    let f = Waits::new().await;
    let now = unix_now();
    f.wait(1, "bobs", WaitReason::Question, now - 120);
    f.pass(now).await;
    f.pass(now + 61).await;
    let sent = f.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "U2");
    f.wait(0, "alices", WaitReason::Question, now - 120);
    f.pass(now + 122).await;
    let users: Vec<_> = f.sent().into_iter().map(|(user, ..)| user).collect();
    assert_eq!(users, ["U2", "U1"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moments_between_agents_send_nothing() {
    let f = Waits::new().await;
    let edit = |token: &str, session: &str| {
        let call = SessionCall {
            session: Some(session.into()),
            via: "hook:claude".into(),
            task: None,
            paths: vec!["src/widget.rs".into()],
            deny: true,
            report: false,
        };
        tokio::task::block_in_place(|| {
            f.state
                .store
                .touch_session(token, &call, unix_now())
                .unwrap()
        })
    };
    // Bob edits a file alice's agent holds: refused once, then repeated.
    edit(&f.agents[0].token_id, "a");
    assert!(!edit(&f.agents[1].token_id, "b").denied.is_empty());
    assert!(!edit(&f.agents[1].token_id, "b").repeated.is_empty());
    let now = unix_now();
    for pass in 0..3 {
        f.pass(now + pass * 61).await;
    }
    assert!(f.sent().is_empty());
}
