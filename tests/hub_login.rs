//! Sign-in, sessions and token minting against an in-process fake forge. No live accounts.
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use agentscm::backend::LocalBackend;
use agentscm::hub::connection::NewConnection;
use agentscm::hub::login::{Login, LoginConfig};
use agentscm::hub::store::{self, Store, agent_token_digest, new_connection_id, unix_now};
use agentscm::hub::{HubState, router};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const CLIENT_ID: &str = "fake-client-id";
const CLIENT_SECRET: &str = "fake-client-secret-value";
const COOKIE: &str = "__Host-agentscm_login";

// ---- fake forge: GitHub- and GitLab-shaped endpoints on one server ----

struct Grant {
    challenge: String,
    redirect: String,
    user: i64,
}

enum Step {
    Pending,
    SlowDown(Option<i64>),
    Expired,
    Denied,
    Approve(i64),
    /// The forge answers 502 with no JSON.
    Unavailable,
}

#[derive(Default)]
struct Fake {
    base: String,
    users: HashMap<i64, String>,
    /// Who approves at the authorize endpoint.
    current_user: i64,
    deny: bool,
    expire_codes: bool,
    no_device: bool,
    codes: HashMap<String, Grant>,
    tokens: HashMap<String, i64>,
    revoked: HashSet<String>,
    next: u32,
    repos: HashMap<(i64, String), i64>,
    repo_lookups: u32,
    /// Lowercase names of public repositories, and who may push to which.
    public: HashSet<String>,
    /// Lowercase names of GitLab projects with `internal` visibility.
    internal: HashSet<String>,
    push: HashSet<(i64, String)>,
    /// GitLab (project, group) access levels that replace the push-derived default.
    levels: HashMap<(i64, String), (Option<i64>, Option<i64>)>,
    script: VecDeque<Step>,
    authorize_queries: Vec<HashMap<String, String>>,
}

type Shared = Arc<Mutex<Fake>>;

fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorize(fake: &Shared, query: HashMap<String, String>) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.authorize_queries.push(query.clone());
    if query.get("client_id").map(String::as_str) != Some(CLIENT_ID) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let (redirect, state) = (&query["redirect_uri"], &query["state"]);
    if fake.deny {
        return Redirect::to(&format!("{redirect}?error=access_denied&state={state}"))
            .into_response();
    }
    fake.next += 1;
    let code = format!("code{}", fake.next);
    let grant = Grant {
        challenge: query["code_challenge"].clone(),
        redirect: redirect.clone(),
        user: fake.current_user,
    };
    fake.codes.insert(code.clone(), grant);
    Redirect::to(&format!("{redirect}?code={code}&state={state}")).into_response()
}

fn oauth_error(gitlab: bool, error: &str) -> Response {
    let status = if gitlab {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    (status, Json(json!({"error": error}))).into_response()
}

fn issue(fake: &mut Fake, user: i64) -> Response {
    fake.next += 1;
    let token = format!("synthetic-forge-token-{}", fake.next);
    fake.tokens.insert(token.clone(), user);
    let expires_in = 7200;
    Json(json!({"access_token": token, "token_type": "bearer", "expires_in": expires_in}))
        .into_response()
}

fn token_endpoint(fake: &Shared, gitlab: bool, form: HashMap<String, String>) -> Response {
    let mut fake = fake.lock().unwrap();
    let get = |name: &str| form.get(name).map(String::as_str).unwrap_or_default();
    if get("client_id") != CLIENT_ID {
        return oauth_error(gitlab, "invalid_client");
    }
    if get("grant_type") == "urn:ietf:params:oauth:grant-type:device_code" {
        if get("device_code") != "devcode" || form.contains_key("client_secret") {
            return oauth_error(gitlab, "invalid_grant");
        }
        return match fake.script.pop_front() {
            Some(Step::Pending) | None => oauth_error(gitlab, "authorization_pending"),
            Some(Step::SlowDown(interval)) => {
                let mut body = json!({"error": "slow_down"});
                if let Some(interval) = interval {
                    body["interval"] = json!(interval);
                }
                (StatusCode::OK, Json(body)).into_response()
            }
            Some(Step::Expired) => oauth_error(gitlab, "expired_token"),
            Some(Step::Denied) => oauth_error(gitlab, "access_denied"),
            Some(Step::Approve(user)) => issue(&mut fake, user),
            Some(Step::Unavailable) => (StatusCode::BAD_GATEWAY, "bad gateway").into_response(),
        };
    }
    let bad = || {
        oauth_error(
            gitlab,
            if gitlab {
                "invalid_grant"
            } else {
                "bad_verification_code"
            },
        )
    };
    if get("client_secret") != CLIENT_SECRET
        || (gitlab && get("grant_type") != "authorization_code")
    {
        return oauth_error(gitlab, "invalid_client");
    }
    let expired = fake.expire_codes;
    let Some(grant) = fake.codes.remove(get("code")).filter(|_| !expired) else {
        return bad();
    };
    if grant.redirect != get("redirect_uri") || challenge(get("code_verifier")) != grant.challenge {
        return bad();
    }
    issue(&mut fake, grant.user)
}

fn device_endpoint(fake: &Shared, gitlab: bool, form: HashMap<String, String>) -> Response {
    let fake = fake.lock().unwrap();
    let scope_ok = !gitlab || form.get("scope").map(String::as_str) == Some("read_api");
    if fake.no_device || !scope_ok || form.get("client_id").map(String::as_str) != Some(CLIENT_ID) {
        return oauth_error(gitlab, "device_flow_disabled");
    }
    Json(json!({"device_code": "devcode", "user_code": "ABCD-1234",
        "verification_uri": format!("{}/device", fake.base), "expires_in": 900, "interval": 1}))
    .into_response()
}

fn bearer_user(fake: &Fake, headers: &HeaderMap) -> Option<i64> {
    let token = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    if fake.revoked.contains(token) {
        return None;
    }
    fake.tokens.get(token).copied()
}

fn user_endpoint(fake: &Shared, headers: &HeaderMap, field: &str) -> Response {
    let fake = fake.lock().unwrap();
    match bearer_user(&fake, headers) {
        Some(id) => Json(json!({"id": id, field: fake.users[&id]})).into_response(),
        None => (StatusCode::UNAUTHORIZED, Json(json!({"message": "401"}))).into_response(),
    }
}

fn repo_endpoint(fake: &Shared, headers: &HeaderMap, full_name: String) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.repo_lookups += 1;
    let Some(user) = bearer_user(&fake, headers) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({}))).into_response();
    };
    // GitLab is also asked by numeric project id.
    let name = match full_name.parse::<i64>() {
        Ok(id) => fake
            .repos
            .iter()
            .find(|((owner, _), found)| *owner == user && **found == id)
            .map(|((_, name), _)| name.clone())
            .unwrap_or_default(),
        Err(_) => full_name.to_ascii_lowercase(),
    };
    if name == "acme/moved" {
        let mut moved = StatusCode::MOVED_PERMANENTLY.into_response();
        moved.headers_mut().insert(
            header::LOCATION,
            "http://127.0.0.1:1/elsewhere".parse().unwrap(),
        );
        return moved;
    }
    // GitHub and GitLab both answer with their own canonical spelling.
    match fake.repos.get(&(user, name.clone())) {
        Some(id) => {
            let mut body = json!({"id": id, "full_name": name, "path_with_namespace": name,
                "visibility": "private"});
            let (public, internal) = (fake.public.contains(&name), fake.internal.contains(&name));
            // Developer (30) when the user may push, else Reporter (20), unless set.
            let push = fake.push.contains(&(user, name.clone()));
            let (project, group) = fake
                .levels
                .get(&(user, name.clone()))
                .copied()
                .unwrap_or((Some(if push { 30 } else { 20 }), None));
            let level = |l: Option<i64>| l.map(|l| json!({"access_level": l}));
            if public || internal {
                body["visibility"] = json!(if public { "public" } else { "internal" });
            }
            body["permissions"] = json!({"pull": true, "push": push,
                "project_access": level(project), "group_access": level(group)});
            if public {
                body["private"] = json!(false);
            }
            Json(body).into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(json!({}))).into_response(),
    }
}

async fn start_fake() -> (Shared, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let fake: Shared = Arc::new(Mutex::new(Fake {
        base: base.clone(),
        ..Fake::default()
    }));
    type Q = Query<HashMap<String, String>>;
    type F = Form<HashMap<String, String>>;
    let app = Router::new()
        .route("/login/oauth/authorize", get(|State(f): State<Shared>, Query(q): Q| async move { authorize(&f, q) }))
        .route("/oauth/authorize", get(|State(f): State<Shared>, Query(q): Q| async move { authorize(&f, q) }))
        .route("/login/oauth/access_token", post(|State(f): State<Shared>, Form(x): F| async move { token_endpoint(&f, false, x) }))
        .route("/oauth/token", post(|State(f): State<Shared>, Form(x): F| async move { token_endpoint(&f, true, x) }))
        .route("/login/device/code", post(|State(f): State<Shared>, Form(x): F| async move { device_endpoint(&f, false, x) }))
        .route("/oauth/authorize_device", post(|State(f): State<Shared>, Form(x): F| async move { device_endpoint(&f, true, x) }))
        .route("/user", get(|State(f): State<Shared>, h: HeaderMap| async move { user_endpoint(&f, &h, "login") }))
        .route("/api/v4/user", get(|State(f): State<Shared>, h: HeaderMap| async move { user_endpoint(&f, &h, "username") }))
        .route("/repos/{owner}/{name}", get(|State(f): State<Shared>, h: HeaderMap, Path((o, n)): Path<(String, String)>| async move { repo_endpoint(&f, &h, format!("{o}/{n}")) }))
        .route("/api/v4/projects/{path}", get(|State(f): State<Shared>, h: HeaderMap, Path(p): Path<String>| async move { repo_endpoint(&f, &h, p) }))
        .with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (fake, base)
}

// ---- hub under test ----

struct Env {
    hub: SocketAddr,
    fake: Shared,
    store: Arc<dyn Store>,
    /// The fake's identity host, and the connection that holds its repositories.
    host: String,
    connection: String,
    http: reqwest::Client,
    /// Every body the hub sent, for secrecy checks.
    log: Arc<Mutex<Vec<String>>>,
    _dir: TempDir,
}

fn lookup(base: &str, admins: &str, allowed: &str) -> HashMap<String, String> {
    let mut vars: HashMap<String, String> = HashMap::new();
    for (prefix, extra) in [("GITHUB", Some(("API_URL", base))), ("GITLAB", None)] {
        vars.insert(format!("AGENTSCM_{prefix}_CLIENT_ID"), CLIENT_ID.into());
        vars.insert(
            format!("AGENTSCM_{prefix}_CLIENT_SECRET"),
            CLIENT_SECRET.into(),
        );
        vars.insert(format!("AGENTSCM_{prefix}_URL"), base.into());
        if let Some((name, value)) = extra {
            vars.insert(format!("AGENTSCM_{prefix}_{name}"), value.into());
        }
    }
    vars.insert("AGENTSCM_ADMINS".into(), admins.into());
    vars.insert("AGENTSCM_ALLOWED_USERS".into(), allowed.into());
    vars
}

impl Env {
    /// Everyone but the admin (user 9) is allowed, by forge id.
    async fn start(admins: &str) -> Self {
        Self::start_with(admins, "github:#7,github:#8,gitlab:#7,gitlab:#8", |_| {}).await
    }

    async fn start_with(
        admins: &str,
        allowed: &str,
        tweak: impl FnOnce(&mut HashMap<String, String>),
    ) -> Self {
        let (fake, base) = start_fake().await;
        {
            let mut fake = fake.lock().unwrap();
            fake.users = HashMap::from([
                (7, "alice".into()),
                (8, "bob".into()),
                (9, "AdminAl".into()),
                (5, "carol".into()),
            ]);
            fake.current_user = 7;
        }
        let dir = TempDir::new().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        // The fake serves GitHub and GitLab on one host; repositories connected on it
        // stand for both, so a connection here is just where they are recorded.
        let host = base.trim_start_matches("http://").to_owned();
        let connection = store
            .insert_connection(
                &NewConnection {
                    id: new_connection_id().unwrap(),
                    provider: "github".into(),
                    host: host.clone(),
                    kind: "app".into(),
                    app_id: Some(1),
                    app_slug: Some("fixture".into()),
                    acts_as: None,
                },
                "fixture",
            )
            .unwrap()
            .id;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub = listener.local_addr().unwrap();
        let mut vars = lookup(&base, admins, allowed);
        tweak(&mut vars);
        let config =
            LoginConfig::from_lookup(&format!("http://{hub}"), |name| vars.get(name).cloned())
                .unwrap();
        let state = HubState::new(
            store.clone(),
            Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
        )
        .with_login(Login::new(config).unwrap());
        let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let env = Self {
            hub,
            fake,
            store,
            host,
            connection,
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            log: Arc::default(),
            _dir: dir,
        };
        // Everyone but carol can read one connected repository, which is what signing in needs.
        for user in [7, 8, 9] {
            env.grant_repo(user, "acme/signin", 9999);
        }
        env
    }

    fn url(&self) -> String {
        format!("http://{}", self.hub)
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        bearer: Option<&str>,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> (u16, HeaderMap, Value) {
        let mut request = self.http.request(method, url);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(body) = body {
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let (status, headers) = (response.status().as_u16(), response.headers().clone());
        let text = response.text().await.unwrap();
        self.log.lock().unwrap().push(text.clone());
        (
            status,
            headers,
            serde_json::from_str(&text).unwrap_or(Value::Null),
        )
    }

    async fn get(&self, path: &str, bearer: Option<&str>) -> (u16, Value) {
        let (status, _, body) = self
            .send(
                Method::GET,
                &format!("{}{path}", self.url()),
                bearer,
                None,
                None,
            )
            .await;
        (status, body)
    }

    async fn post(&self, path: &str, bearer: Option<&str>, body: Value) -> (u16, Value) {
        let (status, _, body) = self
            .send(
                Method::POST,
                &format!("{}{path}", self.url()),
                bearer,
                None,
                Some(body),
            )
            .await;
        (status, body)
    }

    /// Browser start as the CLI would: returns (forge authorize URL, cookie, verifier).
    async fn browser_start(&self, provider: &str) -> (String, String, String) {
        let verifier = "v".repeat(64);
        let path = format!(
            "/v1/hub/login/{provider}?cli_port=40000&cli_challenge={}",
            challenge(&verifier)
        );
        let (status, headers, _) = self
            .send(
                Method::GET,
                &format!("{}{path}", self.url()),
                None,
                None,
                None,
            )
            .await;
        assert_eq!(status, 302);
        let location = headers[header::LOCATION].to_str().unwrap().to_owned();
        let cookie = headers[header::SET_COOKIE].to_str().unwrap();
        (
            location,
            cookie.split(';').next().unwrap().to_owned(),
            verifier,
        )
    }

    /// Follow the forge authorize URL; returns the hub callback URL it redirects to.
    async fn authorize_at_forge(&self, location: &str) -> String {
        let response = self.http.get(location).send().await.unwrap();
        assert_eq!(response.status(), 303);
        response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned()
    }

    async fn callback(&self, url: &str, cookie: Option<&str>) -> (u16, HeaderMap, Value) {
        self.send(Method::GET, url, None, cookie, None).await
    }

    async fn exchange(&self, code: &str, verifier: &str) -> (u16, Value) {
        self.post(
            "/v1/hub/login/exchange",
            None,
            json!({"code": code, "verifier": verifier}),
        )
        .await
    }

    /// Full browser flow; returns the exchange reply.
    async fn browser_login(&self, provider: &str, user: i64) -> Value {
        self.fake.lock().unwrap().current_user = user;
        let (location, cookie, verifier) = self.browser_start(provider).await;
        let callback = self.authorize_at_forge(&location).await;
        let (status, headers, _) = self.callback(&callback, Some(&cookie)).await;
        assert_eq!(status, 302);
        let target = headers[header::LOCATION].to_str().unwrap();
        let code = target
            .strip_prefix("http://127.0.0.1:40000/callback?code=")
            .unwrap();
        let (status, signed) = self.exchange(code, &verifier).await;
        assert_eq!(status, 200, "{signed}");
        signed
    }

    async fn device_login(&self, provider: &str, user: i64) -> Value {
        self.fake
            .lock()
            .unwrap()
            .script
            .push_back(Step::Approve(user));
        let (status, started) = self
            .post(&format!("/v1/hub/login/{provider}/device"), None, json!({}))
            .await;
        assert_eq!(status, 200, "{started}");
        let (status, signed) = self
            .post(
                "/v1/hub/login/device/poll",
                None,
                json!({"handle": started["handle"]}),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(signed["status"], "signed_in");
        signed
    }

    /// The forge lets `user` read `repo`, and the hub has it connected.
    fn grant_repo(&self, user: i64, repo: &str, id: i64) {
        self.store
            .upsert_repository(&self.connection, &self.host, id, repo, None, None)
            .unwrap();
        self.fake
            .lock()
            .unwrap()
            .repos
            .insert((user, repo.into()), id);
    }

    fn kinds(&self) -> Vec<String> {
        self.store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .filter(|kind| !kind.starts_with("connection_") && !kind.starts_with("repository_"))
            .collect()
    }

    async fn create_token(&self, session: &str, repo: &str) -> (u16, Value) {
        self.post(
            "/v1/hub/tokens",
            Some(session),
            json!({"name": "builder", "repository": repo, "ttl_seconds": 600}),
        )
        .await
    }
}

/// A refusal's status and kind, after checking it says why in a message.
fn refusal(reply: &(u16, Value)) -> (u16, &str) {
    let message = reply.1["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("; "),
        "a refusal names its fix: {}",
        reply.1
    );
    (reply.0, reply.1["error"].as_str().unwrap_or_default())
}

/// The kind a failed browser sign-in sends back to the CLI's loopback, after checking it has a
/// message.
fn refused_page(status: u16, headers: &HeaderMap) -> String {
    assert_eq!(status, 302);
    let location = headers[header::LOCATION].to_str().unwrap();
    let query = location
        .strip_prefix("http://127.0.0.1:40000/callback?error=")
        .unwrap_or_else(|| panic!("{location}"));
    let (reason, message) = query.split_once("&message=").unwrap();
    assert!(message.contains("%3B"), "{location}");
    reason.to_owned()
}

async fn poll(env: &Env, handle: &Value) -> (u16, Value) {
    env.post("/v1/hub/login/device/poll", None, json!({"handle": handle}))
        .await
}

fn secret(signed: &Value) -> String {
    signed["session"].as_str().unwrap().to_owned()
}

// ---- browser flow ----

async fn browser_flow(provider: &str) {
    let env = Env::start("").await;
    let (location, cookie, _) = env.browser_start(provider).await;
    let url = reqwest::Url::parse(&location).unwrap();
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], CLIENT_ID);
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(
        params["redirect_uri"],
        format!("{}/v1/hub/login/{provider}/callback", env.url())
    );
    assert_eq!(params.contains_key("scope"), provider == "gitlab");
    assert_eq!(params.contains_key("response_type"), provider == "gitlab");
    assert!(cookie.starts_with(&format!("{COOKIE}=")) && cookie.len() == COOKIE.len() + 1 + 64);
    let (_, headers, _) = env
        .send(
            Method::GET,
            &format!(
                "{}/v1/hub/login/{provider}?cli_port=40000&cli_challenge={}",
                env.url(),
                "A".repeat(43)
            ),
            None,
            None,
            None,
        )
        .await;
    let set = headers[header::SET_COOKIE].to_str().unwrap();
    for attribute in [
        "Path=/",
        "Secure",
        "HttpOnly",
        "SameSite=Lax",
        "Max-Age=600",
    ] {
        assert!(set.contains(attribute), "{set}");
    }

    let signed = env.browser_login(provider, 7).await;
    let (status, session) = env.get("/v1/hub/session", Some(&secret(&signed))).await;
    assert_eq!(status, 200);
    assert_eq!(session["user"]["login"], "alice");
    assert_eq!(session["user"]["provider"], provider);
    assert_eq!(session["admin"], false);
    assert!(session["user"]["id"].as_str().unwrap().starts_with("usr_"));
    if provider == "gitlab" {
        // The session never outlives the forge's token.
        assert!(signed["expires_at"].as_i64().unwrap() <= unix_now() + 7205);
    }
    let events = env.store.events_after(0, 1000).unwrap();
    let signed_in: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "user_signed_in")
        .collect();
    assert_eq!(signed_in.len(), 1);
    assert_eq!(signed_in[0].data, r#"{"admin":false,"method":"browser"}"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_flow_github() {
    browser_flow("github").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_flow_gitlab() {
    browser_flow("gitlab").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_start_validates_input_and_provider() {
    let env = Env::start("").await;
    let ok = challenge("x");
    for query in [
        "",
        "?cli_port=40000",
        &format!("?cli_challenge={ok}"),
        &format!("?cli_port=80&cli_challenge={ok}"),
        &format!("?cli_port=70000&cli_challenge={ok}"),
        "?cli_port=40000&cli_challenge=short",
    ] {
        assert_eq!(
            env.get(&format!("/v1/hub/login/github{query}"), None)
                .await
                .0,
            400,
            "{query}"
        );
    }
    assert_eq!(
        env.get(
            &format!("/v1/hub/login/bitbucket?cli_port=40000&cli_challenge={ok}"),
            None
        )
        .await
        .0,
        404
    );
    assert_eq!(
        env.post("/v1/hub/login/bitbucket/device", None, json!({}))
            .await
            .0,
        404
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_client_cannot_fill_the_pending_login_table() {
    let env = Env::start("").await;
    let start = |client: &str| {
        let url = format!(
            "{}/v1/hub/login/github?cli_port=40000&cli_challenge={}",
            env.url(),
            challenge("x")
        );
        let request = env
            .http
            .get(url)
            .header("x-forwarded-for", client.to_owned());
        async move { request.send().await.unwrap().status().as_u16() }
    };
    for _ in 0..8 {
        assert_eq!(start("203.0.113.1").await, 302);
    }
    assert_eq!(start("203.0.113.1").await, 429);
    assert_eq!(start("203.0.113.2").await, 302);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn denied_login_creates_nothing() {
    let env = Env::start("").await;
    env.fake.lock().unwrap().deny = true;
    let (location, cookie, _) = env.browser_start("github").await;
    let callback = env.authorize_at_forge(&location).await;
    let (status, headers, _) = env.callback(&callback, Some(&cookie)).await;
    assert_eq!(refused_page(status, &headers), "login_denied");
    assert!(env.kinds().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_code_and_bad_state_fail_without_a_session() {
    let env = Env::start("").await;
    env.fake.lock().unwrap().expire_codes = true;
    let (location, cookie, _) = env.browser_start("github").await;
    let callback = env.authorize_at_forge(&location).await;
    let (status, headers, _) = env.callback(&callback, Some(&cookie)).await;
    assert_eq!(refused_page(status, &headers), "login_failed");
    env.fake.lock().unwrap().expire_codes = false;

    // Wrong cookie, no cookie, a second login's cookie, and another provider's callback.
    for (cookie, provider) in [
        (Some(format!("{COOKIE}=nope")), "github"),
        (None, "github"),
        (Some("other=1".to_owned()), "github"),
    ] {
        let (location, _, _) = env.browser_start("github").await;
        let callback = env.authorize_at_forge(&location).await;
        let callback = callback.replace("/github/", &format!("/{provider}/"));
        let (status, _, body) = env.callback(&callback, cookie.as_deref()).await;
        assert_eq!(refusal(&(status, body)), (400, "login_failed"));
    }
    let (location, cookie, _) = env.browser_start("github").await;
    let callback = env.authorize_at_forge(&location).await;
    let (status, _, _) = env
        .callback(&callback.replace("/github/", "/gitlab/"), Some(&cookie))
        .await;
    assert_eq!(status, 400);
    // A failed attempt consumed the state: the right cookie no longer works.
    assert_eq!(env.callback(&callback, Some(&cookie)).await.0, 400);

    // Reuse after success.
    env.fake.lock().unwrap().current_user = 7;
    let (location, cookie, _) = env.browser_start("github").await;
    let callback = env.authorize_at_forge(&location).await;
    assert_eq!(env.callback(&callback, Some(&cookie)).await.0, 302);
    assert_eq!(env.callback(&callback, Some(&cookie)).await.0, 400);
    assert_eq!(env.kinds(), ["user_created", "user_signed_in"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exchange_with_wrong_verifier_consumes_the_code() {
    let env = Env::start("").await;
    let (location, cookie, verifier) = env.browser_start("github").await;
    let callback = env.authorize_at_forge(&location).await;
    let (_, headers, _) = env.callback(&callback, Some(&cookie)).await;
    let target = headers[header::LOCATION].to_str().unwrap();
    let code = target.rsplit('=').next().unwrap();
    assert_eq!(env.exchange(code, &"w".repeat(64)).await.0, 400);
    assert_eq!(env.exchange(code, &verifier).await.0, 400);
    assert_eq!(env.exchange("unknown", &verifier).await.0, 400);
}

// ---- device flow ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_flow_pending_slow_down_then_signed_in() {
    let env = Env::start("").await;
    env.fake.lock().unwrap().script.extend([
        Step::Pending,
        Step::SlowDown(None),
        Step::SlowDown(Some(30)),
        Step::Approve(7),
    ]);
    let (status, started) = env
        .post("/v1/hub/login/github/device", None, json!({}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(started["user_code"], "ABCD-1234");
    assert_eq!(started["interval"], 1);
    assert!(started.get("device_code").is_none());
    assert!(
        !env.log
            .lock()
            .unwrap()
            .iter()
            .any(|body| body.contains("devcode"))
    );
    let handle = started["handle"].clone();
    assert_eq!(
        poll(&env, &handle).await,
        (200, json!({"status": "pending", "interval": 1}))
    );
    assert_eq!(
        poll(&env, &handle).await,
        (200, json!({"status": "slow_down", "interval": 6}))
    );
    assert_eq!(
        poll(&env, &handle).await,
        (200, json!({"status": "slow_down", "interval": 30}))
    );
    let (status, signed) = poll(&env, &handle).await;
    assert_eq!(
        (status, signed["status"].as_str()),
        (200, Some("signed_in"))
    );
    assert_eq!(
        env.get("/v1/hub/session", Some(&secret(&signed))).await.0,
        200
    );
    // The handle is spent.
    assert_eq!(poll(&env, &handle).await.0, 400);
    let events = env.store.events_after(0, 1000).unwrap();
    assert_eq!(
        events.last().unwrap().data,
        r#"{"admin":false,"method":"device"}"#
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_flow_expired_denied_and_unavailable() {
    let env = Env::start("").await;
    for (step, status) in [(Step::Expired, "expired"), (Step::Denied, "denied")] {
        env.fake.lock().unwrap().script.push_back(step);
        let (_, started) = env
            .post("/v1/hub/login/gitlab/device", None, json!({}))
            .await;
        let poll = json!({"handle": started["handle"]});
        assert_eq!(
            env.post("/v1/hub/login/device/poll", None, poll.clone())
                .await,
            (200, json!({"status": status}))
        );
        assert_eq!(
            env.post("/v1/hub/login/device/poll", None, poll).await.0,
            400
        );
    }
    assert!(env.kinds().is_empty());
    assert_eq!(
        env.post("/v1/hub/login/device/poll", None, json!({"handle": "nope"}))
            .await
            .0,
        400
    );
    env.fake.lock().unwrap().no_device = true;
    for provider in ["github", "gitlab"] {
        let reply = env
            .post(&format!("/v1/hub/login/{provider}/device"), None, json!({}))
            .await;
        assert_eq!(refusal(&reply), (409, "device_flow_unavailable"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_poll_keeps_the_handle_through_a_forge_outage() {
    let env = Env::start("").await;
    env.fake
        .lock()
        .unwrap()
        .script
        .extend([Step::Unavailable, Step::Approve(7)]);
    let (_, started) = env
        .post("/v1/hub/login/github/device", None, json!({}))
        .await;
    let handle = started["handle"].clone();
    assert_eq!(
        poll(&env, &handle).await,
        (503, json!({"error": "unavailable"}))
    );
    let (status, signed) = poll(&env, &handle).await;
    assert_eq!(
        (status, signed["status"].as_str()),
        (200, Some("signed_in"))
    );
}

// ---- who may sign in ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_listed_users_and_admins_sign_in() {
    let env = Env::start_with("", "github:#7", |_| {}).await;
    // Not listed: refused on both paths, and nothing is stored.
    env.fake.lock().unwrap().current_user = 8;
    let (location, cookie, _) = env.browser_start("github").await;
    let callback = env.authorize_at_forge(&location).await;
    // The browser lands on the Hub page that says why, with no code for the CLI.
    let (status, headers, _) = env.callback(&callback, Some(&cookie)).await;
    assert_eq!(refused_page(status, &headers), "not_allowed");
    for (provider, user) in [("github", 8), ("gitlab", 7)] {
        env.fake
            .lock()
            .unwrap()
            .script
            .push_back(Step::Approve(user));
        let (_, started) = env
            .post(&format!("/v1/hub/login/{provider}/device"), None, json!({}))
            .await;
        let reply = poll(&env, &started["handle"]).await;
        assert_eq!(refusal(&reply), (403, "not_allowed"), "{provider}");
    }
    assert!(env.kinds().is_empty());
    assert!(env.store.list_users().unwrap().is_empty());
    // Listed: signs in, without admin.
    let signed = env.device_login("github", 7).await;
    assert_eq!(signed["admin"], false);
    assert_eq!(env.kinds(), ["user_created", "user_signed_in"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_listed_user_who_can_read_no_connected_repository_is_refused() {
    let env = Env::start_with("", "github:#5", |_| {}).await;
    env.fake.lock().unwrap().script.push_back(Step::Approve(5));
    let (_, started) = env
        .post("/v1/hub/login/github/device", None, json!({}))
        .await;
    let reply = poll(&env, &started["handle"]).await;
    assert_eq!(refusal(&reply), (403, "not_allowed"));
    assert!(env.kinds().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_a_public_repository_only_people_who_can_write_get_in() {
    let env = Env::start("").await;
    // The one connected repository people sign in through is public.
    {
        let mut fake = env.fake.lock().unwrap();
        fake.public.insert("acme/signin".into());
        fake.push.insert((7, "acme/signin".into()));
    }
    // Reading is no reason to trust someone on a public repository; push is.
    env.fake.lock().unwrap().script.push_back(Step::Approve(8));
    let (_, started) = env
        .post("/v1/hub/login/github/device", None, json!({}))
        .await;
    assert_eq!(
        refusal(&poll(&env, &started["handle"]).await),
        (403, "not_allowed")
    );
    assert!(env.kinds().is_empty());
    let session = secret(&env.device_login("github", 7).await);

    // Minting follows the same rule: public needs push, private needs only read.
    for (name, id) in [
        ("acme/readonly", 11),
        ("acme/writable", 12),
        ("acme/private", 13),
    ] {
        env.grant_repo(7, name, id);
    }
    {
        let mut fake = env.fake.lock().unwrap();
        fake.public
            .extend(["acme/readonly".to_owned(), "acme/writable".to_owned()]);
        fake.push.insert((7, "acme/writable".into()));
    }
    let events = env.kinds().len();
    // The refusal names who lacks which role where, and the one fix.
    let (_, signed_in) = env.get("/v1/hub/session", Some(&session)).await;
    let login = signed_in["user"]["login"].as_str().unwrap().to_owned();
    assert_eq!(
        env.create_token(&session, "acme/readonly").await,
        (
            403,
            json!({"error": "insufficient_role", "login": login,
                "repository": "acme/readonly", "visibility": "public", "has": "read",
                "needs": "triage", "message": format!("{login} has Read on public repository \
                acme/readonly; ask a repository owner for Triage or higher")})
        )
    );
    assert_eq!(env.kinds().len(), events);
    assert_eq!(env.create_token(&session, "acme/writable").await.0, 201);
    assert_eq!(env.create_token(&session, "acme/private").await.0, 201);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_no_allow_list_reading_a_connected_repository_is_enough() {
    let env = Env::start_with("github:#9", "", |_| {}).await;
    // Carol can read nothing connected: refused, and nothing is stored.
    env.fake.lock().unwrap().script.push_back(Step::Approve(5));
    let (_, started) = env
        .post("/v1/hub/login/github/device", None, json!({}))
        .await;
    let reply = poll(&env, &started["handle"]).await;
    assert_eq!(refusal(&reply), (403, "not_allowed"));
    assert!(env.kinds().is_empty());
    // Alice can, and the administrator needs no repository.
    assert_eq!(env.device_login("github", 7).await["admin"], false);
    assert_eq!(env.device_login("github", 9).await["admin"], true);
    // Access that is only on the forge's side of a connected name changing hands does not count.
    env.fake
        .lock()
        .unwrap()
        .repos
        .insert((8, "acme/signin".into()), 1);
    env.fake.lock().unwrap().script.push_back(Step::Approve(8));
    let (_, started) = env
        .post("/v1/hub/login/github/device", None, json!({}))
        .await;
    let reply = poll(&env, &started["handle"]).await;
    assert_eq!(refusal(&reply), (403, "not_allowed"));
    // GitLab follows the same rule, by project id.
    env.fake.lock().unwrap().script.push_back(Step::Approve(8));
    let (_, started) = env
        .post("/v1/hub/login/gitlab/device", None, json!({}))
        .await;
    let reply = poll(&env, &started["handle"]).await;
    assert_eq!(refusal(&reply), (403, "not_allowed"));
    assert_eq!(env.device_login("gitlab", 7).await["admin"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gitlab_sign_in_and_minting_need_developer_on_non_private_projects() {
    let env = Env::start("").await;
    let session = secret(&env.device_login("gitlab", 7).await);
    for (name, id) in [
        ("grp/private", 21),
        ("grp/pub-reporter", 22),
        ("grp/pub-developer", 23),
        ("grp/int-reporter", 24),
        ("grp/int-developer", 25),
    ] {
        env.grant_repo(7, name, id);
    }
    {
        let mut fake = env.fake.lock().unwrap();
        fake.public.extend([
            "grp/pub-reporter".to_owned(),
            "grp/pub-developer".to_owned(),
        ]);
        fake.internal.extend([
            "grp/int-reporter".to_owned(),
            "grp/int-developer".to_owned(),
        ]);
        fake.push.extend([
            (7, "grp/pub-developer".to_owned()),
            (7, "grp/int-developer".to_owned()),
        ]);
    }
    for (name, status) in [
        ("grp/private", 201),
        ("grp/pub-reporter", 403),
        ("grp/pub-developer", 201),
        ("grp/int-reporter", 403),
        ("grp/int-developer", 201),
    ] {
        assert_eq!(env.create_token(&session, name).await.0, status, "{name}");
    }
    let (_, refused) = env.create_token(&session, "grp/int-reporter").await;
    assert_eq!(
        (
            &refused["error"],
            &refused["visibility"],
            &refused["has"],
            &refused["needs"]
        ),
        (
            &json!("insufficient_role"),
            &json!("internal"),
            &json!("reporter"),
            &json!("developer")
        )
    );
    // A project nobody connected gets no token even when the person can read it.
    env.fake
        .lock()
        .unwrap()
        .repos
        .insert((7, "grp/unconnected".into()), 26);
    assert_eq!(
        env.create_token(&session, "grp/unconnected").await,
        (403, json!({"error": "not_connected"}))
    );
    // Signing in follows the same rule: the only connected project is a public one at Reporter.
    let env = Env::start("").await;
    {
        let mut fake = env.fake.lock().unwrap();
        fake.public.insert("acme/signin".into());
    }
    env.fake.lock().unwrap().script.push_back(Step::Approve(7));
    let (_, started) = env
        .post("/v1/hub/login/gitlab/device", None, json!({}))
        .await;
    assert_eq!(
        refusal(&poll(&env, &started["handle"]).await),
        (403, "not_allowed")
    );
    env.fake
        .lock()
        .unwrap()
        .push
        .insert((7, "acme/signin".into()));
    assert_eq!(env.device_login("gitlab", 7).await["admin"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logout_drops_the_session() {
    let env = Env::start("").await;
    let session = secret(&env.device_login("github", 7).await);
    assert_eq!(env.get("/v1/hub/session", Some(&session)).await.0, 200);
    let url = format!("{}/v1/hub/session", env.url());
    assert_eq!(
        env.send(Method::DELETE, &url, Some(&session), None, None)
            .await
            .0,
        204
    );
    assert_eq!(env.get("/v1/hub/session", Some(&session)).await.0, 401);
    assert_eq!(
        env.send(Method::DELETE, &url, Some(&session), None, None)
            .await
            .0,
        401
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_secret_from_a_crlf_file_or_the_environment_both_sign_in() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("secret");
    std::fs::write(&file, format!("{CLIENT_SECRET}\r\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let path = file.to_str().unwrap().to_owned();
    let from_file = Env::start_with("", "github:#7", |vars| {
        vars.remove("AGENTSCM_GITHUB_CLIENT_SECRET");
        vars.insert("AGENTSCM_GITHUB_CLIENT_SECRET_FILE".into(), path);
    })
    .await;
    // The fake rejects any secret that is not exactly the configured one.
    let signed = from_file.browser_login("github", 7).await;
    assert_eq!(signed["user"]["login"], "alice");
    let from_env = Env::start("").await;
    assert_eq!(
        from_env.browser_login("github", 7).await["user"]["login"],
        "alice"
    );
}

// ---- identity ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renamed_forge_user_keeps_the_hub_id() {
    let env = Env::start("").await;
    let first = env.browser_login("github", 7).await;
    env.fake
        .lock()
        .unwrap()
        .users
        .insert(7, "alice-renamed".into());
    let second = env.device_login("github", 7).await;
    assert_eq!(first["user"]["id"], second["user"]["id"]);
    assert_eq!(second["user"]["login"], "alice-renamed");
    assert_eq!(
        env.kinds(),
        [
            "user_created",
            "user_signed_in",
            "user_login_changed",
            "user_signed_in"
        ]
    );
}

// ---- tokens ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_minting_rechecks_forge_access() {
    let env = Env::start("").await;
    env.grant_repo(7, "acme/widgets", 4242);
    let signed = env.browser_login("github", 7).await;
    let session = secret(&signed);
    let (status, token) = env.create_token(&session, "acme/widgets").await;
    assert_eq!(status, 201, "{token}");
    assert_eq!(token["repository_id"], 4242);
    let agent = token["token"].as_str().unwrap();
    let (status, who) = env.get("/v1/hub/whoami", Some(agent)).await;
    assert_eq!(
        (status, who["token"]["repository"].as_str()),
        (200, Some("acme/widgets"))
    );
    assert_eq!(who["token"]["expires_at"], token["expires_at"]);
    let principal = env
        .store
        .authenticate(&agent_token_digest(agent).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    assert_eq!(principal.repository_id, Some(4242));
    assert!(
        !principal.read_only && token["read_only"] == false && who["token"]["read_only"] == false
    );

    // A read-only token is minted on request and listed as such.
    let body = json!({"name": "page", "repository": "acme/widgets", "ttl_seconds": 600, "read_only": true});
    let (status, page) = env.post("/v1/hub/tokens", Some(&session), body).await;
    assert_eq!((status, &page["read_only"]), (201, &json!(true)));
    let (status, who) = env.get("/v1/hub/whoami", page["token"].as_str()).await;
    assert_eq!((status, &who["token"]["read_only"]), (200, &json!(true)));
    let listed = env.get("/v1/hub/tokens", Some(&session)).await.1;
    let flags: Vec<_> = listed["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["read_only"].clone())
        .collect();
    assert!(flags.contains(&json!(true)) && flags.contains(&json!(false)));

    // The forge's spelling is stored, whatever case was asked for; a redirect is a refusal.
    let (status, upper) = env.create_token(&session, "ACME/Widgets").await;
    assert_eq!(
        (status, upper["repository"].as_str()),
        (201, Some("acme/widgets"))
    );
    let listed = env.get("/v1/hub/tokens", Some(&session)).await.1;
    assert!(
        listed["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["repository"] == "acme/widgets")
    );
    env.grant_repo(7, "acme/moved", 5);
    assert_eq!(env.create_token(&session, "acme/moved").await.0, 403);

    let events = env.kinds().len();
    for repo in ["acme/private", "acme/widgets/extra"] {
        assert_ne!(env.create_token(&session, repo).await.0, 201);
    }
    assert_eq!(
        env.create_token(&session, "acme/private").await,
        (403, json!({"error": "forbidden"}))
    );
    assert_eq!(env.kinds().len(), events);
    // Bad input is refused before the forge is asked.
    let lookups = env.fake.lock().unwrap().repo_lookups;
    for body in [
        json!({"name": "bad name", "repository": "acme/widgets", "ttl_seconds": 60}),
        json!({"name": "ok", "repository": "../x", "ttl_seconds": 60}),
        json!({"name": "ok", "repository": "acme/widgets", "ttl_seconds": 0}),
        json!({"name": "ok", "repository": "acme/widgets", "ttl_seconds": 99999999}),
        json!({"name": "ok", "repository": "acme/widgets", "ttl_seconds": 60, "extra": 1}),
    ] {
        assert_eq!(
            env.post("/v1/hub/tokens", Some(&session), body).await.0,
            400
        );
    }
    assert_eq!(env.fake.lock().unwrap().repo_lookups, lookups);

    // Access removed on the forge: the next mint is refused.
    env.fake.lock().unwrap().repos.clear();
    assert_eq!(env.create_token(&session, "acme/widgets").await.0, 403);
    // The forge token itself revoked: the person must sign in again.
    env.grant_repo(7, "acme/widgets", 4242);
    {
        let mut fake = env.fake.lock().unwrap();
        let tokens: Vec<_> = fake.tokens.keys().cloned().collect();
        fake.revoked.extend(tokens);
    }
    assert_eq!(env.create_token(&session, "acme/widgets").await.0, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn minting_needs_the_repository_connected_under_the_same_id() {
    let env = Env::start("").await;
    let session = secret(&env.device_login("github", 7).await);
    // Readable on the forge but not connected to the hub.
    env.fake
        .lock()
        .unwrap()
        .repos
        .insert((7, "acme/loose".into()), 321);
    let events = env.kinds().len();
    assert_eq!(
        env.create_token(&session, "acme/loose").await,
        (403, json!({"error": "not_connected"}))
    );
    // Connected as id 5, but the forge now says the name belongs to repository 9.
    env.grant_repo(7, "acme/reused", 5);
    env.fake
        .lock()
        .unwrap()
        .repos
        .insert((7, "acme/reused".into()), 9);
    assert_eq!(
        env.create_token(&session, "acme/reused").await,
        (403, json!({"error": "forbidden"}))
    );
    assert_eq!(env.kinds().len(), events);
    // Connected and matching: the name is matched ignoring case.
    env.grant_repo(7, "acme/Widgets", 4242);
    env.fake
        .lock()
        .unwrap()
        .repos
        .insert((7, "acme/widgets".into()), 4242);
    assert_eq!(env.create_token(&session, "acme/widgets").await.0, 201);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gitlab_projects_use_the_full_path() {
    let env = Env::start("gitlab:#7").await;
    env.grant_repo(7, "grp/sub/repo", 77);
    let session = secret(&env.device_login("gitlab", 7).await);
    let (status, token) = env.create_token(&session, "grp/sub/repo").await;
    assert_eq!((status, token["repository_id"].as_i64()), (201, Some(77)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gitlab_access_levels_come_from_the_project_or_its_group() {
    let env = Env::start("").await;
    let session = secret(&env.device_login("gitlab", 7).await);
    // (name, public, project level, group level, may mint)
    let cases = [
        ("grp/guest", false, Some(10), None, false),
        ("grp/planner", false, Some(15), None, false),
        ("grp/minimal", false, Some(5), None, false),
        ("grp/reporter", false, Some(20), None, true),
        ("grp/group-reporter", false, None, Some(20), true),
        ("grp/pub-group-dev", true, None, Some(30), true),
        ("grp/pub-group-reporter", true, None, Some(20), false),
        ("grp/pub-none", true, None, None, false),
    ];
    for (i, (name, public, project, group, _)) in cases.iter().enumerate() {
        env.grant_repo(7, name, 40 + i as i64);
        let mut fake = env.fake.lock().unwrap();
        if *public {
            fake.public.insert((*name).to_owned());
        }
        fake.levels
            .insert((7, (*name).to_owned()), (*project, *group));
    }
    for (name, _, _, _, allowed) in cases {
        let (status, reply) = env.create_token(&session, name).await;
        assert_eq!(status, if allowed { 201 } else { 403 }, "{name}");
        assert_eq!(allowed, reply["error"] != "insufficient_role", "{name}");
    }
    let (_, planner) = env.create_token(&session, "grp/planner").await;
    assert_eq!(planner["has"], "planner");
    assert!(
        planner["message"]
            .as_str()
            .unwrap()
            .contains("has Planner on private repository grp/planner"),
        "{planner}"
    );
    let (_, guest) = env.create_token(&session, "grp/guest").await;
    assert_eq!(
        (&guest["has"], &guest["needs"]),
        (&json!("guest"), &json!("reporter"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_are_listed_and_revoked_by_their_owner_only() {
    let env = Env::start("").await;
    env.grant_repo(7, "acme/widgets", 1);
    env.grant_repo(8, "acme/gadgets", 2);
    let alice = secret(&env.device_login("github", 7).await);
    let bob = secret(&env.device_login("github", 8).await);
    let (_, mine) = env.create_token(&alice, "acme/widgets").await;
    let (_, theirs) = env.create_token(&bob, "acme/gadgets").await;
    let (mine_secret, theirs_secret) = (
        mine["token"].as_str().unwrap(),
        theirs["token"].as_str().unwrap(),
    );

    let (status, listed) = env.get("/v1/hub/tokens", Some(&alice)).await;
    assert_eq!(status, 200);
    let tokens = listed["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0]["id"], mine["id"]);
    assert_eq!(tokens[0]["repository_id"], 1);
    let text = listed.to_string();
    assert!(
        !text.contains(mine_secret) && !text.contains("digest"),
        "{text}"
    );

    let their_path = format!("/v1/hub/tokens/{}", theirs["id"].as_str().unwrap());
    let url = format!("{}{their_path}", env.url());
    assert_eq!(
        env.send(Method::DELETE, &url, Some(&alice), None, None)
            .await
            .0,
        404
    );
    assert_eq!(env.get("/v1/hub/whoami", Some(theirs_secret)).await.0, 200);
    let mine_url = format!(
        "{}/v1/hub/tokens/{}",
        env.url(),
        mine["id"].as_str().unwrap()
    );
    assert_eq!(
        env.send(Method::DELETE, &mine_url, Some(&alice), None, None)
            .await
            .0,
        204
    );
    assert_eq!(
        env.send(Method::DELETE, &mine_url, Some(&alice), None, None)
            .await
            .0,
        404
    );
    assert_eq!(env.get("/v1/hub/whoami", Some(mine_secret)).await.0, 401);
    assert_eq!(
        env.get("/v1/hub/tokens", Some(&alice)).await.1["tokens"][0]["revoked"],
        true
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_token_revokes_only_itself() {
    let env = Env::start("").await;
    env.grant_repo(7, "acme/widgets", 1);
    let alice = secret(&env.device_login("github", 7).await);
    let (_, first) = env.create_token(&alice, "acme/widgets").await;
    let (_, second) = env.create_token(&alice, "acme/widgets").await;
    let (first, second) = (
        first["token"].as_str().unwrap(),
        second["token"].as_str().unwrap(),
    );
    let url = format!("{}/v1/hub/token", env.url());
    for refused in [None, Some(alice.as_str())] {
        assert_eq!(
            env.send(Method::DELETE, &url, refused, None, None).await.0,
            401
        );
    }
    assert_eq!(
        env.send(Method::DELETE, &url, Some(first), None, None)
            .await
            .0,
        204
    );
    assert_eq!(env.get("/v1/hub/whoami", Some(first)).await.0, 401);
    assert_eq!(env.get("/v1/hub/whoami", Some(second)).await.0, 200);
    // A revoked token is just an unknown one.
    assert_eq!(
        env.send(Method::DELETE, &url, Some(first), None, None)
            .await
            .0,
        401
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_and_agent_tokens_are_not_interchangeable() {
    let env = Env::start("").await;
    env.grant_repo(7, "acme/widgets", 1);
    let session = secret(&env.device_login("github", 7).await);
    let (_, token) = env.create_token(&session, "acme/widgets").await;
    let agent = token["token"].as_str().unwrap();
    for path in ["/v1/hub/session", "/v1/hub/tokens", "/v1/hub/users"] {
        let (status, body) = env.get(path, Some(agent)).await;
        assert_eq!(
            (status, body),
            (401, json!({"error": "unauthorized"})),
            "{path}"
        );
    }
    assert_eq!(env.get("/v1/hub/whoami", Some(&session)).await.0, 401);
    for bad in [None, Some("0".repeat(64))] {
        assert_eq!(env.get("/v1/hub/session", bad.as_deref()).await.0, 401);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admins_disable_and_enable_users() {
    let env = Env::start("github:adminal, gitlab:nobody").await;
    env.grant_repo(7, "acme/widgets", 1);
    let alice_signed = env.device_login("github", 7).await;
    let alice = secret(&alice_signed);
    let (_, token) = env.create_token(&alice, "acme/widgets").await;
    let agent = token["token"].as_str().unwrap();
    let admin_signed = env.device_login("github", 9).await;
    let admin = secret(&admin_signed);
    assert_eq!(admin_signed["admin"], true);

    assert_eq!(
        env.get("/v1/hub/users", Some(&alice)).await,
        (403, json!({"error": "forbidden"}))
    );
    let (status, users) = env.get("/v1/hub/users", Some(&admin)).await;
    assert_eq!(status, 200);
    assert_eq!(users["users"].as_array().unwrap().len(), 2);
    let alice_id = alice_signed["user"]["id"].as_str().unwrap();
    let (admin_id, disable) = (
        admin_signed["user"]["id"].as_str().unwrap(),
        format!("/v1/hub/users/{alice_id}/disable"),
    );
    assert_eq!(env.post(&disable, Some(&alice), json!({})).await.0, 403);
    assert_eq!(env.post(&disable, Some(&admin), json!({})).await.0, 204);
    assert_eq!(env.get("/v1/hub/whoami", Some(agent)).await.0, 401);
    assert_eq!(env.get("/v1/hub/session", Some(&alice)).await.0, 401);
    let events = env.store.events_after(0, 1000).unwrap();
    let event = events.iter().find(|e| e.kind == "user_disabled").unwrap();
    assert_eq!(
        (event.subject.as_str(), event.data.clone()),
        (alice_id, json!({"by": admin_id}).to_string())
    );
    // An administrator cannot lock themselves out.
    let own = format!("/v1/hub/users/{admin_id}/disable");
    assert_eq!(
        env.post(&own, Some(&admin), json!({})).await,
        (400, json!({"error": "invalid_request"}))
    );
    assert_eq!(env.get("/v1/hub/session", Some(&admin)).await.0, 200);
    // A disabled user cannot sign in, by browser or device, and no sign-in is recorded.
    let signed_in = || {
        env.kinds()
            .iter()
            .filter(|k| *k == "user_signed_in")
            .count()
    };
    let before = signed_in();
    env.fake.lock().unwrap().current_user = 7;
    let (location, cookie, _) = env.browser_start("github").await;
    let callback = env.authorize_at_forge(&location).await;
    // The browser lands on the Hub page that says why, with no code for the CLI.
    let (status, headers, _) = env.callback(&callback, Some(&cookie)).await;
    assert_eq!(refused_page(status, &headers), "user_disabled");
    assert_eq!(signed_in(), before);
    env.fake.lock().unwrap().script.push_back(Step::Approve(7));
    let (_, started) = env
        .post("/v1/hub/login/github/device", None, json!({}))
        .await;
    let reply = env
        .post(
            "/v1/hub/login/device/poll",
            None,
            json!({"handle": started["handle"]}),
        )
        .await;
    assert_eq!(refusal(&reply), (403, "user_disabled"));

    assert_eq!(
        env.post(
            &format!("/v1/hub/users/{alice_id}/enable"),
            Some(&admin),
            json!({})
        )
        .await
        .0,
        204
    );
    assert_eq!(env.get("/v1/hub/whoami", Some(agent)).await.0, 200);
    let events = env.store.events_after(0, 1000).unwrap();
    let enabled = events.iter().find(|e| e.kind == "user_enabled").unwrap();
    assert_eq!(enabled.data, json!({"by": admin_id}).to_string());
    assert_eq!(signed_in(), before);
    assert_eq!(
        env.post("/v1/hub/users/usr_unknown/disable", Some(&admin), json!({}))
            .await
            .0,
        404
    );
}

// ---- configuration ----

fn config(vars: &[(&str, &str)]) -> Result<LoginConfig, String> {
    let vars: HashMap<_, _> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    LoginConfig::from_lookup("http://127.0.0.1:7332", |name| vars.get(name).cloned())
        .map_err(|e| e.to_string())
}

#[test]
fn secrets_come_from_a_private_file_or_the_environment_not_both() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("secret");
    std::fs::write(&file, "file-secret-value\n").unwrap();
    let path = file.to_str().unwrap();
    let id = ("AGENTSCM_GITHUB_CLIENT_ID", "id");
    let file_var = ("AGENTSCM_GITHUB_CLIENT_SECRET_FILE", path);
    let env_var = ("AGENTSCM_GITHUB_CLIENT_SECRET", "env-secret-value");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(
            config(&[id, file_var])
                .unwrap_err()
                .contains("group or others")
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let from_file = config(&[id, file_var]).unwrap();
    let from_env = config(&[id, env_var]).unwrap();
    for (config, secret) in [
        (&from_file, "file-secret-value"),
        (&from_env, "env-secret-value"),
    ] {
        let debug = format!("{config:?}");
        assert!(
            !debug.contains(secret) && debug.contains("REDACTED"),
            "{debug}"
        );
    }
    assert!(config(&[id, file_var, env_var]).is_err());
    assert!(config(&[id]).is_err());
    assert!(
        config(&[
            id,
            (
                "AGENTSCM_GITHUB_CLIENT_SECRET_FILE",
                dir.path().to_str().unwrap()
            )
        ])
        .is_err()
    );
    std::fs::write(&file, "\n").unwrap();
    assert!(config(&[id, file_var]).is_err());
    assert!(config(&[]).is_ok());
}

#[test]
fn forge_urls_and_admins_are_vetted() {
    let base = [
        ("AGENTSCM_GITLAB_CLIENT_ID", "id"),
        ("AGENTSCM_GITLAB_CLIENT_SECRET", "s3cret-value"),
    ];
    let with = |extra: (&'static str, &'static str)| config(&[base[0], base[1], extra]);
    assert!(with(("AGENTSCM_GITLAB_URL", "https://gitlab.example.com")).is_ok());
    for url in [
        "http://example.com",
        "https://u:p@gitlab.example.com",
        "https://gitlab.example.com/?q=1",
    ] {
        let error = with(("AGENTSCM_GITLAB_URL", url)).unwrap_err();
        assert!(
            !error.contains("example.com") && !error.contains("s3cret"),
            "{error}"
        );
    }
    assert!(with(("AGENTSCM_ADMINS", "github:alice,gitlab:Bob,github:#12")).is_ok());
    assert!(with(("AGENTSCM_ALLOWED_USERS", "github:#1, gitlab:carol")).is_ok());
    assert!(with(("AGENTSCM_ALLOWED_USERS", "carol")).is_err());
    assert!(with(("AGENTSCM_ADMINS", "")).is_ok());
    for admins in [
        "github:#0",
        "github:#x",
        "alice",
        "bitbucket:alice",
        "github:",
        "github:alice,",
        "github:a b",
    ] {
        assert!(with(("AGENTSCM_ADMINS", admins)).is_err(), "{admins}");
    }
    // A hub without providers needs no public URL.
    assert!(LoginConfig::from_lookup("not a url", |_| None).is_ok());
    // A default public URL that is not usable says to set --public-url.
    let error = LoginConfig::from_lookup("http://0.0.0.0:7332", |name| match name {
        "AGENTSCM_GITLAB_CLIENT_ID" => Some("x".into()),
        "AGENTSCM_GITLAB_CLIENT_SECRET" => Some("y".into()),
        _ => None,
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("--public-url"), "{error}");
}

// ---- secrecy ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forge_tokens_and_client_secrets_never_leave_the_hub() {
    let env = Env::start("github:adminal,gitlab:#8").await;
    env.grant_repo(7, "acme/widgets", 1);
    let browser = env.browser_login("github", 7).await;
    let device = env.device_login("gitlab", 8).await;
    let admin = secret(&env.device_login("github", 9).await);
    let session = secret(&browser);
    env.create_token(&session, "acme/widgets").await;
    env.get("/v1/hub/tokens", Some(&session)).await;
    env.get("/v1/hub/session", Some(&secret(&device))).await;
    env.get("/v1/hub/users", Some(&admin)).await;
    env.create_token(&session, "acme/private").await;

    let forge_tokens: Vec<String> = env.fake.lock().unwrap().tokens.keys().cloned().collect();
    assert!(forge_tokens.len() >= 3);
    let events: String = env
        .store
        .events_after(0, 1000)
        .unwrap()
        .iter()
        .map(|e| e.data.clone())
        .collect();
    let bodies = env.log.lock().unwrap().join("\n");
    for needle in forge_tokens
        .iter()
        .map(String::as_str)
        .chain([CLIENT_SECRET])
    {
        assert!(
            !bodies.contains(needle) && !events.contains(needle),
            "{needle}"
        );
    }
}

// ---- CLI ----

struct Cli {
    bin: &'static str,
    config: TempDir,
}

impl Cli {
    fn new() -> Self {
        Self {
            bin: env!("CARGO_BIN_EXE_agentscm"),
            config: TempDir::new().unwrap(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(self.bin);
        command
            .args(["hub"])
            .args(args)
            .env("XDG_CONFIG_HOME", self.config.path())
            // Tests never open a real browser: sign-in uses the code flow, or prints its URL.
            .env("AGENTSCM_NO_BROWSER", "1")
            .env("NO_PROXY", "127.0.0.1");
        command
    }

    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let output = self.command(args).output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn session_file(&self) -> std::path::PathBuf {
        self.config.path().join("agentscm/hub-sessions.json")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_signs_in_with_the_device_flow_and_manages_tokens() {
    let env = Env::start("").await;
    env.grant_repo(7, "acme/widgets", 4242);
    env.fake
        .lock()
        .unwrap()
        .script
        .extend([Step::Pending, Step::Approve(7)]);
    let cli = Cli::new();
    let hub = env.url();
    let (ok, _, stderr) = cli.run(&["login", "--hub", &hub, "--provider", "github"]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("ABCD-1234")
            && stderr.contains(&format!(
                "Signed in to {} as alice (GitHub).",
                hub.split_once("://").unwrap().1
            )),
        "{stderr}"
    );
    let file = cli.session_file();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
    let store: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(store["version"], 2);
    assert_eq!(store["default_hub"], json!(hub));
    let saved = store["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|saved| saved["hub"] == hub)
        .unwrap();
    assert!(!stderr.contains(saved["session"].as_str().unwrap()));

    let (ok, stdout, stderr) = cli.run(&[
        "token",
        "create",
        "--name",
        "builder",
        "--repo",
        "acme/widgets",
        "--ttl",
        "10m",
    ]);
    assert!(ok, "{stderr}");
    let agent = stdout.trim();
    assert_eq!(agent.len(), 64, "stdout holds only the secret");
    assert!(!stderr.contains(agent));
    assert_eq!(
        env.get("/v1/hub/whoami", Some(agent)).await.1["token"]["name"],
        "builder"
    );

    let (ok, stdout, _) = cli.run(&["token", "list"]);
    assert!(ok);
    let line = stdout.lines().next().unwrap();
    assert!(
        line.contains("builder") && line.contains("acme/widgets") && !line.contains(agent),
        "{line}"
    );
    let id = line.split(' ').next().unwrap();
    let (ok, _, stderr) = cli.run(&["token", "revoke", id]);
    assert!(ok, "{stderr}");
    assert_eq!(env.get("/v1/hub/whoami", Some(agent)).await.0, 401);
    assert!(!cli.run(&["token", "revoke", id]).0);
    assert!(
        !cli.run(&[
            "token",
            "create",
            "--name",
            "x",
            "--repo",
            "acme/widgets",
            "--ttl",
            "5x"
        ])
        .0
    );
    // A session is only ever sent to the hub that issued it.
    let (ok, _, stderr) = cli.run(&["token", "list", "--hub", "https://other.example.com"]);
    assert!(
        !ok && stderr.contains("no saved session for this hub"),
        "{stderr}"
    );

    // Readable on the forge but not connected: the CLI says how to connect it.
    env.fake
        .lock()
        .unwrap()
        .repos
        .insert((7, "acme/other".into()), 5150);
    let (ok, _, stderr) = cli.run(&[
        "token",
        "create",
        "--name",
        "builder",
        "--repo",
        "acme/other",
    ]);
    assert!(
        !ok && stderr.contains("connect the repository first")
            && stderr.contains("connections refresh"),
        "{stderr}"
    );

    // Read alone on a public repository: the Hub's sentence names the role to ask for.
    env.grant_repo(7, "acme/open", 31);
    env.fake.lock().unwrap().public.insert("acme/open".into());
    let (ok, _, stderr) = cli.run(&[
        "token",
        "create",
        "--name",
        "builder",
        "--repo",
        "acme/open",
    ]);
    assert!(
        !ok && stderr.contains("has Read on public repository acme/open")
            && stderr.contains("for Triage or higher"),
        "{stderr}"
    );

    env.fake.lock().unwrap().repos.clear();
    let (ok, _, stderr) = cli.run(&[
        "token",
        "create",
        "--name",
        "builder",
        "--repo",
        "acme/widgets",
    ]);
    assert!(
        !ok && stderr.contains("can't see that repository"),
        "{stderr}"
    );

    // Logout ends the session on the hub and deletes the file; doing it twice is fine.
    let session = saved["session"].as_str().unwrap();
    assert_eq!(env.get("/v1/hub/session", Some(session)).await.0, 200);
    let (ok, _, stderr) = cli.run(&["logout"]);
    assert!(ok && stderr.contains("signed out"), "{stderr}");
    assert!(!file.exists());
    assert_eq!(env.get("/v1/hub/session", Some(session)).await.0, 401);
    let (ok, _, stderr) = cli.run(&["logout"]);
    assert!(ok && stderr.contains("not signed in"), "{stderr}");

    // A rejected read can still use a legacy session without rewriting its local storage.
    let legacy = cli.config.path().join("agentscm/hub-session.json");
    std::fs::write(
        &legacy,
        json!({"hub": hub, "session": "0".repeat(64)}).to_string(),
    )
    .unwrap();
    let (ok, _, stderr) = cli.run(&["token", "list"]);
    assert!(!ok && stderr.contains("run agentscm hub login"), "{stderr}");
    assert!(!file.exists() && legacy.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_falls_back_to_the_loopback_flow_when_device_flow_is_unavailable() {
    let env = Env::start("gitlab:#8").await;
    {
        let mut fake = env.fake.lock().unwrap();
        fake.no_device = true;
        fake.current_user = 8;
    }
    let cli = Cli::new();
    let hub = env.url();
    let mut child = cli
        .command(&["login", "--hub", &hub, "--provider", "gitlab"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let url = loop {
        let mut line = String::new();
        assert!(stderr.read_line(&mut line).unwrap() > 0, "cli exited early");
        if line.starts_with(&format!("{hub}/v1/hub/login/gitlab?cli_port=")) {
            break line.trim().to_owned();
        }
    };
    let (_, headers, _) = env.send(Method::GET, &url, None, None, None).await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let cookie = headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let callback = env.authorize_at_forge(&location).await;
    let (status, headers, _) = env.callback(&callback, Some(&cookie)).await;
    assert_eq!(status, 302);
    let loopback = headers[header::LOCATION].to_str().unwrap();
    assert!(loopback.starts_with("http://127.0.0.1:"));
    // The CLI trades the code first, then sends the browser to the Hub's signed-in page.
    let response = env.http.get(loopback).send().await.unwrap();
    assert_eq!(response.status(), 302);
    let page = response.headers()[header::LOCATION].to_str().unwrap();
    assert!(
        page.starts_with(&format!(
            "{hub}/ui/#/signed-in?status=signed_in&login=bob&provider=gitlab&computer="
        )),
        "{page}"
    );
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stderr, &mut rest).unwrap();
    assert!(child.wait().unwrap().success(), "{rest}");
    assert!(rest.contains("as bob (GitLab)."), "{rest}");
    let file = cli.session_file();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
    let leftovers = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .filter(|entry| {
            let name = entry.as_ref().unwrap().file_name();
            name.to_string_lossy().starts_with("hub-sessions.json.tmp-")
        })
        .count();
    assert_eq!(leftovers, 0);

    // The sign-in also signed this computer in; without a terminal nobody was asked about
    // auto-connect.
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let device = &saved["sessions"][0]["device"];
    assert!(
        device["id"].as_str().unwrap().starts_with("dev_"),
        "{saved}"
    );
    assert!(
        saved["sessions"][0].get("auto_connect").is_none(),
        "{saved}"
    );
    let (ok, stdout, stderr) = cli.run(&["devices", "list"]);
    assert!(ok && stdout.contains("(this computer)"), "{stdout}{stderr}");
    assert!(!stdout.contains(device["secret"].as_str().unwrap()));
    let (ok, _, stderr) = cli.run(&["auto-connect", "on"]);
    assert!(ok && stderr.contains("Auto-connect is on"), "{stderr}");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["sessions"][0]["auto_connect"], json!(true));
    // Signing out signs the computer out on the Hub too.
    let (ok, _, stderr) = cli.run(&["logout"]);
    assert!(ok, "{stderr}");
    assert!(env.kinds().contains(&"device_revoked".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_browser_sign_in_ends_the_cli_wait_with_the_reason() {
    let env = Env::start_with("", "github:#7", |_| {}).await;
    {
        let mut fake = env.fake.lock().unwrap();
        fake.no_device = true;
        fake.current_user = 8;
    }
    let cli = Cli::new();
    let hub = env.url();
    let mut child = cli
        .command(&["login", "--hub", &hub, "--provider", "github"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let url = loop {
        let mut line = String::new();
        assert!(stderr.read_line(&mut line).unwrap() > 0, "cli exited early");
        if line.starts_with(&format!("{hub}/v1/hub/login/github?cli_port=")) {
            break line.trim().to_owned();
        }
    };
    let (_, headers, _) = env.send(Method::GET, &url, None, None, None).await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let cookie = headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let callback = env.authorize_at_forge(&location).await;
    let (status, headers, _) = env.callback(&callback, Some(&cookie)).await;
    assert_eq!(status, 302);
    let loopback = headers[header::LOCATION].to_str().unwrap();
    assert!(
        loopback.contains("/callback?error=not_allowed&message="),
        "{loopback}"
    );
    // The CLI answers by sending the browser to the Hub's page, and stops waiting.
    let response = env.http.get(loopback).send().await.unwrap();
    assert_eq!(response.status(), 302);
    let page = response.headers()[header::LOCATION].to_str().unwrap();
    assert_eq!(
        page,
        format!("{hub}/ui/#/signed-in?status=refused&reason=not_allowed")
    );
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stderr, &mut rest).unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(rest.contains("ask a Hub admin to add you"), "{rest}");
    assert!(!cli.session_file().exists());
}
