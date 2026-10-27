//! Computer credentials against an in-process fake forge: sign-in, renewal with the role
//! re-checked through the Hub's own connection credential, refusals and revocation.
//! No live accounts.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agentscm::backend::LocalBackend;
use agentscm::hub::login::{Login, LoginConfig};
use agentscm::hub::store::{self, Store, unix_now};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::{HubState, router};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const CLIENT_ID: &str = "fake-client-id";
const CLIENT_SECRET: &str = "fake-client-secret-value";
/// The Hub's own connection credential (a GitHub PAT or a GitLab token).
const HUB_TOKEN: &str = "synthetic-hub-connection-token";
const HUB_BOT: i64 = 100;
const ALICE: i64 = 7;
const BOB: i64 = 8;
const ADMIN: i64 = 9;
/// An Enterprise Managed User, whose login has an underscore.
const EMU: i64 = 11;
const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 4242;

#[derive(Default)]
struct Fake {
    users: HashMap<i64, String>,
    /// Bearer token to forge user.
    tokens: HashMap<String, i64>,
    /// Who approves the next device sign-in.
    approve: i64,
    /// Repository name to (id, visibility: private, public or internal).
    repos: HashMap<String, (i64, &'static str)>,
    /// GitHub (repository, user) to (permission, role_name); GitLab uses the level map.
    roles: HashMap<(String, i64), (&'static str, &'static str)>,
    levels: HashMap<(i64, i64), i64>,
    /// When false, the Hub's credential cannot read roles (403).
    hub_reads_roles: bool,
    /// A fork's name to the repository it was forked from.
    forks: HashMap<String, String>,
    role_reads: u32,
}

type Shared = Arc<Mutex<Fake>>;

fn caller(fake: &Fake, headers: &HeaderMap) -> Option<i64> {
    let token = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    fake.tokens.get(token).copied()
}

fn status(code: StatusCode) -> Response {
    (code, Json(json!({"message": code.as_str()}))).into_response()
}

fn user(fake: &Shared, headers: &HeaderMap, field: &str) -> Response {
    let fake = fake.lock().unwrap();
    match caller(&fake, headers) {
        Some(id) => Json(json!({"id": id, field: fake.users[&id]})).into_response(),
        None => status(StatusCode::UNAUTHORIZED),
    }
}

fn device_code(gitlab: bool) -> Response {
    let _ = gitlab;
    Json(json!({"device_code": "devcode", "user_code": "ABCD-1234",
        "verification_uri": "http://127.0.0.1/device", "expires_in": 900, "interval": 1}))
    .into_response()
}

fn device_token(fake: &Shared, form: HashMap<String, String>) -> Response {
    let mut fake = fake.lock().unwrap();
    if form.get("client_id").map(String::as_str) != Some(CLIENT_ID) {
        return Json(json!({"error": "invalid_client"})).into_response();
    }
    let user = fake.approve;
    let token = format!("synthetic-user-token-{user}-{}", fake.tokens.len());
    fake.tokens.insert(token.clone(), user);
    Json(json!({"access_token": token, "token_type": "bearer", "expires_in": 7200})).into_response()
}

/// GitHub repository: the user's own view (with `permissions`, for sign-in) or the Hub's.
fn github_repo(fake: &Shared, headers: &HeaderMap, name: String) -> Response {
    let fake = fake.lock().unwrap();
    let Some(who) = caller(&fake, headers) else {
        return status(StatusCode::UNAUTHORIZED);
    };
    if let Some(parent) = fake.forks.get(&name) {
        return Json(
            json!({"id": 1, "full_name": name, "private": false, "fork": true,
            "parent": {"full_name": parent}}),
        )
        .into_response();
    }
    let Some(&(id, visibility)) = fake.repos.get(&name) else {
        return status(StatusCode::NOT_FOUND);
    };
    let mut body = json!({"id": id, "full_name": name, "private": visibility == "private"});
    if who != HUB_BOT {
        let Some(&(permission, _)) = fake.roles.get(&(name.clone(), who)) else {
            return status(StatusCode::NOT_FOUND);
        };
        body["permissions"] = json!({"pull": true,
            "push": matches!(permission, "write" | "admin"), "triage": false});
    }
    Json(body).into_response()
}

fn github_permission(fake: &Shared, headers: &HeaderMap, name: String, login: String) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.role_reads += 1;
    if caller(&fake, headers) != Some(HUB_BOT) {
        return status(StatusCode::UNAUTHORIZED);
    }
    if !fake.hub_reads_roles {
        return status(StatusCode::FORBIDDEN);
    }
    let Some(id) = fake
        .users
        .iter()
        .find(|(_, known)| known.eq_ignore_ascii_case(&login))
        .map(|(id, _)| *id)
    else {
        return status(StatusCode::NOT_FOUND);
    };
    let (permission, role) = fake.roles.get(&(name, id)).copied().unwrap_or(("none", ""));
    Json(json!({"permission": permission, "role_name": role, "user": {"id": id, "login": login}}))
        .into_response()
}

fn gitlab_project(fake: &Shared, headers: &HeaderMap, path: String) -> Response {
    let fake = fake.lock().unwrap();
    let Some(who) = caller(&fake, headers) else {
        return status(StatusCode::UNAUTHORIZED);
    };
    let found = fake.repos.iter().find(|(name, (id, _))| {
        path.parse::<i64>().ok() == Some(*id) || name.as_str() == path.replace("%2F", "/")
    });
    let Some((name, &(id, visibility))) = found else {
        return status(StatusCode::NOT_FOUND);
    };
    let mut body = json!({"id": id, "path_with_namespace": name, "visibility": visibility});
    if who != HUB_BOT {
        let Some(level) = fake.levels.get(&(id, who)) else {
            return status(StatusCode::NOT_FOUND);
        };
        body["permissions"] = json!({"project_access": {"access_level": level}});
    }
    Json(body).into_response()
}

fn gitlab_member(fake: &Shared, headers: &HeaderMap, project: i64, user: i64) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.role_reads += 1;
    if caller(&fake, headers) != Some(HUB_BOT) {
        return status(StatusCode::UNAUTHORIZED);
    }
    if !fake.hub_reads_roles {
        return status(StatusCode::FORBIDDEN);
    }
    match fake.levels.get(&(project, user)) {
        Some(level) => {
            Json(json!({"id": user, "access_level": level, "state": "active"})).into_response()
        }
        None => status(StatusCode::NOT_FOUND),
    }
}

async fn start_fake() -> (Shared, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let fake: Shared = Arc::new(Mutex::new(Fake {
        users: HashMap::from([
            (ALICE, "alice".into()),
            (BOB, "bob".into()),
            (ADMIN, "admin".into()),
            (EMU, "carol_acme".into()),
            (HUB_BOT, "hubbot".into()),
        ]),
        tokens: HashMap::from([(HUB_TOKEN.into(), HUB_BOT)]),
        hub_reads_roles: true,
        ..Fake::default()
    }));
    type F = Form<HashMap<String, String>>;
    type H = HeaderMap;
    let app = Router::new()
        .route("/login/device/code", post(|| async { device_code(false) }))
        // The browser flow: the forge approves at once and sends the code back.
        .route(
            "/login/oauth/authorize",
            get(|axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>| async move {
                let to = format!("{}?code=browser-code&state={}", query["redirect_uri"], query["state"]);
                (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
            }),
        )
        .route("/oauth/authorize_device", post(|| async { device_code(true) }))
        .route("/login/oauth/access_token", post(|State(f): State<Shared>, Form(x): F| async move { device_token(&f, x) }))
        .route("/oauth/token", post(|State(f): State<Shared>, Form(x): F| async move { device_token(&f, x) }))
        .route("/user", get(|State(f): State<Shared>, h: H| async move { user(&f, &h, "login") }))
        .route("/api/v4/user", get(|State(f): State<Shared>, h: H| async move { user(&f, &h, "username") }))
        .route("/repos/{o}/{n}", get(|State(f): State<Shared>, h: H, Path((o, n)): Path<(String, String)>| async move { github_repo(&f, &h, format!("{o}/{n}")) }))
        .route("/repos/{o}/{n}/collaborators/{l}/permission", get(|State(f): State<Shared>, h: H, Path((o, n, l)): Path<(String, String, String)>| async move { github_permission(&f, &h, format!("{o}/{n}"), l) }))
        .route("/api/v4/projects/{p}", get(|State(f): State<Shared>, h: H, Path(p): Path<String>| async move { gitlab_project(&f, &h, p) }))
        .route("/api/v4/projects/{p}/members/all/{u}", get(|State(f): State<Shared>, h: H, Path((p, u)): Path<(i64, i64)>| async move { gitlab_member(&f, &h, p, u) }))
        .with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (fake, base)
}

struct Env {
    hub: String,
    base: String,
    provider: &'static str,
    fake: Shared,
    store: Arc<dyn Store>,
    http: reqwest::Client,
    dir: TempDir,
}

impl Env {
    async fn start(provider: &'static str) -> Self {
        let (fake, base) = start_fake().await;
        let dir = TempDir::new().unwrap();
        let mut env = Self {
            hub: String::new(),
            base,
            provider,
            fake,
            store: store::open(&format!(
                "sqlite://{}",
                dir.path().join("hub.sqlite").display()
            ))
            .unwrap(),
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            dir,
        };
        env.hub = env.serve("").await;
        let host = env.base.trim_start_matches("http://").to_owned();
        let connection = env.store.pat_connection(&host).unwrap().unwrap().id;
        env.store
            .upsert_repository(&connection, &host, REPO_ID, REPO, None, None)
            .unwrap();
        // alice can write; bob only reads; the admin reads.
        env.set_repo(REPO, REPO_ID, "private");
        env.role(ALICE, "write", "write", 30);
        env.role(BOB, "read", "read", 20);
        env.role(ADMIN, "read", "read", 20);
        env
    }

    /// A Hub process over this store; `allowed` is `AGENTSCM_ALLOWED_USERS`.
    async fn serve(&self, allowed: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let prefix = if self.provider == "gitlab" {
            "GITLAB"
        } else {
            "GITHUB"
        };
        let base = self.base.clone();
        let mut vars: HashMap<String, String> = HashMap::from([
            (format!("AGENTSCM_{prefix}_CLIENT_ID"), CLIENT_ID.into()),
            (
                format!("AGENTSCM_{prefix}_CLIENT_SECRET"),
                CLIENT_SECRET.into(),
            ),
            (format!("AGENTSCM_{prefix}_URL"), base.clone()),
            (
                "AGENTSCM_ADMINS".into(),
                format!("{}:#{ADMIN}", self.provider),
            ),
        ]);
        if !allowed.is_empty() {
            vars.insert("AGENTSCM_ALLOWED_USERS".into(), allowed.into());
        }
        if self.provider == "gitlab" {
            vars.insert("AGENTSCM_GITLAB_TOKEN".into(), HUB_TOKEN.into());
        } else {
            vars.insert("AGENTSCM_GITHUB_API_URL".into(), base.clone());
            vars.insert("AGENTSCM_GITHUB_PAT".into(), HUB_TOKEN.into());
        }
        let login = LoginConfig::from_lookup(&url, |name| vars.get(name).cloned()).unwrap();
        let secrets = self.dir.path().join("secrets");
        sync::prepare_secrets_dir(&secrets, true).unwrap();
        let sync_config =
            SyncConfig::from_lookup(&url, secrets, 14, |name| vars.get(name).cloned()).unwrap();
        let state = HubState::new(
            self.store.clone(),
            Arc::new(LocalBackend::open(self.dir.path().join("objects")).unwrap()),
        )
        .with_login(Login::new(login).unwrap())
        .with_sync(Sync::new(sync_config).unwrap());
        sync::start(&state).await.unwrap();
        let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    /// Connects another repository on the Hub and gives `user` read on it.
    fn connect_repo(&self, name: &str, id: i64, user: i64) {
        let host = self.base.trim_start_matches("http://");
        let connection = self.store.pat_connection(host).unwrap().unwrap().id;
        self.store
            .upsert_repository(&connection, host, id, name, None, None)
            .unwrap();
        self.set_repo(name, id, "private");
        let mut fake = self.fake.lock().unwrap();
        fake.roles.insert((name.into(), user), ("read", "read"));
        fake.levels.insert((id, user), 20);
    }

    fn set_repo(&self, name: &str, id: i64, visibility: &'static str) {
        self.fake
            .lock()
            .unwrap()
            .repos
            .insert(name.into(), (id, visibility));
    }

    /// The GitHub permission and role, or the GitLab access level, of `user` on REPO.
    fn role(&self, user: i64, permission: &'static str, role: &'static str, level: i64) {
        let mut fake = self.fake.lock().unwrap();
        fake.roles.insert((REPO.into(), user), (permission, role));
        fake.levels.insert((REPO_ID, user), level);
    }

    fn drop_role(&self, user: i64) {
        let mut fake = self.fake.lock().unwrap();
        fake.roles.remove(&(REPO.into(), user));
        fake.levels.remove(&(REPO_ID, user));
    }

    async fn call(
        &self,
        hub: &str,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self.http.request(method, format!("{hub}{path}"));
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str, bearer: &str) -> (u16, Value) {
        self.call(&self.hub, Method::GET, path, Some(bearer), None)
            .await
    }

    async fn delete(&self, path: &str, bearer: &str) -> u16 {
        self.call(&self.hub, Method::DELETE, path, Some(bearer), None)
            .await
            .0
    }

    /// Device-flow sign-in as `user`, asking for a computer credential named `name`.
    async fn sign_in_at(&self, hub: &str, user: i64, device: Option<Value>) -> (u16, Value) {
        self.fake.lock().unwrap().approve = user;
        let (status, started) = self
            .call(
                hub,
                Method::POST,
                &format!("/v1/hub/login/{}/device", self.provider),
                None,
                Some(json!({})),
            )
            .await;
        assert_eq!(status, 200, "{started}");
        let mut body = json!({"handle": started["handle"]});
        if let Some(device) = device {
            body["device"] = device;
        }
        self.call(
            hub,
            Method::POST,
            "/v1/hub/login/device/poll",
            None,
            Some(body),
        )
        .await
    }

    /// Returns (session, computer credential, computer id).
    async fn sign_in(&self, user: i64, name: &str) -> (String, String, String) {
        let (status, signed) = self
            .sign_in_at(&self.hub, user, Some(json!({"name": name})))
            .await;
        assert_eq!(
            (status, signed["status"].as_str()),
            (200, Some("signed_in")),
            "{signed}"
        );
        (
            signed["session"].as_str().unwrap().into(),
            signed["device"]["secret"].as_str().unwrap().into(),
            signed["device"]["id"].as_str().unwrap().into(),
        )
    }

    async fn mint_at(&self, hub: &str, device: &str, body: Value) -> (u16, Value) {
        self.call(
            hub,
            Method::POST,
            "/v1/hub/device/tokens",
            Some(device),
            Some(body),
        )
        .await
    }

    async fn mint(&self, device: &str) -> (u16, Value) {
        let body = json!({"name": "client-laptop", "repository": REPO});
        self.mint_at(&self.hub, device, body).await
    }

    async fn whoami(&self, token: &str) -> u16 {
        self.get("/v1/hub/whoami", token).await.0
    }

    fn kinds(&self) -> Vec<String> {
        self.store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }
}

fn token(minted: &Value) -> (String, String) {
    (
        minted["id"].as_str().unwrap().into(),
        minted["token"].as_str().unwrap().into(),
    )
}

async fn renewal_rechecks_the_role(provider: &'static str) {
    let env = Env::start(provider).await;
    let (_, device, device_id) = env.sign_in(ALICE, "alice's MacBook Pro").await;
    let (status, first) = env.mint(&device).await;
    assert_eq!(status, 201, "{first}");
    let now = unix_now();
    let expires_at = first["expires_at"].as_i64().unwrap();
    assert!(
        (now + 86_390..=now + 86_410).contains(&expires_at),
        "{first}"
    );
    assert_eq!(first["device_id"], json!(device_id));
    let (first_id, first_secret) = token(&first);
    assert_eq!(env.whoami(&first_secret).await, 200);

    // Renewal mints a new token, then the computer revokes the old one.
    let (status, second) = env.mint(&device).await;
    assert_eq!(status, 201, "{second}");
    let (_, second_secret) = token(&second);
    let revoke = format!("/v1/hub/device/tokens/{first_id}");
    assert_eq!(env.delete(&revoke, &device).await, 204);
    assert_eq!(env.whoami(&first_secret).await, 401);
    assert_eq!(env.whoami(&second_secret).await, 200);

    // Losing the role refuses the next renewal; the current token keeps working until it ends.
    env.drop_role(ALICE);
    let (status, refused) = env.mint(&device).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("insufficient_role"))
    );
    assert_eq!(env.whoami(&second_secret).await, 200);

    // A role below the rule is refused too: read is not enough on a non-private repository.
    env.set_repo(REPO, REPO_ID, "public");
    env.role(ALICE, "read", "read", 20);
    let (status, refused) = env.mint(&device).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("insufficient_role"))
    );
    env.role(ALICE, "read", "triage", 30);
    assert_eq!(env.mint(&device).await.0, 201);

    // The admin sees the refusal once, counted, with no secret.
    let (admin_session, _, _) = env.sign_in(ADMIN, "admin's laptop").await;
    let (status, list) = env
        .get("/v1/hub/admin/sign-in-refusals", &admin_session)
        .await;
    assert_eq!(status, 200, "{list}");
    let rows = list["refusals"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{list}");
    assert_eq!(
        (
            &rows[0]["login"],
            &rows[0]["reason"],
            &rows[0]["count"],
            &rows[0]["via"]
        ),
        (
            &json!("alice"),
            &json!("insufficient_role"),
            &json!(2),
            &json!("device_mint")
        )
    );
    assert!(env.kinds().contains(&"device_created".to_owned()));
}

#[tokio::test]
async fn github_renewal_rechecks_the_role_with_the_hub_credential() {
    renewal_rechecks_the_role("github").await;
}

#[tokio::test]
async fn gitlab_renewal_rechecks_the_role_with_the_hub_credential() {
    renewal_rechecks_the_role("gitlab").await;
}

#[tokio::test]
async fn minting_fails_closed_when_the_hub_cannot_check() {
    let env = Env::start("github").await;
    let (_, device, _) = env.sign_in(ALICE, "laptop").await;
    env.fake.lock().unwrap().hub_reads_roles = false;
    let (status, refused) = env.mint(&device).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("access_check_unavailable"))
    );
    env.fake.lock().unwrap().hub_reads_roles = true;
    let (status, refused) = env
        .mint_at(
            &env.hub,
            &device,
            json!({"name": "client-laptop", "repository": "acme/other"}),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_connected"))
    );
}

#[tokio::test]
async fn credentials_are_not_sessions_agent_tokens_or_admin() {
    let env = Env::start("github").await;
    let (session, device, _) = env.sign_in(ADMIN, "admin's laptop").await;
    let (_, minted) = env.mint(&device).await;
    let (_, agent) = token(&minted);
    for path in [
        "/v1/hub/session",
        "/v1/hub/tokens",
        "/v1/hub/users",
        "/v1/hub/connections",
        "/v1/hub/admin/devices",
        "/v1/hub/admin/repositories",
        "/v1/hub/admin/sign-in-refusals",
        "/v1/hub/whoami",
    ] {
        assert_eq!(env.get(path, &device).await.0, 401, "{path}");
    }
    for credential in [&session, &agent] {
        assert_eq!(env.get("/v1/hub/device", credential).await.0, 401);
        let (status, _) = env.mint(credential).await;
        assert_eq!(status, 401);
    }
    let (status, me) = env.get("/v1/hub/device", &device).await;
    assert_eq!((status, &me["user"]["login"]), (200, &json!("admin")));
    assert_eq!(env.get("/v1/hub/admin/devices", &session).await.0, 200);
}

#[tokio::test]
async fn revoking_a_computer_stops_it_and_its_tokens() {
    let env = Env::start("github").await;
    let (session, laptop, laptop_id) = env.sign_in(ALICE, "laptop").await;
    let (_, desktop, desktop_id) = env.sign_in(ALICE, "laptop").await;
    // The same name is two computers: signing in again did not revoke the first.
    let (status, list) = env.get("/v1/hub/devices", &laptop).await;
    assert_eq!(status, 200);
    let devices = list["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2, "{list}");
    assert!(devices.iter().all(|d| d["revoked_at"].is_null()));
    assert_eq!(
        devices
            .iter()
            .filter(|d| d["current"] == json!(true))
            .count(),
        1
    );
    let (_, minted) = env.mint(&laptop).await;
    let (_, agent) = token(&minted);
    let (_, list) = env.get("/v1/hub/devices", &session).await;
    let listed = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == json!(laptop_id))
        .unwrap()
        .clone();
    assert_eq!(listed["repositories"], json!([REPO]));

    // A computer cannot revoke itself by id, nor someone else's computer.
    assert_eq!(
        env.delete(&format!("/v1/hub/devices/{laptop_id}"), &laptop)
            .await,
        400
    );
    let (_, _, bobs) = env.sign_in(BOB, "bob's").await;
    assert_eq!(
        env.delete(&format!("/v1/hub/devices/{bobs}"), &laptop)
            .await,
        404
    );
    // From the desktop, alice removes the lost laptop: its credential and tokens stop.
    assert_eq!(
        env.delete(&format!("/v1/hub/devices/{laptop_id}"), &desktop)
            .await,
        204
    );
    assert_eq!(env.get("/v1/hub/device", &laptop).await.0, 401);
    assert_eq!(env.whoami(&agent).await, 401);
    let (status, refused) = env.mint(&laptop).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (401, Some("sign_in_required"))
    );
    assert_eq!(
        env.delete(&format!("/v1/hub/devices/{laptop_id}"), &desktop)
            .await,
        404
    );
    // An administrator's session revokes anyone's; a computer signs itself out.
    let (admin, _, _) = env.sign_in(ADMIN, "admin's").await;
    assert_eq!(
        env.delete(&format!("/v1/hub/devices/{bobs}"), &admin).await,
        204
    );
    assert_eq!(env.delete("/v1/hub/device", &desktop).await, 204);
    assert_eq!(env.get("/v1/hub/device", &desktop).await.0, 401);
    let revoked = env
        .store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "device_revoked")
        .map(|event| {
            (
                event.subject,
                serde_json::from_str::<Value>(&event.data).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(revoked[0], (laptop_id, json!({"by": desktop_id})));
    assert_eq!(revoked.len(), 3);
}

#[tokio::test]
async fn signing_in_again_replaces_only_the_presented_computer() {
    let env = Env::start("github").await;
    let (_, old, old_id) = env.sign_in(ALICE, "laptop").await;
    let (_, minted) = env.mint(&old).await;
    let (_, agent) = token(&minted);
    let (status, signed) = env
        .sign_in_at(
            &env.hub,
            ALICE,
            Some(json!({"name": "laptop", "replaces": old})),
        )
        .await;
    assert_eq!(status, 200, "{signed}");
    let new = signed["device"]["secret"].as_str().unwrap();
    assert_ne!(signed["device"]["id"], json!(old_id));
    assert_eq!(env.get("/v1/hub/device", &old).await.0, 401);
    assert_eq!(env.whoami(&agent).await, 401);
    assert_eq!(env.get("/v1/hub/device", new).await.0, 200);
    // Someone else's credential replaces nothing.
    let (status, signed) = env
        .sign_in_at(
            &env.hub,
            BOB,
            Some(json!({"name": "bob's", "replaces": new})),
        )
        .await;
    assert_eq!(status, 200, "{signed}");
    assert_eq!(env.get("/v1/hub/device", new).await.0, 200);
    // A sign-in without a device request returns no credential; one without a name gets a
    // default that is no host name.
    let (_, signed) = env.sign_in_at(&env.hub, ALICE, None).await;
    assert!(signed.get("device").is_none(), "{signed}");
    let (_, signed) = env.sign_in_at(&env.hub, ALICE, Some(json!({}))).await;
    assert_eq!(signed["device"]["name"], "alice's computer");
}

#[tokio::test]
async fn computers_survive_a_restart_and_follow_the_allow_list_and_disabling() {
    let env = Env::start("github").await;
    let (_, device, _) = env.sign_in(ALICE, "laptop").await;
    let (_, bob_device, _) = env.sign_in(BOB, "bob's").await;
    assert_eq!(env.mint(&device).await.0, 201);

    // A restarted Hub that drops alice from its allow list refuses her next renewal.
    let restarted = env.serve("github:#8").await;
    let (status, refused) = env
        .mint_at(
            &restarted,
            &device,
            json!({"name": "client-laptop", "repository": REPO}),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_allowed"))
    );
    let body = json!({"name": "client-laptop", "repository": REPO});
    assert_eq!(
        env.mint_at(&restarted, &bob_device, body.clone()).await.0,
        201
    );

    // Disabling stops the credential with its own reason.
    let alice = env.store.list_users().unwrap();
    let alice = alice.iter().find(|u| u.login == "alice").unwrap();
    env.store.set_user_disabled(&alice.id, true).unwrap();
    let (status, refused) = env.mint(&device).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("user_disabled"))
    );
    env.store.set_user_disabled(&alice.id, false).unwrap();
    assert_eq!(env.mint(&device).await.0, 201);
}

#[tokio::test]
async fn people_with_a_session_enroll_and_admins_see_connected_repositories() {
    let env = Env::start("github").await;
    let (status, signed) = env.sign_in_at(&env.hub, ALICE, None).await;
    assert_eq!(status, 200);
    let session = signed["session"].as_str().unwrap();
    let (status, enrolled) = env
        .call(
            &env.hub,
            Method::POST,
            "/v1/hub/devices",
            Some(session),
            Some(json!({"name": "alice's MacBook Pro"})),
        )
        .await;
    assert_eq!(status, 201, "{enrolled}");
    let device = enrolled["device"]["secret"].as_str().unwrap();
    assert_eq!(env.mint(device).await.0, 201);
    let (status, _) = env
        .call(
            &env.hub,
            Method::POST,
            "/v1/hub/device/name",
            Some(device),
            Some(json!({"name": "work laptop"})),
        )
        .await;
    assert_eq!(status, 204);
    let (_, me) = env.get("/v1/hub/device", device).await;
    assert_eq!(me["device"]["name"], "work laptop");

    let (admin, _, _) = env.sign_in(ADMIN, "admin's").await;
    let (status, usage) = env.get("/v1/hub/admin/repositories", &admin).await;
    assert_eq!(status, 200, "{usage}");
    let repo = usage["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["full_name"] == json!(REPO))
        .unwrap()
        .clone();
    assert_eq!(
        (&repo["devices"], &repo["agent_tokens"]),
        (&json!(1), &json!(1))
    );
    assert_eq!(repo["provider"], "github");
    let (_, devices) = env.get("/v1/hub/admin/devices", &admin).await;
    assert!(
        devices["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["owner"]["login"] == json!("alice"))
    );
    assert_eq!(env.get("/v1/hub/admin/devices", session).await.0, 403);
}

#[tokio::test]
async fn a_refused_sign_in_is_recorded_once_per_person_and_reason() {
    let env = Env::start("github").await;
    env.drop_role(BOB);
    for _ in 0..3 {
        let (status, refused) = env.sign_in_at(&env.hub, BOB, None).await;
        assert_eq!(
            (status, refused["error"].as_str()),
            (403, Some("not_allowed"))
        );
    }
    let (admin, _, _) = env.sign_in(ADMIN, "admin's").await;
    let (_, list) = env.get("/v1/hub/admin/sign-in-refusals", &admin).await;
    assert_eq!(
        list["refusals"],
        json!([{"at": list["refusals"][0]["at"], "count": 3, "provider": "github",
            "host": env.base.trim_start_matches("http://"), "login": "bob",
            "forge_user_id": BOB, "reason": "no_repository_role", "via": "sign_in"}])
    );
    // Nothing else was stored for bob.
    assert!(
        env.store
            .list_users()
            .unwrap()
            .iter()
            .all(|u| u.login != "bob")
    );
}

// ---- the client: sign in once, agents connect and renew ----

/// One computer: its own config and home directories and a checkout of `repo` whose origin is
/// on the fake forge's host.
struct Machine {
    config: TempDir,
    home: TempDir,
    checkout: TempDir,
}

impl Machine {
    fn new(repo: &str) -> Self {
        let checkout = TempDir::new().unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "remote",
                "add",
                "origin",
                &format!("http://127.0.0.1/{repo}.git"),
            ],
        ] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(checkout.path())
                .status()
                .unwrap();
            assert!(status.success());
        }
        Self {
            config: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
            checkout,
        }
    }

    fn root(&self) -> std::path::PathBuf {
        self.checkout.path().canonicalize().unwrap()
    }

    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command
            .args(args)
            .env("XDG_CONFIG_HOME", self.config.path())
            // Tests never open a real browser: sign-in uses the code flow, or prints its URL.
            .env("AGENTSCM_NO_BROWSER", "1")
            .env("HOME", self.home.path())
            .env("NO_PROXY", "127.0.0.1")
            .env_remove("AGENTSCM_HUB_URL")
            .env_remove("AGENTSCM_HUB_TOKEN")
            .stdin(std::process::Stdio::null());
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

    /// One SessionStart hook in the checkout; returns the hook's JSON reply.
    fn session_start(&self, session: &str) -> Value {
        use std::io::Write;
        let mut child = self
            .command(&["--no-telemetry", "hook", "claude", "--client-connection"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let payload = json!({"hook_event_name": "SessionStart", "session_id": session,
            "cwd": self.root(), "source": "startup"});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn file(&self, name: &str) -> Value {
        let path = self.config.path().join("agentscm").join(name);
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn connection(&self) -> Value {
        self.file("connections.json")["connections"][0].clone()
    }

    /// Makes the saved token look about to expire, so the next renewal is due.
    fn age_token(&self) {
        let path = self.config.path().join("agentscm/connections.json");
        let mut saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        saved["connections"][0]["owned_token_expires_at"] = json!(unix_now() + 60);
        std::fs::write(path, saved.to_string()).unwrap();
    }

    fn token(&self) -> String {
        let path = self.connection()["token_file"].as_str().unwrap().to_owned();
        std::fs::read_to_string(path).unwrap().trim().to_owned()
    }

    fn mcp(&self) -> Mcp {
        let root = self.root();
        let mut child = self
            .command(&[
                "--no-telemetry",
                "client",
                "mcp",
                "--root",
                root.to_str().unwrap(),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        Mcp {
            child,
            stdin,
            stdout,
            next: 0,
        }
    }

    fn renew(&self) -> Value {
        let root = self.root();
        let (ok, stdout, stderr) = self.run(&[
            "--no-telemetry",
            "client",
            "renew",
            "--root",
            root.to_str().unwrap(),
        ]);
        assert!(ok, "{stderr}");
        serde_json::from_str(stdout.lines().last().unwrap()).unwrap()
    }
}

/// A running `client mcp` server.
struct Mcp {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    next: u32,
}

impl Mcp {
    /// One `tools/list` request; the reply line.
    fn tools_list(&mut self) -> String {
        use std::io::{BufRead, Write};
        self.next += 1;
        let request = json!({"jsonrpc": "2.0", "id": self.next, "method": "tools/list"});
        writeln!(self.stdin, "{request}").unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        line
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn context(reply: &Value) -> String {
    reply["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_connects_itself_renews_and_stops_when_access_ends() {
    let env = Env::start("github").await;
    env.fake.lock().unwrap().approve = ALICE;
    let machine = Machine::new(REPO);
    let hub = env.hub.clone();
    let (ok, _, stderr) = machine.run(&["hub", "login", "--hub", &hub, "--provider", "github"]);
    assert!(ok, "{stderr}");
    // Before auto-connect is on, an agent in the checkout is only told how to connect.
    let reply = machine.session_start("s0");
    assert!(context(&reply).contains("no client connection"), "{reply}");
    let (ok, _, stderr) = machine.run(&["hub", "auto-connect", "on"]);
    assert!(ok, "{stderr}");

    // The next session connects the checkout and says so in one line, writing no file in it.
    let reply = machine.session_start("s1");
    assert!(
        context(&reply).starts_with(&format!("AgentSCM: coordinating on {REPO}.")),
        "{reply}"
    );
    let connection = machine.connection();
    assert_eq!(connection["repo"], REPO);
    // The agent is recorded, so removing another checkout keeps the hooks this one uses.
    assert_eq!(connection["agents"], json!(["claude"]));
    let entries: Vec<_> = std::fs::read_dir(machine.root())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, [std::ffi::OsString::from(".git")]);
    let exclude = machine.root().join(".git/info/exclude");
    assert!(
        !std::fs::read_to_string(exclude)
            .unwrap_or_default()
            .contains("agentscm")
    );
    assert!(env.kinds().contains(&"auto_connected".to_owned()));
    let first = machine.token();
    assert_eq!(env.whoami(&first).await, 200);

    // Who this computer is, and a connection check that needs no running agent.
    let (ok, whoami, stderr) = machine.run(&["hub", "whoami"]);
    assert!(ok, "{stderr}");
    assert!(whoami.contains("login: alice"), "{whoami}");
    assert!(whoami.contains("auto-connect on"), "{whoami}");
    assert!(whoami.contains(REPO), "{whoami}");
    let root = machine.root();
    let (ok, doctor, stderr) = machine.run(&[
        "--no-telemetry",
        "client",
        "doctor",
        "--agent",
        "claude",
        "--root",
        root.to_str().unwrap(),
    ]);
    assert!(ok, "{stderr}");
    assert!(doctor.contains("Connection verified"), "{doctor}");

    // Renewal swaps the token before it ends and revokes the old one; a running MCP server
    // keeps working across it.
    let mut mcp = machine.mcp();
    assert!(mcp.tools_list().contains("\"result\""));
    assert_eq!(machine.renew()["result"], "not_due");
    machine.age_token();
    assert_eq!(machine.renew()["result"], "renewed");
    let after = mcp.tools_list();
    assert!(after.contains("\"result\""), "{after}");
    let second = machine.token();
    assert_ne!(first, second);
    assert_eq!(env.whoami(&first).await, 401);
    assert_eq!(env.whoami(&second).await, 200);

    // Losing the role refuses the next renewal; the agent is told why and what to do.
    env.drop_role(ALICE);
    machine.age_token();
    let refused = machine.renew();
    assert_eq!(refused["result"], "refused", "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("ask a repository owner for")
    );
    let reply = machine.session_start("s2");
    assert!(
        context(&reply).starts_with("AgentSCM: alice has no access on private repository"),
        "{reply}"
    );

    // Revoking the computer signs it out: renewal says so, and nothing re-enrolls it.
    env.role(ALICE, "write", "write", 30);
    let device = machine.file("hub-sessions.json")["sessions"][0]["device"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        env.store
            .revoke_device(&device, None, "usr_admin", unix_now())
            .unwrap()
    );
    let refused = machine.renew();
    assert_eq!(refused["result"], "refused", "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("isn't signed in to this Hub any more")
    );
    assert!(
        machine.file("hub-sessions.json")["sessions"][0]
            .get("device")
            .is_none()
    );
    assert_eq!(machine.renew()["result"], "refused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_connect_is_refused_without_the_role_and_skips_unconnected_repositories() {
    let env = Env::start("github").await;
    env.fake.lock().unwrap().approve = BOB;
    env.set_repo(REPO, REPO_ID, "public");
    // bob can sign in through another repository he reads.
    env.connect_repo("acme/docs", 4343, BOB);
    let machine = Machine::new(REPO);
    let hub = env.hub.clone();
    let (ok, _, stderr) = machine.run(&["hub", "login", "--hub", &hub, "--provider", "github"]);
    assert!(ok, "{stderr}");
    machine.run(&["hub", "auto-connect", "on"]);
    // bob only reads a public repository: refused, with the reason, and nothing saved.
    let reply = machine.session_start("s1");
    assert!(
        context(&reply).contains("bob has Read on public repository acme/widgets"),
        "{reply}"
    );
    assert!(
        !machine
            .config
            .path()
            .join("agentscm/connections.json")
            .exists()
    );

    // A repository the Hub does not have is not reported as a refusal.
    let other = Machine::new("acme/elsewhere");
    let config = machine.config.path().to_owned();
    let mut command = other.command(&["--no-telemetry", "client", "auto-connect", "--root"]);
    let output = command
        .arg(other.root())
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["result"], "not_connected", "{result}");
}

#[tokio::test]
async fn refusals_explain_themselves_and_name_a_connected_upstream() {
    let env = Env::start("github").await;
    env.role(EMU, "write", "write", 30);
    let (_, device, _) = env.sign_in(EMU, "carol's").await;
    // An Enterprise Managed User login (with `_`) is checked like any other.
    assert_eq!(env.mint(&device).await.0, 201);
    // A fork of a connected repository is not connected, and the refusal names the upstream.
    env.fake
        .lock()
        .unwrap()
        .forks
        .insert("carol_acme/widgets".into(), REPO.into());
    let (status, refused) = env
        .mint_at(
            &env.hub,
            &device,
            json!({"name": "client-laptop", "repository": "carol_acme/widgets"}),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_connected"))
    );
    assert_eq!(refused["fork_of"], REPO);
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains(&format!("fork of {REPO}")),
        "{refused}"
    );
    // The hint is asked only for owner/name and only of a connected repository with the same
    // name, through that repository's own connection.
    env.fake
        .lock()
        .unwrap()
        .forks
        .insert("carol_acme/renamed".into(), REPO.into());
    for unasked in ["carol_acme/renamed", "group/sub/widgets"] {
        let (_, refused) = env
            .mint_at(
                &env.hub,
                &device,
                json!({"name": "client-laptop", "repository": unasked}),
            )
            .await;
        assert_eq!(refused["error"], "not_connected", "{unasked}");
        assert!(refused.get("fork_of").is_none(), "{unasked}: {refused}");
    }
    // A plain unconnected repository says so, with its fix.
    let (_, refused) = env
        .mint_at(
            &env.hub,
            &device,
            json!({"name": "client-laptop", "repository": "acme/unknown"}),
        )
        .await;
    assert_eq!(
        refused["message"],
        "acme/unknown isn't connected to this Hub yet; ask a Hub admin to connect it"
    );
    // A too-low role uses the same refusal as session minting, with its fields.
    env.role(EMU, "read", "read", 20);
    env.set_repo(REPO, REPO_ID, "public");
    let (status, refused) = env.mint(&device).await;
    assert_eq!(status, 403);
    assert_eq!(
        (
            &refused["error"],
            &refused["has"],
            &refused["needs"],
            &refused["visibility"]
        ),
        (
            &json!("insufficient_role"),
            &json!("read"),
            &json!("triage"),
            &json!("public")
        )
    );
    assert_eq!(
        refused["message"],
        format!(
            "carol_acme has Read on public repository {REPO}; ask a repository owner for Triage or higher"
        )
    );
    // A failed sign-in step says which one.
    let (status, failed) = env
        .call(
            &env.hub,
            Method::POST,
            "/v1/hub/login/device/poll",
            None,
            Some(json!({"handle": "0".repeat(64)})),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(
        failed["message"],
        "This sign-in code expired; start signing in again"
    );
}

#[tokio::test]
async fn a_revoked_computer_cannot_be_enrolled_again_from_its_session() {
    let env = Env::start("github").await;
    let (session, device, id) = env.sign_in(ALICE, "laptop").await;
    // The session that came with a computer cannot add another.
    let (status, refused) = env
        .call(
            &env.hub,
            Method::POST,
            "/v1/hub/devices",
            Some(&session),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("device_revoked"))
    );
    // Revoking the computer ends that session too.
    assert_eq!(
        env.delete(&format!("/v1/hub/devices/{id}"), &session).await,
        204
    );
    assert_eq!(env.get("/v1/hub/session", &session).await.0, 401);
    assert_eq!(env.get("/v1/hub/device", &device).await.0, 401);
    // Live computers are listed before revoked ones.
    let (_, _, live) = env.sign_in(ALICE, "desktop").await;
    let (_, list) = env
        .get(
            "/v1/hub/admin/devices",
            &env.sign_in(ADMIN, "admin's").await.0,
        )
        .await;
    let alice: Vec<_> = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["owner"]["login"] == json!("alice"))
        .map(|d| d["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(alice, [live, id]);
}

#[tokio::test]
async fn the_login_reports_the_browser_only_providers() {
    let env = Env::start("github").await;
    let (status, providers) = env
        .call(&env.hub, Method::GET, "/v1/hub/login", None, None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(providers["browser_only"], json!([]));
}

/// A Hub from before computer credentials: its sign-in refuses an unknown `device` field.
async fn older_hub() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/v1/hub/login/github/device",
            post(|| async {
                Json(
                    json!({"handle": "h", "user_code": "ABCD", "verification_uri":
                    "http://127.0.0.1/device", "interval": 1, "expires_in": 60}),
                )
            }),
        )
        .route(
            "/v1/hub/login/device/poll",
            post(|Json(body): Json<Value>| async move {
                if body.get("device").is_some() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "invalid_request"})),
                    );
                }
                (
                    StatusCode::OK,
                    Json(json!({"status": "signed_in", "session": "s".repeat(64),
                        "expires_at": unix_now() + 3600,
                        "user": {"provider": "github", "login": "alice"}})),
                )
            }),
        )
        .route(
            "/v1/hub/login",
            get(|| async { Json(json!({"providers": ["github"]})) }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_still_signs_in_to_an_older_hub() {
    let hub = older_hub().await;
    let machine = Machine::new(REPO);
    let (ok, _, stderr) = machine.run(&[
        "hub",
        "login",
        "--hub",
        &hub,
        "--provider",
        "github",
        "--device",
    ]);
    assert!(ok, "{stderr}");
    let saved = machine.file("hub-sessions.json");
    assert!(saved["sessions"][0].get("device").is_none(), "{saved}");
}

#[tokio::test]
async fn a_session_enrolls_one_computer_and_revoking_it_ends_the_session() {
    let env = Env::start("github").await;
    let (status, signed) = env.sign_in_at(&env.hub, ALICE, None).await;
    assert_eq!(status, 200);
    let session = signed["session"].as_str().unwrap().to_owned();
    let enroll = || {
        env.call(
            &env.hub,
            Method::POST,
            "/v1/hub/devices",
            Some(&session),
            Some(json!({})),
        )
    };
    let (status, enrolled) = enroll().await;
    assert_eq!(status, 201, "{enrolled}");
    let id = enrolled["device"]["id"].as_str().unwrap().to_owned();
    // The same session cannot add a second computer.
    let (status, refused) = enroll().await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("device_revoked"))
    );
    // Revoking the enrolled computer ends the session that enrolled it, so a copied session
    // file cannot enroll a replacement.
    assert_eq!(
        env.delete(&format!("/v1/hub/devices/{id}"), &session).await,
        204
    );
    assert_eq!(env.get("/v1/hub/session", &session).await.0, 401);
    assert_eq!(enroll().await.0, 401);
}

/// A browser that keeps no cookies of its own and follows no redirects.
fn browser() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn set_cookie(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| {
            value
                .strip_prefix(&format!("{name}="))?
                .split(';')
                .next()
                .map(str::to_owned)
        })
}

/// Signs `user` in to the Hub's web pages; returns the session cookie's value.
async fn web_sign_in(env: &Env, user: i64) -> String {
    env.fake.lock().unwrap().approve = user;
    let http = browser();
    let start = http
        .get(format!("{}/v1/hub/login/github?web=1", env.hub))
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), 302);
    let login = set_cookie(&start, "__Host-agentscm_login").unwrap();
    let authorize = start.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    let forge = http.get(authorize).send().await.unwrap();
    let callback = forge.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    let done = http
        .get(callback)
        .header(header::COOKIE, format!("__Host-agentscm_login={login}"))
        .send()
        .await
        .unwrap();
    assert_eq!(done.status(), 302);
    assert_eq!(done.headers()[header::LOCATION], "/ui/#home");
    let all: Vec<_> = done
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect();
    let session = all
        .iter()
        .find(|value| value.starts_with("__Host-agentscm_session="))
        .unwrap();
    for attribute in [
        "HttpOnly",
        "Secure",
        "SameSite=Strict",
        "Path=/",
        "Max-Age=604800",
    ] {
        assert!(session.contains(attribute), "{session}");
    }
    set_cookie(&done, "__Host-agentscm_session").unwrap()
}

async fn with_cookie(
    hub: &str,
    method: Method,
    path: &str,
    cookie: &str,
    csrf: Option<&str>,
) -> (u16, Value) {
    let mut request = browser()
        .request(method, format!("{hub}{path}"))
        .header(header::COOKIE, format!("__Host-agentscm_session={cookie}"));
    if let Some(csrf) = csrf {
        request = request.header("x-csrf-token", csrf);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_web_sign_in_survives_a_restart_and_needs_csrf_to_change_anything() {
    let env = Env::start("github").await;
    let cookie = web_sign_in(&env, ALICE).await;
    let (status, me) = with_cookie(&env.hub, Method::GET, "/v1/hub/session", &cookie, None).await;
    assert_eq!(status, 200, "{me}");
    assert_eq!(
        (&me["user"]["login"], &me["admin"]),
        (&json!("alice"), &json!(false))
    );
    let csrf = me["csrf_token"].as_str().unwrap().to_owned();

    // A restarted Hub process still knows the sign-in: it lives in the database.
    let restarted = env.serve("").await;
    let (status, _) = with_cookie(&restarted, Method::GET, "/v1/hub/session", &cookie, None).await;
    assert_eq!(status, 200);

    // The person's computers are listed; a revoke needs the CSRF token, not the cookie alone.
    let (_, _, device) = env.sign_in(ALICE, "laptop").await;
    let (status, list) = with_cookie(&env.hub, Method::GET, "/v1/hub/devices", &cookie, None).await;
    assert_eq!(status, 200);
    assert_eq!(list["devices"][0]["id"], json!(device));
    let path = format!("/v1/hub/devices/{device}");
    let (status, refused) = with_cookie(&env.hub, Method::DELETE, &path, &cookie, None).await;
    assert_eq!((status, refused["error"].as_str()), (403, Some("csrf")));
    let wrong = "0".repeat(64);
    assert_eq!(
        with_cookie(&env.hub, Method::DELETE, &path, &cookie, Some(&wrong))
            .await
            .0,
        403
    );
    assert_eq!(
        with_cookie(&env.hub, Method::DELETE, &path, &cookie, Some(&csrf))
            .await
            .0,
        204
    );

    // A web sign-in makes no agent tokens and is no administrator unless listed.
    let minted = browser()
        .post(format!("{}/v1/hub/tokens", env.hub))
        .header(header::COOKIE, format!("__Host-agentscm_session={cookie}"))
        .header("x-csrf-token", &csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"name": "web", "repository": REPO, "ttl_seconds": 600}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(minted.status(), 403);
    // Nor a computer credential, which would outlive the page's sign-in.
    let enrolled = browser()
        .post(format!("{}/v1/hub/devices", env.hub))
        .header(header::COOKIE, format!("__Host-agentscm_session={cookie}"))
        .header("x-csrf-token", &csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(enrolled.status(), 403);
    let (status, _) = with_cookie(
        &env.hub,
        Method::GET,
        "/v1/hub/admin/devices",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, 403);
    let admin = web_sign_in(&env, ADMIN).await;
    let (status, _) =
        with_cookie(&env.hub, Method::GET, "/v1/hub/admin/devices", &admin, None).await;
    assert_eq!(status, 200);

    // Signing out needs the CSRF token and ends the sign-in.
    let hub = &env.hub;
    let session = |cookie: String| async move {
        with_cookie(hub, Method::GET, "/v1/hub/session", &cookie, None).await
    };
    assert_eq!(
        with_cookie(&env.hub, Method::DELETE, "/v1/hub/session", &cookie, None)
            .await
            .0,
        403
    );
    assert_eq!(
        with_cookie(
            &env.hub,
            Method::DELETE,
            "/v1/hub/session",
            &cookie,
            Some(&csrf)
        )
        .await
        .0,
        204
    );
    assert_eq!(session(cookie).await.0, 401);

    // Disabling the person ends the sign-in, and enabling them again does not bring it back.
    let cookie = web_sign_in(&env, ALICE).await;
    let users = env.store.list_users().unwrap();
    let alice = users.iter().find(|u| u.login == "alice").unwrap();
    env.store.set_user_disabled(&alice.id, true).unwrap();
    assert_eq!(session(cookie.clone()).await.0, 401);
    env.store.set_user_disabled(&alice.id, false).unwrap();
    assert_eq!(session(cookie).await.0, 401);

    // A Hub whose allow list no longer has the person refuses the sign-in.
    let cookie = web_sign_in(&env, ALICE).await;
    let restarted = env.serve("github:#8").await;
    let (status, refused) =
        with_cookie(&restarted, Method::GET, "/v1/hub/session", &cookie, None).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_allowed"))
    );

    // An unknown cookie, or two of them, is no sign-in.
    assert_eq!(session("a".repeat(64)).await.0, 401);
    assert_eq!(
        session(format!("{cookie}; __Host-agentscm_session={cookie}"))
            .await
            .0,
        401
    );
    assert_eq!(session(cookie).await.0, 200);
}
