//! A connected app lists and answers its person's permission prompts, as the person's own page
//! does; a read-only app only lists them; a computer credential and an agent token still cannot.
//! Against an in-process fake forge; no live accounts.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::login::{Login, LoginConfig};
use agentscm::hub::store::{
    self, ForgeIdentity, Store, device_digest, mint_bound_agent_token, unix_now,
};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::{HubState, router};
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 4242;
/// Connected too, but the forge says alice has no role there.
const SECRET: &str = "acme/secret";
const SECRET_ID: i64 = 4243;
const PERMISSIONS: &str = "/v1/hub/session/permissions";
const ANSWER: &str = "/v1/hub/session/permissions/answer";

/// The forge calls the Hub makes here: its connection's own user, and a person's role.
async fn fake_forge() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/user",
            get(|| async { Json(json!({"id": 100, "login": "hubbot"})) }),
        )
        .route(
            "/repos/{o}/{n}",
            get(|Path((o, n)): Path<(String, String)>| async move {
                let full_name = format!("{o}/{n}");
                let id = if full_name == SECRET {
                    SECRET_ID
                } else {
                    REPO_ID
                };
                Json(json!({"id": id, "full_name": full_name, "private": true}))
            }),
        )
        .route(
            "/repos/{o}/{n}/collaborators/{l}/permission",
            get(
                |Path((o, n, l)): Path<(String, String, String)>| async move {
                    if format!("{o}/{n}") == SECRET {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    let id = match l.as_str() {
                        "alice" => 7,
                        "bob" => 8,
                        _ => return StatusCode::NOT_FOUND.into_response(),
                    };
                    Json(json!({"permission": "write", "role_name": "write",
                    "user": {"id": id, "login": l}}))
                    .into_response()
                },
            ),
        );
    tokio::spawn(async move { axum::serve(listener, app).await });
    base
}

fn private_file(dir: &TempDir, name: &str, text: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path.display().to_string()
}

/// A Hub with a registered app and a read-only one, over a store with REPO and SECRET connected.
async fn hub(dir: &TempDir, store: Arc<dyn Store>, base: &str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let secret = private_file(dir, "app-secret", &format!("{}\n", "s".repeat(40)));
    let apps = json!({"apps": [{"id": "example-app", "name": "Example App",
        "redirect_uri": "https://app.example.com/callback", "secret_file": secret, "via": "slack"},
        {"id": "notifier", "name": "Notifier", "redirect_uri": "https://app.example.com/callback",
        "secret_file": secret, "read_only": true}]});
    let vars: HashMap<String, String> = HashMap::from([
        ("AGENTSCM_GITHUB_CLIENT_ID".into(), "fake-client-id".into()),
        ("AGENTSCM_GITHUB_CLIENT_SECRET".into(), "fake-secret".into()),
        ("AGENTSCM_GITHUB_URL".into(), base.into()),
        ("AGENTSCM_GITHUB_API_URL".into(), base.into()),
        ("AGENTSCM_GITHUB_PAT".into(), "synthetic-hub-token".into()),
        (
            "AGENTSCM_HUB_APPS_FILE".into(),
            private_file(dir, "apps.json", &apps.to_string()),
        ),
    ]);
    let login = LoginConfig::from_lookup(&url, |name| vars.get(name).cloned()).unwrap();
    let secrets = dir.path().join("secrets");
    sync::prepare_secrets_dir(&secrets, true).unwrap();
    let sync_config =
        SyncConfig::from_lookup(&url, secrets, 14, |name| vars.get(name).cloned()).unwrap();
    let state = HubState::new(
        store.clone(),
        Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
    )
    .with_login(Login::new(login).unwrap())
    .with_sync(Sync::new(sync_config).unwrap());
    sync::start(&state).await.unwrap();
    let host = base.trim_start_matches("http://");
    let connection = store.pat_connection(host).unwrap().unwrap().id;
    for (id, name) in [(REPO_ID, REPO), (SECRET_ID, SECRET)] {
        store
            .upsert_repository(&connection, host, id, name, None, None)
            .unwrap();
    }
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

async fn call(
    hub: &str,
    method: Method,
    path: &str,
    bearer: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .request(method, format!("{hub}{path}"))
        .bearer_auth(bearer);
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// The agent's session shows a prompt and its hook offers it; replies the `request_id`.
async fn offer(hub: &str, agent: &str, session: &str) -> Value {
    let hook = "/v1/hub/hook/claude";
    let prompt =
        json!({"hook_event_name": "PermissionRequest", "session_id": session, "tool_name": "Bash"});
    assert_eq!(
        call(hub, Method::POST, hook, agent, Some(prompt)).await.0,
        200
    );
    let register = json!({"session": session, "tool": "Bash", "display": "cargo publish",
        "complete": true, "input_hash": "d".repeat(64)});
    let (status, made) = call(
        hub,
        Method::POST,
        "/v1/hub/permission/register",
        agent,
        Some(register),
    )
    .await;
    assert_eq!(status, 200, "{made}");
    let shown = json!({"hook_event_name": "Notification", "session_id": session,
        "notification_type": "permission_prompt"});
    assert_eq!(
        call(hub, Method::POST, hook, agent, Some(shown)).await.0,
        200
    );
    made["request_id"].clone()
}

#[tokio::test]
async fn a_connected_app_lists_and_answers_its_persons_prompt_and_nothing_else_does() {
    let dir = TempDir::new().unwrap();
    let base = fake_forge().await;
    let store = store::open(&format!(
        "sqlite://{}",
        dir.path().join("hub.sqlite").display()
    ))
    .unwrap();
    let hub = hub(&dir, store.clone(), &base).await;
    let host = base.trim_start_matches("http://").to_owned();
    let person = |user_id: i64, login: &str| {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: host.clone(),
            user_id,
        };
        store.upsert_user(&identity, login).unwrap().id
    };
    let (alice, bob) = (person(7, "alice"), person(8, "bob"));
    let now = unix_now();
    let device = |owner: &str, raw: &str, app: Option<&str>| {
        store
            .insert_device(
                owner,
                "laptop",
                &device_digest(raw).unwrap(),
                None,
                app,
                now,
            )
            .unwrap();
        raw.to_owned()
    };
    let app = device(&alice, &"a".repeat(64), Some("example-app"));
    let bobs_app = device(&bob, &"b".repeat(64), Some("example-app"));
    let computer = device(&alice, &"c".repeat(64), None);
    let (_, agent, _) =
        mint_bound_agent_token(&*store, &alice, "claude-code", REPO, REPO_ID, 3600, false).unwrap();
    let agent = agent.expose().to_owned();
    let (_, read_only, _) =
        mint_bound_agent_token(&*store, &alice, "claude-code", REPO, REPO_ID, 3600, true).unwrap();
    let read_only = read_only.expose().to_owned();
    // Alice's agent in a repository the forge says she can no longer read.
    let (_, secret_agent, _) = mint_bound_agent_token(
        &*store,
        &alice,
        "claude-code",
        SECRET,
        SECRET_ID,
        3600,
        false,
    )
    .unwrap();
    let secret_agent = secret_agent.expose().to_owned();

    let id = offer(&hub, &agent, "11111111-2222-3333-4444-555555555555").await;
    let secret_id = offer(&hub, &secret_agent, "66666666-7777-8888-9999-000000000000").await;

    // The app lists only the prompt in the repository she can read, in every repository or in
    // the one named, with its repository.
    for path in [
        PERMISSIONS.to_owned(),
        format!("{PERMISSIONS}?repository={REPO}"),
    ] {
        let (status, listed) = call(&hub, Method::GET, &path, &app, None).await;
        assert_eq!(status, 200, "{listed}");
        assert_eq!(
            listed["requests"].as_array().map(Vec::len),
            Some(1),
            "{listed}"
        );
        let request = &listed["requests"][0];
        assert_eq!(
            (
                &request["request_id"],
                &request["repository"],
                &request["state"]
            ),
            (&id, &json!(REPO), &json!("pending")),
            "{listed}"
        );
    }
    let path = format!("{PERMISSIONS}?repository={SECRET}");
    let (status, hidden) = call(&hub, Method::GET, &path, &app, None).await;
    assert_eq!((status, hidden), (200, json!({"requests": []})));
    let secret = json!({"repository": SECRET, "request_id": secret_id, "decision": "allow"});
    let (status, refused) = call(&hub, Method::POST, ANSWER, &app, Some(secret)).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("insufficient_role")),
        "{refused}"
    );
    // Another person's app sees none of it and cannot answer it.
    let (status, theirs) = call(&hub, Method::GET, PERMISSIONS, &bobs_app, None).await;
    assert_eq!((status, theirs), (200, json!({"requests": []})));
    let answer = json!({"repository": REPO, "request_id": id, "decision": "deny"});
    let (status, refused) = call(&hub, Method::POST, ANSWER, &bobs_app, Some(answer.clone())).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_your_agent"))
    );

    // A computer credential and an agent token of the person, read-only or not, still cannot.
    for bearer in [&computer, &agent, &read_only] {
        assert_eq!(
            call(&hub, Method::GET, PERMISSIONS, bearer, None).await.0,
            401
        );
        let (status, _) = call(&hub, Method::POST, ANSWER, bearer, Some(answer.clone())).await;
        assert_eq!(status, 401);
    }

    // A read-only app lists her prompt, never another person's, mints read-only tokens only, and
    // answers nothing: no prompt, coordination or file request, and no message to an agent.
    let notifier = device(&alice, &"d".repeat(64), Some("notifier"));
    let bobs_notifier = device(&bob, &"e".repeat(64), Some("notifier"));
    let (status, listed) = call(&hub, Method::GET, PERMISSIONS, &notifier, None).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(
        listed["requests"].as_array().map(Vec::len),
        Some(1),
        "{listed}"
    );
    assert_eq!(listed["requests"][0]["request_id"], id, "{listed}");
    let (status, theirs) = call(&hub, Method::GET, PERMISSIONS, &bobs_notifier, None).await;
    assert_eq!((status, theirs), (200, json!({"requests": []})));
    let tokens = "/v1/hub/device/tokens";
    let mint =
        |read_only: bool| json!({"name": "notifier", "repository": REPO, "read_only": read_only});
    let (status, minted) = call(&hub, Method::POST, tokens, &notifier, Some(mint(true))).await;
    assert_eq!(status, 201, "{minted}");
    let (status, refused) = call(&hub, Method::POST, tokens, &notifier, Some(mint(false))).await;
    assert_eq!(
        refused["error"].as_str(),
        Some("app_read_only"),
        "{status} {refused}"
    );
    for (path, body) in [
        (ANSWER, answer.clone()),
        ("/v1/hub/session/coordination/reply", json!({})),
        ("/v1/hub/session/handoff/answer", json!({})),
    ] {
        let (status, refused) = call(&hub, Method::POST, path, &notifier, Some(body)).await;
        assert_eq!(
            (status, refused["error"].as_str()),
            (403, Some("app_cannot_answer")),
            "{path}: {refused}"
        );
    }
    // Sending an agent a message takes the web sign-in alone.
    let message = json!({"repository": REPO, "token": agent, "body": "hello"});
    let (status, _) = call(
        &hub,
        Method::POST,
        "/v1/hub/session/agent-messages",
        &notifier,
        Some(message),
    )
    .await;
    assert_eq!(status, 401);

    // The full app answers, and the hook hears it.
    let (status, done) = call(&hub, Method::POST, ANSWER, &app, Some(answer)).await;
    assert_eq!(
        (status, done),
        (200, json!({"state": "answered", "decision": "deny"}))
    );
    let poll = json!({"request_id": id, "input_hash": "d".repeat(64), "wait_seconds": 0});
    let (_, polled) = call(
        &hub,
        Method::POST,
        "/v1/hub/permission/poll",
        &agent,
        Some(poll),
    )
    .await;
    assert_eq!(
        (&polled["state"], &polled["decision"]),
        (&json!("answered"), &json!("deny"))
    );
}
