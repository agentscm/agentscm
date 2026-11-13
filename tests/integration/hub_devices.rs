//! Computer credentials against an in-process fake forge: sign-in, renewal with the role
//! re-checked through the Hub's own connection credential, refusals and revocation.
//! No live accounts.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agentscm::backend::LocalBackend;
use agentscm::hub::access::{Access, Grant, Level};
use agentscm::hub::handoff::Via;
use agentscm::hub::intent::NewIntent;
use agentscm::hub::login::{Login, LoginConfig};
use agentscm::hub::promise::{
    PromiseCreateV1, PromiseForgeAdmissionV1, PromiseRepositoryAdmissionV1, admissions_digest,
};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token, unix_now};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::workspace::{Identity, Workspace, WorkspaceConfig, WorkspaceForgeRepositoryV1};
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
    workspace: Option<Arc<Workspace>>,
    /// `AGENTSCM_HUB_APPS_FILE` for the next `serve`.
    apps: Option<String>,
    /// An embedder's access rules for the next `serve`.
    access: Option<Arc<dyn Access>>,
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
            workspace: None,
            apps: None,
            access: None,
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
        if let Some(apps) = &self.apps {
            vars.insert("AGENTSCM_HUB_APPS_FILE".into(), apps.clone());
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
        let state = match &self.workspace {
            Some(workspace) => state.with_workspace(workspace.clone()),
            None => state,
        };
        let state = match &self.access {
            Some(access) => state.with_access(access.clone()),
            None => state,
        };
        sync::start(&state).await.unwrap();
        state.revoke_dropped_apps().unwrap();
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

fn device_promise_workspace(env: &Env, forge_repository_id: i64) -> Arc<Workspace> {
    Arc::new(
        Workspace::open(
            WorkspaceConfig {
                workspace_id: "device-promise-workspace".into(),
                name: "Device promise workspace".into(),
                database: env.dir.path().join("device-promise-workspace.sqlite"),
                initial_owner: Identity {
                    provider: "github".into(),
                    host: env.base.trim_start_matches("http://").into(),
                    user_id: ALICE,
                },
                forge_repositories: vec![WorkspaceForgeRepositoryV1 {
                    repository_id: REPO.into(),
                    provider: "github".into(),
                    host: env.base.trim_start_matches("http://").into(),
                    forge_repository_id,
                }],
            },
            env.store.clone(),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn device_minted_token_obeys_promise_scope_and_revocation() {
    let mut env = Env::start("github").await;
    let (_, device, _) = env.sign_in(ALICE, "promise laptop").await;
    env.workspace = Some(device_promise_workspace(&env, REPO_ID));
    env.hub = env.serve("").await;
    let (status, minted) = env.mint(&device).await;
    assert_eq!(status, 201, "{minted}");
    assert_eq!(minted["repository"], REPO);
    assert_eq!(minted["repository_id"], REPO_ID);
    let (token_id, secret) = token(&minted);

    let owner = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == ALICE)
        .unwrap();
    let admission = PromiseRepositoryAdmissionV1 {
        repository_id: REPO.into(),
        forge: Some(PromiseForgeAdmissionV1 {
            provider: "github".into(),
            host: env.base.trim_start_matches("http://").into(),
            forge_repository_id: REPO_ID,
        }),
        native_repository_ids: vec![],
    };
    let create = PromiseCreateV1 {
        operation_id: "device_promise_create".into(),
        expected_admissions_digest: admissions_digest(
            "device-promise-workspace",
            std::slice::from_ref(&admission),
        )
        .unwrap(),
        definition: serde_json::from_value(json!({
            "original_request":"Keep customer settings.",
            "commitment":"Existing settings persist.",
            "criteria":[{"id":"settings","text":"Settings persist.","evidence_requirements":[]}],
            "constraints":[],
            "repositories":[{"repository_id":REPO,"relevance":"Settings source."}]
        }))
        .unwrap(),
    };
    let created = env
        .store
        .promise_create(
            "device-promise-workspace",
            &owner.id,
            &create,
            &[admission],
            unix_now(),
        )
        .unwrap();
    let show = format!(
        "/v1/hub/workspace/promises/{}",
        created.reference.promise_id
    );
    let proposal = json!({"kind":"proposal","operation_id":"device_proposal",
        "repository_id":REPO,"original_request":"Keep settings.",
        "commitment":"Settings persist.",
        "criteria":[{"id":"settings","text":"Settings persist.","evidence_requirements":[]}],
        "constraints":[]});
    let report = json!({"kind":"report","operation_id":"device_report",
        "promise_id":created.reference.promise_id,
        "promise_version":created.reference.version,
        "promise_digest":created.reference.digest,
        "repository_id":REPO,"summary":"Caller-observed concern; not verified evidence."});
    assert_eq!(env.get(&show, &secret).await.0, 200);
    for body in [&proposal, &report] {
        assert_eq!(
            env.call(
                &env.hub,
                Method::POST,
                "/v1/hub/workspace/promises/inputs",
                Some(&secret),
                Some(body.clone()),
            )
            .await
            .0,
            200
        );
    }
    let mut wrong_key = proposal.clone();
    wrong_key["operation_id"] = json!("device_wrong_key");
    wrong_key["repository_id"] = json!("acme/other");
    assert_eq!(
        env.call(
            &env.hub,
            Method::POST,
            "/v1/hub/workspace/promises/inputs",
            Some(&secret),
            Some(wrong_key),
        )
        .await
        .0,
        404
    );

    env.connect_repo("acme/rebound", REPO_ID + 1, ALICE);
    env.workspace = Some(device_promise_workspace(&env, REPO_ID + 1));
    env.hub = env.serve("").await;
    assert_eq!(env.get(&show, &secret).await.0, 404);
    for body in [&proposal, &report] {
        assert_eq!(
            env.call(
                &env.hub,
                Method::POST,
                "/v1/hub/workspace/promises/inputs",
                Some(&secret),
                Some(body.clone()),
            )
            .await
            .0,
            404
        );
    }

    env.workspace = Some(device_promise_workspace(&env, REPO_ID));
    env.hub = env.serve("").await;
    assert_eq!(env.get(&show, &secret).await.0, 200);
    let database = env.dir.path().join("hub.sqlite");
    let input_count = || {
        rusqlite::Connection::open(&database)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM hub_promise_inputs", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
    };
    let before_revoke = input_count();
    assert_eq!(
        env.delete(&format!("/v1/hub/device/tokens/{token_id}"), &device)
            .await,
        204
    );
    assert_eq!(env.get(&show, &secret).await.0, 401);
    for body in [&proposal, &report] {
        assert_eq!(
            env.call(
                &env.hub,
                Method::POST,
                "/v1/hub/workspace/promises/inputs",
                Some(&secret),
                Some(body.clone()),
            )
            .await
            .0,
            401
        );
    }
    assert_eq!(input_count(), before_revoke);
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

/// A checkout of `repo` whose origin is on the fake forge's host.
fn checkout_of(repo: &str) -> TempDir {
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
    checkout
}

impl Machine {
    fn new(repo: &str) -> Self {
        Self {
            config: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
            checkout: checkout_of(repo),
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
        self.session_start_in(&self.root(), session)
    }

    /// One SessionStart hook whose working directory is `cwd`.
    fn session_start_in(&self, cwd: &std::path::Path, session: &str) -> Value {
        self.hook_in(
            cwd,
            session,
            json!({"hook_event_name": "SessionStart", "source": "startup"}),
        )
    }

    /// One hook event, `event` plus the session and `cwd`; returns the hook's JSON reply.
    fn hook_in(&self, cwd: &std::path::Path, session: &str, event: Value) -> Value {
        use std::io::Write;
        let mut child = self
            .command(&["--no-telemetry", "hook", "claude", "--client-connection"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut payload = event;
        payload["session_id"] = json!(session);
        payload["cwd"] = json!(cwd);
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
        self.mcp_at(&self.root(), &[])
    }

    /// A `client mcp` server started with `--root root` and `env` added to its environment.
    fn mcp_at(&self, root: &std::path::Path, env: &[(&str, &str)]) -> Mcp {
        let mut child = self
            .command(&[
                "--no-telemetry",
                "client",
                "mcp",
                "--root",
                root.to_str().unwrap(),
            ])
            .envs(env.iter().copied())
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

    /// One tool call; the reply's result.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        use std::io::{BufRead, Write};
        self.next += 1;
        let request = json!({"jsonrpc": "2.0", "id": self.next, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        writeln!(self.stdin, "{request}").unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()["result"].take()
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

/// Connecting before any sign-in signs in on the way; the token still comes through the computer,
/// so revoking the computer stops this agent too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connecting_with_a_first_sign_in_mints_through_the_computer() {
    let env = Env::start("github").await;
    env.fake.lock().unwrap().approve = ALICE;
    let machine = Machine::new(REPO);
    let (hub, root) = (env.hub.clone(), machine.root());
    let (ok, connected, stderr) = machine.run(&[
        "--no-telemetry",
        "client",
        "connect",
        "--hub",
        &hub,
        "--provider",
        "github",
        "--root",
        root.to_str().unwrap(),
    ]);
    assert!(ok, "{stderr}");
    assert!(
        connected.contains("renews automatically") && connected.contains("agentscm client renew"),
        "{connected}"
    );
    let expires = machine.connection()["owned_token_expires_at"]
        .as_i64()
        .unwrap();
    assert!(expires <= unix_now() + 24 * 60 * 60 + 60, "{expires}");
    let agent = machine.token();
    assert_eq!(env.whoami(&agent).await, 200);
    let (session, _, desktop) = env.sign_in(ALICE, "desktop").await;
    let (_, list) = env.get("/v1/hub/devices", &session).await;
    let laptop = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] != json!(desktop))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        env.delete(&format!("/v1/hub/devices/{laptop}"), &session)
            .await,
        204
    );
    assert_eq!(env.whoami(&agent).await, 401);
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
    // whoami says what the unchosen setting does and how to choose it.
    let (ok, whoami, stderr) = machine.run(&["hub", "whoami"]);
    assert!(ok, "{stderr}");
    assert!(whoami.contains("auto-connect not chosen yet"), "{whoami}");
    assert!(
        whoami.contains(&format!("agentscm hub auto-connect on|off --hub {hub}")),
        "{whoami}"
    );
    let (ok, _, stderr) = machine.run(&["hub", "auto-connect", "on"]);
    assert!(ok, "{stderr}");

    // The next session connects the checkout and says so in one line, writing no file in it.
    // That line is not always first: a Hub that shares agent conversations says so before it.
    let reply = machine.session_start("s1");
    assert!(
        context(&reply).contains(&format!("AgentSCM: coordinating on {REPO} at "))
            && context(&reply).contains("This checkout was just connected."),
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
    assert!(doctor.contains("Connection: authenticated"), "{doctor}");
    assert!(doctor.contains("Hub build: "), "{doctor}");

    // Renewal swaps the token before it ends and revokes the old one; a running MCP server
    // keeps working across it.
    let mut mcp = machine.mcp();
    assert!(mcp.tools_list().contains("\"result\""));
    assert_eq!(machine.renew()["result"], "not_due");
    machine.age_token();
    assert_eq!(machine.renew()["result"], "renewed");
    let after = mcp.tools_list();
    assert!(after.contains("\"result\""), "{after}");
    assert!(!after.contains("agentscm_promises_list"), "{after}");
    // AGENTSCM_MCP_TOOLS=all asks the Hub for the full list, opt-in tools included.
    let mut full = machine.mcp_at(&machine.root(), &[("AGENTSCM_MCP_TOOLS", "all")]);
    full.call("agentscm_work_status", json!({}));
    let listed = full.tools_list();
    assert!(listed.contains("agentscm_promises_list"), "{listed}");
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
        context(&reply).contains("AgentSCM: alice has no access on private repository"),
        "{reply}"
    );
    // The refusal is visible without opening the Hub: the token still works, so status says the
    // renewal failed, and says what to do.
    let root = machine.root();
    let (ok, status, stderr) = machine.run(&[
        "client",
        "status",
        "--json",
        "--check",
        "--root",
        root.to_str().unwrap(),
    ]);
    assert!(ok, "{stderr}");
    let status: Value = serde_json::from_str(&status).unwrap();
    let health = &status["connections"][0]["health"];
    assert_eq!(health["state"], "renewal_failed", "{status}");
    assert!(
        health["message"]
            .as_str()
            .unwrap()
            .contains("ask a repository owner for"),
        "{health}"
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

/// Two renewals that fall due together (two hooks, or a hook and a person) leave one live token
/// and neither reports a failure. The two processes may or may not overlap on a given run. If both
/// minted a token, both report `renewed` and the losing process's token is revoked, so exactly one
/// is live. If they did not overlap, one renews and the other reports `not_due`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_renewals_leave_one_live_token() {
    let env = Env::start("github").await;
    env.fake.lock().unwrap().approve = ALICE;
    let machine = Machine::new(REPO);
    let hub = env.hub.clone();
    let (ok, _, stderr) = machine.run(&["hub", "login", "--hub", &hub, "--provider", "github"]);
    assert!(ok, "{stderr}");
    let (ok, _, stderr) = machine.run(&["hub", "auto-connect", "on"]);
    assert!(ok, "{stderr}");
    machine.session_start("s1");
    let first = machine.token();
    let first_id = machine.connection()["owned_token_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(env.whoami(&first).await, 200);
    let alice = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == ALICE)
        .unwrap();
    let live = |env: &Env| -> Vec<String> {
        let now = unix_now();
        let mut ids: Vec<String> = env
            .store
            .list_agent_tokens(&alice.id)
            .unwrap()
            .into_iter()
            .filter(|token| !token.revoked && token.expires_at > now)
            .map(|token| token.id)
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(live(&env), std::slice::from_ref(&first_id));

    machine.age_token();
    let root = machine.root();
    let spawn = || {
        machine
            .command(&[
                "--no-telemetry",
                "client",
                "renew",
                "--root",
                root.to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    // Both processes are running before either is waited on.
    let children = [spawn(), spawn()];
    let mut results = Vec::new();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let reply: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
        // Which process wins is not fixed; neither may report the renewal as failed.
        assert!(
            ["renewed", "not_due"].contains(&reply["result"].as_str().unwrap()),
            "{reply}"
        );
        results.push(reply["result"].as_str().unwrap().to_owned());
    }

    let saved = machine.token();
    let saved_id = machine.connection()["owned_token_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(saved_id, first_id);
    assert_eq!(env.whoami(&first).await, 401);
    assert_eq!(env.whoami(&saved).await, 200);
    assert_eq!(live(&env), [saved_id]);
    // Every token issued for the user, live or revoked, including the first.
    let issued = env.store.list_agent_tokens(&alice.id).unwrap().len();
    if issued == 3 {
        // The runs overlapped: both minted a token and the loser's was revoked.
        assert_eq!(results, ["renewed", "renewed"]);
    } else {
        assert_eq!(issued, 2, "{results:?}");
        assert!(results.contains(&"not_due".to_owned()), "{results:?}");
    }
}

/// Signing in again replaces this computer's credential, which revokes the agent tokens it
/// issued; the sign-in renews each connected checkout, and names one it could not renew.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signing_in_again_keeps_connected_checkouts_working() {
    let env = Env::start("github").await;
    env.fake.lock().unwrap().approve = ALICE;
    let machine = Machine::new(REPO);
    let hub = env.hub.clone();
    let login = [
        "hub",
        "login",
        "--hub",
        hub.as_str(),
        "--provider",
        "github",
    ];
    let (ok, _, stderr) = machine.run(&login);
    assert!(ok, "{stderr}");
    let (ok, _, stderr) = machine.run(&["hub", "auto-connect", "on"]);
    assert!(ok, "{stderr}");
    machine.session_start("s1");
    let first = machine.token();
    assert_eq!(env.whoami(&first).await, 200);

    let (ok, _, stderr) = machine.run(&login);
    assert!(ok, "{stderr}");
    assert!(!stderr.contains("was not renewed"), "{stderr}");
    let second = machine.token();
    assert_ne!(first, second);
    assert_eq!(env.whoami(&first).await, 401);
    assert_eq!(env.whoami(&second).await, 200);

    // A checkout that cannot be renewed is named with its fix; the sign-in, allowed by a role
    // on another connected repository, still succeeds.
    env.connect_repo("acme/gadgets", 4343, ALICE);
    env.drop_role(ALICE);
    let (ok, _, stderr) = machine.run(&login);
    assert!(ok, "{stderr}");
    let root = machine.root();
    assert!(
        stderr.contains(&format!(
            "The connection of {} was not renewed (",
            root.display()
        )) && stderr.contains("reconnect it with `agentscm client connect` in that folder"),
        "{stderr}"
    );
    // The next agent in the checkout is told too.
    let reply = machine.session_start("s2");
    assert!(context(&reply).contains("was not renewed"), "{reply}");
}

/// A session that starts in one repository and edits another declares its Work in the other:
/// the MCP server sends the call to the repository of the file it names, under that
/// repository's token and the session the start hook recorded for this agent host.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_declares_work_in_the_other_connected_repository_it_edits() {
    const OTHER: &str = "acme/gadgets";
    const OTHER_ID: i64 = 4343;
    let env = Env::start("github").await;
    env.connect_repo(OTHER, OTHER_ID, ALICE);
    {
        let mut fake = env.fake.lock().unwrap();
        fake.roles.insert((OTHER.into(), ALICE), ("write", "write"));
        fake.levels.insert((OTHER_ID, ALICE), 30);
        fake.approve = ALICE;
    }
    let machine = Machine::new(REPO);
    let other = checkout_of(OTHER);
    let other = other.path().canonicalize().unwrap();
    for root in [machine.root(), other.clone()] {
        let (ok, _, stderr) = machine.run(&[
            "--no-telemetry",
            "client",
            "connect",
            "--hub",
            &env.hub,
            "--provider",
            "github",
            "--root",
            root.to_str().unwrap(),
        ]);
        assert!(ok, "{stderr}");
    }

    // The start context names the second repository, so the agent knows it is covered.
    let reply = machine.session_start("native-1");
    assert!(
        context(&reply).contains(&format!("Also connected on this computer: {OTHER}.")),
        "{reply}"
    );

    // No session and no root in the call: the file's path and the hook's record decide both.
    let mut mcp = machine.mcp();
    let file = other.join("src/lib.rs");
    let started = mcp.call(
        "agentscm_start",
        json!({"task": "Change the gadget", "paths": [file]}),
    );
    assert_ne!(started["isError"], true, "{started}");
    let said = started["content"].to_string();
    assert!(
        said.contains(&format!("this call went to {OTHER}")),
        "{said}"
    );
    let connections = machine.file("connections.json");
    let work = |repo: &str| {
        let token = connections["connections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|connection| connection["repo"] == repo)
            .unwrap()["owned_token_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let query = agentscm::hub::work::WorkOverviewQuery::default();
        env.store
            .work_overview(&token, &query, unix_now())
            .unwrap()
            .current
    };
    let recorded = work(OTHER);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].title, "Change the gadget");
    assert_eq!(recorded[0].scope, ["src/lib.rs"]);
    assert_eq!(recorded[0].lead_session, "native-1");
    assert!(work(REPO).is_empty());

    // A read reaches the same repository by root; without it the start checkout answers.
    let there = mcp.call(
        "agentscm_work_overview",
        json!({"root": other, "detail": "full"}),
    );
    assert!(there.to_string().contains("Change the gadget"), "{there}");
    let here = mcp.call("agentscm_work_overview", json!({"detail": "full"}));
    assert!(!here.to_string().contains("Change the gadget"), "{here}");

    // One call cannot declare files of both repositories.
    let both = mcp.call(
        "agentscm_start",
        json!({"task": "Change both", "paths": [machine.root().join("a.rs"), file]}),
    );
    assert_eq!(both["isError"], true, "{both}");
    assert!(
        both.to_string().contains("Call once per repository"),
        "{both}"
    );
    assert_eq!(work(OTHER).len(), 1);
    assert!(work(REPO).is_empty());
}

/// A Claude Code session in a linked worktree that has its own connection is one agent: the
/// MCP server, started in the main checkout, declares Work under the worktree connection the
/// session's hooks use, not under the main checkout's.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_in_a_worktree_with_its_own_connection_declares_work_as_that_agent() {
    let env = Env::start("github").await;
    env.fake.lock().unwrap().approve = ALICE;
    let machine = Machine::new(REPO);
    let main = machine.root();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&main)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["commit", "-q", "--allow-empty", "-m", "base"]);
    let linked = machine.home.path().join("linked");
    git(&["worktree", "add", "-q", linked.to_str().unwrap()]);
    let linked = linked.canonicalize().unwrap();
    for (root, own) in [(&main, false), (&linked, true)] {
        let mut args = vec!["--no-telemetry", "client", "connect"];
        if own {
            args.push("--own-connection");
        }
        args.extend(["--hub", &env.hub, "--provider", "github", "--root"]);
        args.push(root.to_str().unwrap());
        let (ok, _, stderr) = machine.run(&args);
        assert!(ok, "{stderr}");
    }

    machine.session_start_in(&linked, "native-1");
    let mut mcp = machine.mcp_at(&main, &[]);
    let started = mcp.call(
        "agentscm_start",
        json!({"task": "Work in the worktree", "session": "native-1"}),
    );
    assert_ne!(started["isError"], true, "{started}");
    // The same repository, so the reply does not claim it left the session's repository.
    assert!(!started["content"].to_string().contains("this call went to"));
    // A session no hook routed stays on the main checkout's connection.
    let default = mcp.call(
        "agentscm_start",
        json!({"task": "Work in the main checkout", "session": "native-2"}),
    );
    assert_ne!(default["isError"], true, "{default}");

    let connections = machine.file("connections.json");
    let token = connections["connections"][0]["owned_token_id"]
        .as_str()
        .unwrap();
    let query = agentscm::hub::work::WorkOverviewQuery::default();
    let current = env
        .store
        .work_overview(token, &query, unix_now())
        .unwrap()
        .current;
    let lead = |task: &str| {
        let work = current.iter().find(|work| work.title == task).unwrap();
        work.lead_agent.clone()
    };
    let (worktree_agent, main_agent) = (
        lead("Work in the worktree"),
        lead("Work in the main checkout"),
    );
    assert_ne!(worktree_agent, main_agent, "{current:?}");
    // Not asserted: the Work's lead is a Hub-side agent name, and no hook here declares Work
    // that could be compared with it, so the two agents are told apart by name only.

    // Ending the session drops its route record, and the hook does not write it back.
    let routes = machine.config.path().join("agentscm/session-routes");
    let recorded = || std::fs::read_dir(&routes).map_or(0, Iterator::count);
    assert_eq!(recorded(), 1);
    machine.hook_in(
        &linked,
        "native-1",
        json!({"hook_event_name": "SessionEnd"}),
    );
    assert_eq!(recorded(), 0);
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

/// Runs a web-page sign-in for `user` up to the Hub's answer to the forge's callback.
async fn callback_for(env: &Env, user: i64, purpose: &str) -> reqwest::Response {
    callback_query(env, user, &format!("{purpose}=1")).await
}

/// `callback_for` with the sign-in's whole query, e.g. `web=1&return_to=%2Fnext%2F`.
async fn callback_query(env: &Env, user: i64, query: &str) -> reqwest::Response {
    env.fake.lock().unwrap().approve = user;
    let http = browser();
    let start = http
        .get(format!("{}/v1/hub/login/github?{query}", env.hub))
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
    http.get(callback)
        .header(header::COOKIE, format!("__Host-agentscm_login={login}"))
        .send()
        .await
        .unwrap()
}

async fn web_callback(env: &Env, user: i64) -> reqwest::Response {
    callback_for(env, user, "web").await
}

/// Signs `user` in to the Hub's web pages; returns the session cookie's value.
async fn web_sign_in(env: &Env, user: i64) -> String {
    let done = web_callback(env, user).await;
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
    // Each refusal carries its own error reference; blank it so equal refusals compare equal.
    let text = crate::hub_error_assert::blank_references(&text);
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// A signed-in web page: the cookie, its CSRF token and the person's user id.
struct Page {
    cookie: String,
    csrf: String,
    id: String,
}

async fn page(env: &Env, user: i64) -> Page {
    let cookie = web_sign_in(env, user).await;
    let (_, me) = with_cookie(&env.hub, Method::GET, "/v1/hub/session", &cookie, None).await;
    Page {
        csrf: me["csrf_token"].as_str().unwrap().to_owned(),
        id: me["user"]["id"].as_str().unwrap().to_owned(),
        cookie,
    }
}

/// A POST from a web page; `csrf` is withheld to play another site's form.
async fn page_post(env: &Env, page: &Page, csrf: bool, path: &str, body: Value) -> (u16, Value) {
    let mut request = browser()
        .post(format!("{}{path}", env.hub))
        .header(
            header::COOKIE,
            format!("__Host-agentscm_session={}", page.cookie),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string());
    if csrf {
        request = request.header("x-csrf-token", &page.csrf);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn agent_for(env: &Env, owner: &str, name: &str, read_only: bool) -> (String, String) {
    let (id, secret, _) =
        mint_bound_agent_token(&*env.store, owner, name, REPO, REPO_ID, 3600, read_only).unwrap();
    (id, secret.expose().to_owned())
}

fn hook_call(session: &str, paths: &[&str]) -> agentscm::hub::session::SessionCall {
    agentscm::hub::session::SessionCall {
        session: Some(session.to_owned()),
        via: "hook:claude".into(),
        task: None,
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        deny: true,
        report: true,
    }
}

fn work(key: &str, session: &str) -> agentscm::hub::work::WorkStart {
    agentscm::hub::work::WorkStart {
        issue_choice: None,
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: key.into(),
        description: None,
        session: session.into(),
        role: agentscm::hub::work::WorkRole::Lead,
        scope: vec!["src/billing.rs".into()],
        branch: None,
        worktree: None,
        next_action: Some("continue".into()),
        depends_on: Vec::new(),
        related_work: Vec::new(),
        takeover_generation: None,
        preflight: None,
    }
}

const REPLY: &str = "/v1/hub/session/coordination/reply";
const ANSWER: &str = "/v1/hub/session/handoff/answer";
const ASK: &str = "/v1/hub/session/handoff/ask";

#[tokio::test]
async fn a_web_session_answers_a_coordination_request_only_for_its_own_agent() {
    let env = Env::start("github").await;
    let (alice, bob) = (page(&env, ALICE).await, page(&env, BOB).await);
    let (lead, _) = agent_for(&env, &alice.id, "claude-code", false);
    let (lead_read_only, _) = agent_for(&env, &alice.id, "viewer", true);
    let (asker, asker_secret) = agent_for(&env, &bob.id, "codex", false);
    let now = unix_now();
    env.store
        .work_start(&lead, &work("billing", "alice-session"), now)
        .unwrap();
    let request = env
        .store
        .work_start(&asker, &work("rounding", "bob-session"), now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let body = |token: &str, operation: &str| {
        json!({"token": token, "request_id": request.id, "operation_id": operation,
            "target_work_id": request.targets[0].work_id, "session": "alice-session",
            "expected_updated_seq": request.updated_seq, "kind": "propose_split",
            "note": "I take tax, you take rounding"})
    };

    // Signed out, a cookie without its CSRF token, and a computer's bearer session are refused.
    let (status, _) = env
        .call(&env.hub, Method::POST, REPLY, None, Some(body(&lead, "a")))
        .await;
    assert_eq!(status, 401);
    let (status, refused) = page_post(&env, &alice, false, REPLY, body(&lead, "a")).await;
    assert_eq!((status, refused["error"].as_str()), (403, Some("csrf")));
    let (session, _, _) = env.sign_in(ALICE, "laptop").await;
    let (status, refused) = env
        .call(
            &env.hub,
            Method::POST,
            REPLY,
            Some(&session),
            Some(body(&lead, "a")),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("forbidden"))
    );

    // Nobody answers for an agent that is not theirs: not another person's, not a read-only
    // token of their own, not one that does not exist. All look alike.
    for (who, token) in [
        (&bob, lead.as_str()),
        (&alice, asker.as_str()),
        (&alice, lead_read_only.as_str()),
        (&alice, "tok_00000000000000000000000000000000"),
    ] {
        let (status, refused) = page_post(&env, who, true, REPLY, body(token, "b")).await;
        assert_eq!(
            (status, refused["error"].as_str()),
            (403, Some("not_your_agent")),
            "{token}"
        );
    }
    // The store's own rule still holds for the right owner: only the Work's lead session.
    let mut wrong = body(&lead, "c");
    wrong["session"] = json!("someone-elses-session");
    let (status, _) = page_post(&env, &alice, true, REPLY, wrong).await;
    assert_eq!(status, 403);

    // Bad kinds, unknown fields (no one sets the mark themselves) and long notes are refused.
    let mut bad_kind = body(&lead, "d");
    bad_kind["kind"] = json!("merge_it");
    assert_eq!(page_post(&env, &alice, true, REPLY, bad_kind).await.0, 400);
    let mut marked = body(&lead, "d");
    marked["via_hub"] = json!(false);
    assert_eq!(page_post(&env, &alice, true, REPLY, marked).await.0, 400);
    let mut long = body(&lead, "d");
    long["note"] = json!("x".repeat(501));
    assert_eq!(page_post(&env, &alice, true, REPLY, long).await.0, 400);
    let mut stale = body(&lead, "e");
    stale["expected_updated_seq"] = json!(request.updated_seq + 99);
    let (status, refused) = page_post(&env, &alice, true, REPLY, stale).await;
    assert_eq!(status, 400, "{refused}");
    assert!(
        refused["message"].as_str().unwrap().contains("changed"),
        "{refused}"
    );

    // The owner answers; the record says a person did, beside the agent that leads the Work.
    let (status, answered) = page_post(&env, &alice, true, REPLY, body(&lead, "f")).await;
    assert_eq!(status, 200, "{answered}");
    let response = &answered["responses"][0];
    assert_eq!(response["via"], "hub");
    assert_eq!(response["kind"], "propose_split");
    assert_eq!(response["responder"]["login"], "alice");
    assert_eq!(response["responder"]["agent"], "claude-code");
    assert_eq!(response["responder"]["token"], json!(lead));
    // A retry replays the same answer instead of adding a second.
    let (status, replay) = page_post(&env, &alice, true, REPLY, body(&lead, "f")).await;
    assert_eq!(
        (status, replay["responses"].as_array().unwrap().len()),
        (200, 1)
    );

    // The asking agent reads it in its inbox at its next touch, marked as a person's answer.
    let (status, inbox) = env
        .get("/v1/hub/standup/inbox?session=bob-session", &asker_secret)
        .await;
    assert_eq!(status, 200, "{inbox}");
    assert_eq!(inbox["requests"][0]["responses"][0]["via"], "hub");
    assert_eq!(
        inbox["requests"][0]["responses"][0]["note"],
        "I take tax, you take rounding"
    );
    let events = env.store.events_after(0, 1000).unwrap();
    let responded: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "coordination_responded")
        .collect();
    assert_eq!(responded.len(), 1);
    assert!(responded[0].data.contains("\"via\":\"hub\""));
}

const WITHDRAW: &str = "/v1/hub/session/coordination/withdraw";

#[tokio::test]
async fn a_web_session_closes_a_request_only_once_its_requester_is_gone() {
    let env = Env::start("github").await;
    let (alice, bob) = (page(&env, ALICE).await, page(&env, BOB).await);
    let (lead, lead_secret) = agent_for(&env, &alice.id, "claude-code", false);
    let (lead_read_only, _) = agent_for(&env, &alice.id, "viewer", true);
    let (asker, _) = agent_for(&env, &bob.id, "codex", false);
    let now = unix_now();
    let incumbent = env
        .store
        .work_start(&lead, &work("billing", "alice-session"), now)
        .unwrap()
        .item;
    let request = env
        .store
        .work_start(&asker, &work("rounding", "bob-session"), now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let target = &request.targets[0];
    let body = |token: &str, operation: &str| {
        json!({"token": token, "request_id": request.id, "operation_id": operation,
            "session": "alice-session", "expected_updated_seq": request.updated_seq,
            "target_work_id": target.work_id, "target_generation": target.generation})
    };
    let action = format!("request:{}:{}", request.id, target.work_id);
    let has_action = |status: &Value| {
        status["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == json!(action) && a["kind"] == "directed_request")
    };
    let listed = |value: &Value| value["requests"][0]["requester_unavailable"].clone();

    // While the requesting agent can act, nobody closes it for it, and the list says so.
    let (_, status) = env.get("/v1/hub/standup/status", &lead_secret).await;
    assert!(has_action(&status), "{status}");
    let (_, before) = env.get("/v1/hub/standup/coordination", &lead_secret).await;
    assert_eq!(listed(&before), Value::Null, "{before}");
    let (status, refused) = page_post(&env, &alice, true, WITHDRAW, body(&lead, "early")).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_your_agent"))
    );
    // The requester's owner cannot close its live request here either: this route is for leads.
    let mut as_requester = body(&asker, "own");
    as_requester["session"] = json!("bob-session");
    let (status, refused) = page_post(&env, &bob, true, WITHDRAW, as_requester).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_your_agent"))
    );

    env.store.revoke_agent_token(&asker).unwrap();
    let (_, after) = env.get("/v1/hub/standup/coordination", &lead_secret).await;
    assert_eq!(listed(&after), json!(true), "{after}");

    // Signed out, a cookie without its CSRF token, and a computer's bearer session are refused.
    let (status, _) = env
        .call(
            &env.hub,
            Method::POST,
            WITHDRAW,
            None,
            Some(body(&lead, "a")),
        )
        .await;
    assert_eq!(status, 401);
    let (status, refused) = page_post(&env, &alice, false, WITHDRAW, body(&lead, "a")).await;
    assert_eq!((status, refused["error"].as_str()), (403, Some("csrf")));
    let (session, _, _) = env.sign_in(ALICE, "laptop").await;
    let (status, refused) = env
        .call(
            &env.hub,
            Method::POST,
            WITHDRAW,
            Some(&session),
            Some(body(&lead, "a")),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("forbidden"))
    );

    // Nobody closes it for an agent that is not theirs: not another person's, not a read-only
    // token of their own, not one that does not exist. All look alike.
    for (who, token) in [
        (&bob, lead.as_str()),
        (&alice, asker.as_str()),
        (&alice, lead_read_only.as_str()),
        (&alice, "tok_00000000000000000000000000000000"),
    ] {
        let (status, refused) = page_post(&env, who, true, WITHDRAW, body(token, "b")).await;
        assert_eq!(
            (status, refused["error"].as_str()),
            (403, Some("not_your_agent")),
            "{token}"
        );
    }
    // The store's own rule still holds for the right owner: only the Work's lead session.
    let mut wrong_session = body(&lead, "c");
    wrong_session["session"] = json!("someone-elses-session");
    assert_eq!(
        page_post(&env, &alice, true, WITHDRAW, wrong_session)
            .await
            .0,
        403
    );

    // Unknown fields (no one sets the mark themselves), a missing fence and bad values are refused.
    let mut marked = body(&lead, "d");
    marked["via_hub"] = json!(false);
    assert_eq!(page_post(&env, &alice, true, WITHDRAW, marked).await.0, 400);
    let mut unfenced = body(&lead, "d");
    unfenced
        .as_object_mut()
        .unwrap()
        .remove("target_generation");
    assert_eq!(
        page_post(&env, &alice, true, WITHDRAW, unfenced).await.0,
        400
    );
    let mut zero = body(&lead, "d");
    zero["target_generation"] = json!(0);
    assert_eq!(page_post(&env, &alice, true, WITHDRAW, zero).await.0, 400);

    // A page that saw an older sequence or generation is told to look again.
    for (field, value) in [
        ("expected_updated_seq", json!(request.updated_seq + 99)),
        ("target_generation", json!(target.generation + 1)),
    ] {
        let mut stale = body(&lead, "e");
        stale[field] = value;
        let (status, refused) = page_post(&env, &alice, true, WITHDRAW, stale).await;
        assert_eq!(status, 400, "{refused}");
        assert!(
            refused["message"].as_str().unwrap().contains("changed"),
            "{refused}"
        );
    }
    let (_, untouched) = env.get("/v1/hub/standup/coordination", &lead_secret).await;
    assert_eq!(untouched["requests"][0]["status"], "pending");

    // The owner closes it for the agent that leads the Work; the record says a person did.
    let (status, closed) = page_post(&env, &alice, true, WITHDRAW, body(&lead, "f")).await;
    assert_eq!(status, 200, "{closed}");
    assert_eq!(closed["status"], "withdrawn");
    // A retry replays the same close instead of closing twice.
    let (status, replay) = page_post(&env, &alice, true, WITHDRAW, body(&lead, "f")).await;
    assert_eq!(
        (status, &replay["updated_seq"]),
        (200, &closed["updated_seq"])
    );
    let events = env.store.events_after(0, 1000).unwrap();
    let withdrawn: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "coordination_withdrawn")
        .collect();
    assert_eq!(withdrawn.len(), 1);
    assert!(withdrawn[0].data.contains("\"via\":\"hub\""));
    assert!(
        withdrawn[0]
            .data
            .contains("\"reason\":\"requester_unavailable\"")
    );

    // It stops counting as something to do, and the lead's Work was not released.
    let (_, status) = env.get("/v1/hub/standup/status", &lead_secret).await;
    assert!(!has_action(&status), "{status}");
    let kept = env
        .store
        .work_get(&lead, &incumbent.id, now + 2)
        .unwrap()
        .unwrap();
    assert_eq!(kept.lead_token, lead);
}

const TRANSITION: &str = "/v1/hub/session/work/transition";

fn person_transition(item: &agentscm::hub::work::WorkItem, operation: &str, kind: &str) -> Value {
    json!({"repository": REPO, "work_id": item.id, "operation_id": operation,
        "generation": item.generation, "expected_updated_seq": item.updated_seq,
        "transition": kind, "reason": "synthetic reason"})
}

#[tokio::test]
async fn a_web_session_releases_or_ends_work_without_any_agent_token() {
    let env = Env::start("github").await;
    let (alice, bob, admin) = (
        page(&env, ALICE).await,
        page(&env, BOB).await,
        page(&env, ADMIN).await,
    );
    let now = unix_now();
    let start = |token: &str, key: &str, path: &str, at: i64| {
        let mut start = work(key, &format!("{key}-session"));
        start.scope = vec![path.into()];
        env.store.work_start(token, &start, at).unwrap().item
    };
    let (lead, _) = agent_for(&env, &alice.id, "claude-code", false);
    let first = start(&lead, "first", "src/first.rs", now);
    // The lead's token is gone; the owner still acts.
    env.store.revoke_agent_token(&lead).unwrap();

    // Signed out, without the page's CSRF token, and a computer's bearer session are refused.
    let release = person_transition(&first, "rel-1", "release");
    let (status, _) = env
        .call(
            &env.hub,
            Method::POST,
            TRANSITION,
            None,
            Some(release.clone()),
        )
        .await;
    assert_eq!(status, 401);
    let (status, refused) = page_post(&env, &alice, false, TRANSITION, release.clone()).await;
    assert_eq!((status, refused["error"].as_str()), (403, Some("csrf")));
    let (session, _, _) = env.sign_in(ALICE, "laptop").await;
    let (status, _) = env
        .call(
            &env.hub,
            Method::POST,
            TRANSITION,
            Some(&session),
            Some(release.clone()),
        )
        .await;
    assert_eq!(status, 403);
    // Another person, even one who can read the repository, cannot touch Alice's Work.
    let (status, refused) = page_post(&env, &bob, true, TRANSITION, release.clone()).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_your_work"))
    );

    // The route takes the same `reclaimed` opt-in as the agent routes.
    assert_eq!(
        page_post(
            &env,
            &alice,
            true,
            &format!("{TRANSITION}?reclaimed=2"),
            release.clone()
        )
        .await
        .0,
        400
    );

    // Unknown fields and an unconnected repository are refused.
    let mut stray = release.clone();
    stray["session"] = json!("someone");
    assert_eq!(
        page_post(&env, &alice, true, TRANSITION, stray).await.0,
        400
    );
    let mut elsewhere = release.clone();
    elsewhere["repository"] = json!("acme/elsewhere");
    assert_eq!(
        page_post(&env, &alice, true, TRANSITION, elsewhere).await.0,
        403
    );

    // The owner releases without a token, and an exact retry returns the same receipt.
    let (status, done) = page_post(&env, &alice, true, TRANSITION, release.clone()).await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["item"]["state"], "paused");
    assert!(done["conflict"].is_null());
    assert!(done.get("reclaimed").is_none(), "{done}");
    let (status, retry) = page_post(&env, &alice, true, TRANSITION, release).await;
    assert_eq!(status, 200, "{retry}");
    assert_eq!(retry["receipt"]["event_seq"], done["receipt"]["event_seq"]);
    assert_eq!(retry["receipt"]["replayed"], true);
    let events = env.store.events_after(0, 1000).unwrap();
    let recorded: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "work_transition")
        .collect();
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].data.contains("\"kind\":\"owner\""));
    assert!(recorded[0].data.contains("\"via\":\"person\""));
    assert!(!recorded[0].data.contains(&lead));

    // An administrator who is not the owner may release or abandon, never complete.
    let (second_lead, _) = agent_for(&env, &alice.id, "codex", false);
    let second = start(&second_lead, "second", "src/second.rs", now + 1);
    let (status, refused) = page_post(
        &env,
        &admin,
        true,
        TRANSITION,
        person_transition(&second, "adm-complete", "complete"),
    )
    .await;
    assert_eq!(status, 400, "{refused}");
    let (status, released) = page_post(
        &env,
        &admin,
        true,
        TRANSITION,
        person_transition(&second, "adm-release", "release"),
    )
    .await;
    assert_eq!(status, 200, "{released}");
    assert_eq!(released["item"]["state"], "paused");
    // A person cannot reopen: a lead needs an agent session.
    let third = start(&second_lead, "third", "src/third.rs", now + 2);
    let (_, ended) = page_post(
        &env,
        &alice,
        true,
        TRANSITION,
        person_transition(&third, "end-third", "abandon"),
    )
    .await;
    assert_eq!(ended["item"]["state"], "finished");
    let finished = env
        .store
        .work_get(&second_lead, &third.id, now + 3)
        .unwrap()
        .unwrap();
    let (status, _) = page_post(
        &env,
        &alice,
        true,
        TRANSITION,
        person_transition(&finished, "reopen-third", "reopen"),
    )
    .await;
    assert_eq!(status, 400);

    // Repository access is checked on every call: no role, or read on a public repository.
    let fourth = start(&second_lead, "fourth", "src/fourth.rs", now + 4);
    env.drop_role(ALICE);
    let (status, refused) = page_post(
        &env,
        &alice,
        true,
        TRANSITION,
        person_transition(&fourth, "no-role", "release"),
    )
    .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("insufficient_role"))
    );
    env.set_repo(REPO, REPO_ID, "public");
    env.role(ALICE, "read", "read", 20);
    let (status, refused) = page_post(
        &env,
        &alice,
        true,
        TRANSITION,
        person_transition(&fourth, "read-only", "release"),
    )
    .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("insufficient_role"))
    );
    env.role(ALICE, "write", "write", 30);
    let (status, _) = page_post(
        &env,
        &alice,
        true,
        TRANSITION,
        person_transition(&fourth, "role-back", "release"),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_web_session_asks_and_answers_file_handoffs_only_for_its_own_agents() {
    let env = Env::start("github").await;
    let (alice, bob, admin) = (
        page(&env, ALICE).await,
        page(&env, BOB).await,
        page(&env, ADMIN).await,
    );
    let (holder, _) = agent_for(&env, &alice.id, "claude-code", false);
    let (wanter, _) = agent_for(&env, &bob.id, "codex", false);
    let file = "src/billing.rs";
    let now = unix_now();
    env.store
        .touch_session(&holder, &hook_call("ha", &[file]), now)
        .unwrap();
    env.store
        .touch_session(&wanter, &hook_call("hb", &[]), now + 1)
        .unwrap();
    let ask = |token: &str| json!({"token": token, "session": "hb", "path": file, "note": "rounding fix"});

    // Bob may not ask through Alice's page, without the page's CSRF token, or signed out.
    let (status, _) = page_post(&env, &bob, false, ASK, ask(&wanter)).await;
    assert_eq!(status, 403);
    let (status, refused) = page_post(&env, &alice, true, ASK, ask(&wanter)).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_your_agent"))
    );
    let (status, _) = env
        .call(&env.hub, Method::POST, ASK, None, Some(ask(&wanter)))
        .await;
    assert_eq!(status, 401);
    let mut glob = ask(&wanter);
    glob["path"] = json!("src/**");
    assert_eq!(page_post(&env, &bob, true, ASK, glob).await.0, 400);
    // A long note is cut like an agent's, not refused.
    let mut long = ask(&wanter);
    long["note"] = json!("word ".repeat(100));
    let (status, asked) = page_post(&env, &bob, true, ASK, long).await;
    assert_eq!(
        (status, asked["outcome"].as_str()),
        (200, Some("requested")),
        "{asked}"
    );
    assert_eq!(asked["holder"]["login"], "alice");
    let (_, again) = page_post(&env, &bob, true, ASK, ask(&wanter)).await;
    assert_eq!(again["outcome"], "already_asked");

    // Each person reads their own side of it, with the other agent's token and session withheld.
    let list = |page: &Page| {
        let (hub, cookie) = (env.hub.clone(), page.cookie.clone());
        async move {
            with_cookie(
                &hub,
                Method::GET,
                &format!("/v1/hub/session/handoffs?repository={REPO}"),
                &cookie,
                None,
            )
            .await
        }
    };
    let (status, mine) = list(&alice).await;
    assert_eq!(status, 200, "{mine}");
    let row = &mine["handoffs"][0];
    assert_eq!(
        (row["state"].as_str(), row["asked_via_hub"].as_bool()),
        (Some("pending"), Some(true))
    );
    assert_eq!(row["holder"]["token"], json!(holder));
    assert_eq!(row["holder"]["session"], "ha");
    assert!(row["requester"].get("token").is_none() && row["requester"]["mine"] == false);
    let (_, theirs) = list(&bob).await;
    assert_eq!(theirs["handoffs"][0]["requester"]["token"], json!(wanter));
    assert!(theirs["handoffs"][0]["holder"].get("token").is_none());
    assert!(
        list(&admin).await.1["handoffs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let path = "/v1/hub/session/handoffs?repository=acme/widgets";
    assert_eq!(
        with_cookie(&env.hub, Method::GET, path, "nope", None)
            .await
            .0,
        401
    );

    // The holder's agent hears it at its next touch, from a person at the Hub.
    let told = env
        .store
        .touch_session(&holder, &hook_call("ha", &[]), now + 5)
        .unwrap();
    assert_eq!(told.handoffs.len(), 1);
    assert_eq!(told.handoffs[0].via, Via::Hub);

    // Bob cannot answer for Alice's agent; she can, once.
    let answer = |token: &str, accept: bool| json!({"token": token, "session": "ha", "path": file, "accept": accept});
    let (status, refused) = page_post(&env, &bob, true, ANSWER, answer(&holder, true)).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_your_agent"))
    );
    assert_eq!(
        page_post(&env, &alice, false, ANSWER, answer(&holder, true))
            .await
            .0,
        403
    );
    let (status, done) = page_post(&env, &alice, true, ANSWER, answer(&holder, true)).await;
    assert_eq!(
        (status, done["outcome"].as_str()),
        (200, Some("granted")),
        "{done}"
    );
    assert_eq!(done["requester"]["login"], "bob");
    let (_, twice) = page_post(&env, &alice, true, ANSWER, answer(&holder, true)).await;
    assert_eq!(twice["outcome"], "no_open_request");

    // The wanting agent hears the grant, and the record says a person gave it.
    let heard = env
        .store
        .touch_session(&wanter, &hook_call("hb", &[file]), now + 9)
        .unwrap();
    assert_eq!(heard.handoffs.len(), 1);
    assert!(heard.handoffs[0].via == Via::Hub && heard.handoffs[0].kind == "grant");
    let grants: Vec<_> = env
        .store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "handoff_grant")
        .collect();
    assert!(grants[0].data.contains("\"via\":\"hub\""));
    let (_, after) = list(&alice).await;
    assert_eq!(after["handoffs"][0]["state"], "granted");
    assert_eq!(after["handoffs"][0]["answered_via_hub"], true);
}

#[tokio::test]
async fn a_web_sign_in_returns_to_the_hub_page_that_asked_and_nowhere_else() {
    let env = Env::start("github").await;
    let done = callback_query(&env, ALICE, "web=1&return_to=%2Fnext%2F").await;
    assert_eq!(done.status(), 302);
    assert_eq!(done.headers()[header::LOCATION], "/next/#home");
    assert!(set_cookie(&done, "__Host-agentscm_session").is_some());
    // Another site, an API route or an odd path is refused before GitHub is asked.
    for bad in [
        "https%3A%2F%2Fevil.example%2F",
        "%2F%2Fevil.example%2F",
        "%2Fv1%2Fhub%2Fsession%2F",
        "%2Fnext",
    ] {
        let start = browser()
            .get(format!(
                "{}/v1/hub/login/github?web=1&return_to={bad}",
                env.hub
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(start.status(), 400, "{bad}");
    }
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
    // A refused web sign-in lands on the Hub's page with the reason alone, which the page
    // words itself: anyone can forge that link.
    let refused = web_callback(&env, ALICE).await;
    assert_eq!(refused.status(), 302);
    assert_eq!(
        refused.headers()[header::LOCATION],
        "/ui/#/signed-in?status=refused&reason=user_disabled"
    );
    assert!(set_cookie(&refused, "__Host-agentscm_session").is_none());
    // A page that asked to come back gets the refusal on itself.
    let refused = callback_query(&env, ALICE, "web=1&return_to=%2Fnext%2F").await;
    assert_eq!(
        refused.headers()[header::LOCATION],
        "/next/#/signed-in?status=refused&reason=user_disabled"
    );
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

/// Asks for the web pages' read-only token with the web sign-in's cookie.
async fn web_read_token(
    env: &Env,
    cookie: &str,
    csrf: Option<&str>,
    repository: &str,
) -> (u16, Value) {
    let mut request = browser()
        .post(format!("{}/v1/hub/session/read-token", env.hub))
        .header(header::COOKIE, format!("__Host-agentscm_session={cookie}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"repository": repository}).to_string());
    if let Some(csrf) = csrf {
        request = request.header("x-csrf-token", csrf);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_web_sign_in_reads_with_a_read_only_token_under_the_mint_checks() {
    let env = Env::start("github").await;
    let cookie = web_sign_in(&env, ALICE).await;
    let (_, me) = with_cookie(&env.hub, Method::GET, "/v1/hub/session", &cookie, None).await;
    let csrf = me["csrf_token"].as_str().unwrap().to_owned();

    // The cookie alone, as another site's form would send it, gets nothing.
    let (status, refused) = web_read_token(&env, &cookie, None, REPO).await;
    assert_eq!((status, refused["error"].as_str()), (403, Some("csrf")));

    let (status, minted) = web_read_token(&env, &cookie, Some(&csrf), REPO).await;
    assert_eq!(status, 201, "{minted}");
    let token = minted["token"].as_str().unwrap().to_owned();
    let expires_at = minted["expires_at"].as_i64().unwrap();
    assert!((unix_now() + 1790..=unix_now() + 1800).contains(&expires_at));
    // It reads like any read-only repository token, and changes nothing.
    let (status, whoami) = env.get("/v1/hub/whoami", &token).await;
    assert_eq!(status, 200, "{whoami}");
    assert_eq!(whoami["token"]["read_only"], true);
    assert_eq!(whoami["token"]["repository_id"], REPO_ID);
    assert_eq!(env.get("/v1/hub/standup/overview", &token).await.0, 200);

    // Only the web sign-in asks: a `hub login` session and a computer credential are refused.
    let (session, device, _) = env.sign_in(ALICE, "laptop").await;
    let body = json!({"repository": REPO});
    let path = "/v1/hub/session/read-token";
    let (status, refused) = env
        .call(
            &env.hub,
            Method::POST,
            path,
            Some(&session),
            Some(body.clone()),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("forbidden"))
    );
    let (status, _) = env
        .call(&env.hub, Method::POST, path, Some(&device), Some(body))
        .await;
    assert_eq!(status, 401);

    // A repository the Hub does not have is refused as for a computer, naming a connected
    // upstream of a fork.
    env.fake
        .lock()
        .unwrap()
        .forks
        .insert("carol/widgets".into(), REPO.into());
    let (status, refused) = web_read_token(&env, &cookie, Some(&csrf), "carol/widgets").await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_connected"))
    );
    assert_eq!(refused["fork_of"], REPO);

    // Losing the role refuses the next one, with the same refusal a computer gets.
    env.drop_role(ALICE);
    let (status, refused) = web_read_token(&env, &cookie, Some(&csrf), REPO).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("insufficient_role"))
    );
    assert!(
        refused["message"].as_str().unwrap().contains("; "),
        "{refused}"
    );
    // Administrators see it among the refused sign-ins.
    let refusals = env.store.refusals(unix_now()).unwrap();
    assert!(
        refusals
            .iter()
            .any(|r| (r.reason.as_str(), r.via.as_str()) == ("insufficient_role", "web_read_token"))
    );
}

/// An agent reads current Work across the repositories its person can read now: the same role
/// check as the All repositories page, another owner's sessions digested, lists bounded, and a
/// repository the forge could not answer for counted, never named.
#[tokio::test]
async fn an_agent_lists_current_work_in_the_repositories_its_person_can_read() {
    use agentscm::hub::work::{WorkRole, WorkStart};
    let env = Env::start("github").await;
    env.connect_repo("acme/docs", 4343, ALICE);
    // Connected, but alice cannot read it: bob's only.
    env.connect_repo("acme/secret", 4444, BOB);
    // Alice could read this one, and lost access before she asked.
    env.connect_repo("acme/gone", 4545, ALICE);
    {
        let mut fake = env.fake.lock().unwrap();
        fake.roles.remove(&("acme/gone".into(), ALICE));
        fake.levels.remove(&(4545, ALICE));
    }
    let (_, device, _) = env.sign_in(ALICE, "laptop").await;
    let (status, minted) = env.mint(&device).await;
    assert_eq!(status, 201, "{minted}");
    let alice = minted["token"].as_str().unwrap().to_owned();
    let path = "/v1/hub/standup/repositories";
    let host = env.base.trim_start_matches("http://").to_owned();
    let user = |user_id: i64, login: &str| {
        env.store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: host.clone(),
                    user_id,
                },
                login,
            )
            .unwrap()
            .id
    };
    let (_, who) = env.get("/v1/hub/whoami", &alice).await;
    let alice_id = user(ALICE, who["owner"]["login"].as_str().unwrap());
    let bob_id = user(BOB, "bob");
    // Each fresh read needs its own token: one token gets its last answer back for 30 seconds.
    let fresh = |name: &str, read_only: bool| {
        mint_bound_agent_token(&*env.store, &alice_id, name, REPO, REPO_ID, 600, read_only)
            .unwrap()
            .1
    };

    // A forge that cannot answer is counted, never named, and the answer is not complete.
    env.fake.lock().unwrap().hub_reads_roles = false;
    let (status, body) = env.get(path, fresh("probe", false).expose()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["repositories"], json!([]));
    assert_eq!(
        (&body["unchecked"], &body["complete"]),
        (&json!(4), &json!(false))
    );
    env.fake.lock().unwrap().hub_reads_roles = true;

    let start = |owner: &str, name: &str, repository: (&str, i64), n: usize, session: &str| {
        let (bound, _, _) = mint_bound_agent_token(
            &*env.store,
            owner,
            name,
            repository.0,
            repository.1,
            600,
            false,
        )
        .unwrap();
        env.store
            .work_start(
                &bound,
                &WorkStart {
                    issue_choice: None,
                    work_id: None,
                    source: None,
                    request_key: Some(format!("cross-{n}")),
                    title: format!("Task {n}"),
                    description: None,
                    session: session.into(),
                    role: WorkRole::Lead,
                    scope: vec![format!("src/{n}.rs")],
                    branch: None,
                    worktree: None,
                    next_action: Some(format!("step {n}")),
                    depends_on: Vec::new(),
                    related_work: Vec::new(),
                    takeover_generation: None,
                    preflight: None,
                },
                unix_now(),
            )
            .unwrap();
        bound
    };
    // More of alice's Work in acme/widgets than one repository lists.
    for n in 0..11 {
        start(
            &alice_id,
            &format!("agent-{n}"),
            (REPO, REPO_ID),
            n,
            &format!("alice-{n}"),
        );
    }
    // Bob's Work in acme/docs, which alice can read, led by an older client whose label names
    // its host.
    let bob_token = start(
        &bob_id,
        "client-bobs-laptop-a1b2c3",
        ("acme/docs", 4343),
        99,
        "bob-session",
    );
    let digest = agentscm::hub::store::lower_hex_of(
        format!("agentscm.hub.session-view.v1\0{bob_token}\0bob-session").as_bytes(),
    )[..16]
        .to_owned();

    let reader = fresh("alice-reader", true);
    let mut first = Value::Null;
    for token in [alice.as_str(), reader.expose()] {
        let (status, body) = env.get(path, token).await;
        assert_eq!(status, 200, "{body}");
        let names: Vec<_> = body["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["repository"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["acme/docs", REPO], "{body}");
        assert_eq!(
            (
                &body["unchecked"],
                &body["complete"],
                &body["work_complete"]
            ),
            (&json!(0), &json!(true), &json!(true))
        );
        let docs = &body["repositories"][0];
        assert_eq!(docs["work_complete"], true);
        let theirs = &docs["work"][0];
        assert_eq!(theirs["title"], "Task 99");
        assert_eq!(theirs["owner_login"], "bob");
        assert_eq!(theirs["next_action"], "step 99");
        assert_eq!(theirs["scope"], json!(["src/99.rs"]));
        assert_eq!(
            (&theirs["blocked"], &theirs["waiting"]),
            (&json!(false), &json!(false))
        );
        assert_eq!(
            theirs["lead_session"], digest,
            "another owner's session is digested"
        );
        assert_eq!(
            theirs["lead_agent"], "client-host-a1b2c3",
            "its host name is masked"
        );
        let widgets = &body["repositories"][1];
        assert_eq!(widgets["work"].as_array().unwrap().len(), 10);
        assert_eq!(widgets["work_complete"], false);
        assert!(
            widgets["work"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["lead_session"].as_str().unwrap().starts_with("alice-")),
            "alice's own sessions stay as stored"
        );
        if first.is_null() {
            first = body;
        }
    }

    // Within the 90-second role cache a lost access is not asked again: the forge is not called
    // and the repository still lists; it drops out once the answer expires or alice signs out.
    let reads = {
        let mut fake = env.fake.lock().unwrap();
        fake.roles.remove(&("acme/docs".into(), ALICE));
        fake.levels.remove(&(4343, ALICE));
        fake.role_reads
    };
    let (_, body) = env.get(path, fresh("after-revoke", false).expose()).await;
    let names: Vec<_> = body["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["repository"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["acme/docs", REPO], "{body}");
    assert_eq!(env.fake.lock().unwrap().role_reads, reads);
    // The token that asked within the last 30 seconds gets its earlier answer back.
    let (_, again) = env.get(path, &alice).await;
    assert_eq!(again, first);

    // The MCP tool is the same read.
    let (status, reply) = env
        .call(
            &env.hub,
            Method::POST,
            "/mcp",
            Some(&alice),
            Some(json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"agentscm_work_repositories","arguments":{}}})),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    let (_, body) = env.get(path, &alice).await;
    assert_eq!(
        reply["result"]["structuredContent"]["repositories"], body["repositories"],
        "{reply}"
    );
}

/// More Work across repositories than one agent read lists in all: the top-level `work_complete`
/// says the overall cap cut a list, and the repositories past it list less.
#[tokio::test]
async fn the_agent_read_flags_the_overall_work_cap() {
    use agentscm::hub::work::{WorkRole, WorkStart};
    let env = Env::start("github").await;
    let host = env.base.trim_start_matches("http://").to_owned();
    let (_, device, _) = env.sign_in(ALICE, "laptop").await;
    let (status, minted) = env.mint(&device).await;
    assert_eq!(status, 201, "{minted}");
    let alice = minted["token"].as_str().unwrap().to_owned();
    let (_, who) = env.get("/v1/hub/whoami", &alice).await;
    let alice_id = env
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host,
                user_id: ALICE,
            },
            who["owner"]["login"].as_str().unwrap(),
        )
        .unwrap()
        .id;
    // Eleven repositories of ten Work each: 110, past the 100 one read lists.
    for r in 0..11_i64 {
        let (name, id) = (format!("acme/cap-{r:02}"), 5000 + r);
        env.connect_repo(&name, id, ALICE);
        let (bound, _, _) =
            mint_bound_agent_token(&*env.store, &alice_id, "cap-agent", &name, id, 600, false)
                .unwrap();
        for n in 0..10 {
            let request: WorkStart = serde_json::from_value(json!({
                "request_key": format!("cap-{r}-{n}"), "title": format!("Cap {r} {n}"),
                "session": format!("cap-{r}-{n}"), "scope": [format!("src/{n}.rs")]
            }))
            .unwrap();
            assert_eq!(request.role, WorkRole::Lead);
            env.store.work_start(&bound, &request, unix_now()).unwrap();
        }
    }
    let (status, body) = env.get("/v1/hub/standup/repositories", &alice).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["work_complete"], false, "the overall cap cut a list");
    let listed: Vec<usize> = body["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["repository"].as_str().unwrap().starts_with("acme/cap-"))
        .map(|row| row["work"].as_array().unwrap().len())
        .collect();
    assert_eq!(listed.iter().sum::<usize>(), 100);
    assert_eq!(listed.len(), 11);
    assert_eq!((listed[0], listed[10]), (10, 0));
    let last = body["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["repository"] == "acme/cap-10")
        .unwrap();
    assert_eq!(last["work_complete"], false);
}

#[tokio::test]
async fn a_web_sign_in_lists_the_repositories_it_can_read_now_with_counts_only() {
    let env = Env::start("github").await;
    env.connect_repo("acme/docs", 4343, ALICE);
    // Connected, but alice has no role on it.
    env.connect_repo("acme/secret", 4444, BOB);
    let cookie = web_sign_in(&env, ALICE).await;
    let path = "/v1/hub/session/repositories";
    let list = || with_cookie(&env.hub, Method::GET, path, &cookie, None);

    // An agent of alice's holds a file in acme/widgets.
    let (session, device, _) = env.sign_in(ALICE, "laptop").await;
    let (status, minted) = env.mint(&device).await;
    assert_eq!(status, 201, "{minted}");
    let intent = json!({"intent_id": format!("int_{}", "a".repeat(32)), "task": "t",
        "branch": null, "base": null, "machine": null, "paths": ["src/lib.rs"], "ttl_s": 600});
    let (status, announced) = env
        .call(
            &env.hub,
            Method::POST,
            &format!("/v1/hub/repos/{}/intents", REPO.replace('/', "%2F")),
            minted["token"].as_str(),
            Some(intent),
        )
        .await;
    assert_eq!(status, 200, "{announced}");

    // Same-named repositories on another host or provider must not disclose intent text.
    for (provider, host, marker) in [
        ("github", "other-forge.example", "b"),
        ("gitlab", env.base.trim_start_matches("http://"), "c"),
    ] {
        let foreign = env
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: provider.into(),
                    host: host.into(),
                    user_id: ALICE,
                },
                "foreign-alice",
            )
            .unwrap();
        let (foreign_id, _, _) = mint_bound_agent_token(
            &*env.store,
            &foreign.id,
            "foreign-agent",
            REPO,
            REPO_ID,
            600,
            false,
        )
        .unwrap();
        env.store
            .announce_intent(
                &foreign_id,
                &NewIntent {
                    id: format!("int_{}", marker.repeat(32)),
                    task: format!("private {provider} task"),
                    branch: None,
                    base: None,
                    machine: None,
                    paths: vec![format!("{marker}.rs")],
                    ttl_seconds: 600,
                },
                unix_now(),
            )
            .unwrap();
    }

    // A forge that cannot answer is counted, never named, and never shown as lost access.
    env.fake.lock().unwrap().hub_reads_roles = false;
    let (status, body) = list().await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["repositories"], json!([]));
    assert_eq!(body["unchecked"], 3);
    env.fake.lock().unwrap().hub_reads_roles = true;

    // Those failures were not kept: the next list asks the forge again.
    let reads = env.fake.lock().unwrap().role_reads;
    let (status, body) = list().await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(env.fake.lock().unwrap().role_reads, reads + 3);
    let names: Vec<_> = body["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["repository"].as_str().unwrap())
        .collect();
    // The one with activity first; the one alice cannot read is not named.
    assert_eq!(names, [REPO, "acme/docs"]);
    let widgets = &body["repositories"][0];
    assert_eq!(widgets["repository_id"], REPO_ID);
    // Each row names its forge, so a page tells GitHub from GitLab without other reads.
    let host = env.store.connections().unwrap()[0].host.clone();
    assert_eq!(
        (&widgets["provider"], &widgets["host"]),
        (&json!("github"), &json!(host))
    );
    assert_eq!(widgets["agents"], 1);
    assert_eq!(widgets["needs_you"], 0);
    assert_eq!(
        widgets["unfinished"],
        json!({"total": 0, "moving": 0, "stuck": 0})
    );
    assert!(widgets["last_activity_at"].as_i64().is_some());
    assert_eq!(
        widgets["complete"], true,
        "every attention producer is integrated and nothing limited this summary"
    );
    assert_eq!(body["repositories"][1]["last_activity_at"], Value::Null);
    assert_eq!(
        (&body["unchecked"], &body["complete"]),
        (&json!(0), &json!(true))
    );
    // Counts only: nothing a repository's own read token would hide.
    let mut keys: Vec<_> = widgets.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "agents",
            "complete",
            "coverage",
            "host",
            "last_activity_at",
            "needs_you",
            "provider",
            "read_only",
            "repository",
            "repository_id",
            "status_counts",
            "status_digest",
            "unfinished"
        ]
    );
    assert_eq!(widgets["read_only"], false);

    let (status, started) = env
        .call(
            &env.hub,
            Method::POST,
            "/v1/hub/standup/start",
            minted["token"].as_str(),
            Some(json!({
                "work_id":null,"source":null,"request_key":"tray-work-view",
                "title":"Review checkout state","session":"tray-test-session","role":"lead",
                "scope":["src/lib.rs"],"branch":null,"worktree":null,
                "next_action":"Check the diff","depends_on":[],"related_work":[],
                "takeover_generation":null
            })),
        )
        .await;
    assert_eq!(status, 200, "{started}");
    let (status, device_list) = env
        .call(
            &env.hub,
            Method::GET,
            "/v1/hub/device/repositories",
            Some(&device),
            None,
        )
        .await;
    assert_eq!(status, 200, "{device_list}");
    let marker_key = env
        .store
        .get_setting("device_attention_revision_key_v1")
        .unwrap()
        .unwrap();
    assert_eq!(marker_key.len(), 64);
    assert_eq!(
        env.store
            .get_or_create_setting("device_attention_revision_key_v1", "alternate", "system")
            .unwrap(),
        marker_key
    );
    assert!(!device_list.to_string().contains("private github task"));
    assert!(!device_list.to_string().contains("private gitlab task"));
    assert_eq!(device_list["repositories"][0]["repository"], REPO);
    assert_eq!(
        device_list["repositories"][0]["work"][0]["title"],
        "Review checkout state"
    );
    assert_eq!(
        device_list["repositories"][0]["work"][0]["next_action"],
        "Continue the reported work; activity is not independently observed."
    );
    assert_eq!(
        device_list["repositories"][0]["work"][0]["needs_you"],
        false
    );
    assert_eq!(device_list["repositories"][0]["status_counts"]["active"], 1);
    assert_eq!(device_list["repositories"][0]["work_complete"], false);
    assert_eq!(device_list["repositories"][0]["live_complete"], true);
    assert_eq!(
        device_list["repositories"][0]["attention_revision"],
        Value::Null
    );
    assert!(
        device_list["repositories"][0]["live"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["task"] == "t")
    );
    assert!(
        device_list["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["repository"] != "acme/secret")
    );
    let (status, _) = env
        .call(
            &env.hub,
            Method::GET,
            "/v1/hub/device/repositories",
            Some(&session),
            None,
        )
        .await;
    assert_eq!(status, 401);

    // Catalog counts, recipient and next action are exactly the shared person/token projection.
    let mut work = started["item"].clone();
    let mut previous_marker = None;
    for (operation, summary) in [
        ("catalog-handover-1", "Synthetic handover"),
        ("catalog-handover-2", "Synthetic updated handover"),
    ] {
        let (code, checkpoint) = env.call(&env.hub, Method::POST, "/v1/hub/standup/checkpoint", minted["token"].as_str(), Some(json!({
            "work_id":work["id"],"operation_id":operation,"generation":work["generation"],"session":"tray-test-session", "expected_updated_seq":work["updated_seq"],"kind":"handover","summary":summary
        }))).await;
        assert_eq!(code, 200, "{checkpoint}");
        work = checkpoint["item"].clone();
        let (code, catalog) = env
            .call(
                &env.hub,
                Method::GET,
                "/v1/hub/device/repositories",
                Some(&device),
                None,
            )
            .await;
        assert_eq!(code, 200, "{catalog}");
        let (_, status) = env
            .call(
                &env.hub,
                Method::GET,
                "/v1/hub/standup/status",
                minted["token"].as_str(),
                None,
            )
            .await;
        let row = catalog["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["repository"] == REPO)
            .unwrap();
        assert_eq!(row["status_counts"], status["counts"]);
        assert_eq!(row["status_digest"], status["digest"]);
        assert_eq!(row["work"][0]["state"], "awaiting_party");
        assert_eq!(row["work"][0]["recipient"], status["work"][0]["recipient"]);
        assert_eq!(row["work"][0]["next_action"], status["work"][0]["action"]);
        assert_eq!(row["work"][0]["needs_you"], false);
        assert_eq!(row["status_counts"]["needs_you"], 0);
        assert_eq!(row["attention_revision"], Value::Null);
        if let Some(previous) = &previous_marker {
            assert_eq!(
                &row["attention_revision"], previous,
                "routine handover updates do not create a personal obligation"
            );
        }
        previous_marker = Some(row["attention_revision"].clone());
        let (_, again) = env
            .call(
                &env.hub,
                Method::GET,
                "/v1/hub/device/repositories",
                Some(&device),
                None,
            )
            .await;
        let again = again["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["repository"] == REPO)
            .unwrap();
        assert_eq!(
            Some(again["attention_revision"].clone()),
            previous_marker,
            "poll time does not create new attention"
        );
    }

    // Only the web sign-in asks.
    let (status, refused) = env
        .call(&env.hub, Method::GET, path, Some(&session), None)
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("forbidden"))
    );

    // Within the TTL the list reuses those answers, the refused one too, so losing the role
    // shows only once they expire or alice signs in again.
    let reads = env.fake.lock().unwrap().role_reads;
    env.drop_role(ALICE);
    let (_, body) = list().await;
    assert_eq!(body["repositories"][0]["repository"], REPO);
    assert_eq!(body["repositories"].as_array().unwrap().len(), 2);
    assert_eq!(env.fake.lock().unwrap().role_reads, reads);

    // Another person's list never uses alice's answers.
    let bob = web_sign_in(&env, BOB).await;
    let (_, body) = with_cookie(&env.hub, Method::GET, path, &bob, None).await;
    assert!(
        body["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["repository"] == "acme/secret"),
        "{body}"
    );
    assert_eq!(env.fake.lock().unwrap().role_reads, reads + 3);

    // The detailed device list reuses the same cached answers, so the lost role still lists.
    let (status, device_after_role_loss) = env
        .call(
            &env.hub,
            Method::GET,
            "/v1/hub/device/repositories",
            Some(&device),
            None,
        )
        .await;
    assert_eq!(status, 200, "{device_after_role_loss}");
    assert_eq!(
        device_after_role_loss["repositories"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(env.fake.lock().unwrap().role_reads, reads + 3);

    // Signing out forgets them: losing the role leaves the repository out, with no refused
    // sign-in recorded.
    let (_, session) = with_cookie(&env.hub, Method::GET, "/v1/hub/session", &cookie, None).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let (status, _) = with_cookie(
        &env.hub,
        Method::DELETE,
        "/v1/hub/session",
        &cookie,
        Some(csrf),
    )
    .await;
    assert_eq!(status, 204);
    let cookie = web_sign_in(&env, ALICE).await;
    let (_, body) = with_cookie(&env.hub, Method::GET, path, &cookie, None).await;
    assert_eq!(body["repositories"][0]["repository"], "acme/docs");
    assert_eq!(body["repositories"].as_array().unwrap().len(), 1);
    let refusals = env.store.refusals(unix_now()).unwrap();
    assert!(!refusals.iter().any(|r| r.via == "session_repositories"));
}

#[tokio::test]
async fn web_read_tokens_cannot_cross_workspace_authentication_and_hub_sign_out_keeps_workspace() {
    let mut env = Env::start("github").await;
    env.workspace = Some(device_promise_workspace(&env, REPO_ID));
    env.hub = env.serve("").await;
    let hub_cookie = web_sign_in(&env, ALICE).await;
    let (_, hub_session) =
        with_cookie(&env.hub, Method::GET, "/v1/hub/session", &hub_cookie, None).await;
    let hub_csrf = hub_session["csrf_token"].as_str().unwrap();
    let (status, minted) = web_read_token(&env, &hub_cookie, Some(hub_csrf), REPO).await;
    assert_eq!(status, 201, "{minted}");
    let web_token = minted["token"].as_str().unwrap();

    let workspace_done = callback_for(&env, ALICE, "workspace").await;
    assert_eq!(workspace_done.status(), 302);
    let workspace_cookie = set_cookie(&workspace_done, "__Host-agentscm_workspace").unwrap();
    let workspace_get = |path: &str, bearer: Option<&str>| {
        let mut request = env.http.get(format!("{}{path}", env.hub));
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        } else {
            request = request.header(
                header::COOKIE,
                format!("__Host-agentscm_workspace={workspace_cookie}"),
            );
        }
        request
    };
    let owner = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == ALICE)
        .unwrap();
    let admission = PromiseRepositoryAdmissionV1 {
        repository_id: REPO.into(),
        forge: Some(PromiseForgeAdmissionV1 {
            provider: "github".into(),
            host: env.base.trim_start_matches("http://").into(),
            forge_repository_id: REPO_ID,
        }),
        native_repository_ids: vec![],
    };
    let created = env.store.promise_create(
        "device-promise-workspace", &owner.id,
        &PromiseCreateV1 {
            operation_id: "web_auth_boundary".into(),
            expected_admissions_digest: admissions_digest("device-promise-workspace", std::slice::from_ref(&admission)).unwrap(),
            definition: serde_json::from_value(json!({
                "original_request":"Keep customer settings.",
                "commitment":"Existing settings persist.",
                "criteria":[{"id":"settings","text":"Settings persist.","evidence_requirements":[]}],
                "constraints":[],
                "repositories":[{"repository_id":REPO,"relevance":"Settings source."}]
            })).unwrap(),
        }, &[admission], unix_now(),
    ).unwrap();
    let id = created.reference.promise_id;
    let paths = [
        "/v1/hub/workspace/promises".to_owned(),
        format!("/v1/hub/workspace/promises/{id}"),
        format!("/v1/hub/workspace/promises/{id}/history"),
    ];
    let (_, ordinary, _) =
        mint_bound_agent_token(&*env.store, &owner.id, "reader", REPO, REPO_ID, 1800, true)
            .unwrap();
    let (_, legacy, _) =
        mint_bound_agent_token(&*env.store, &owner.id, "hub-web", REPO, REPO_ID, 1800, true)
            .unwrap();
    for path in &paths {
        assert_eq!(
            workspace_get(path, Some(web_token))
                .send()
                .await
                .unwrap()
                .status(),
            401,
            "{path}"
        );
        assert_eq!(
            workspace_get(path, Some(legacy.expose()))
                .send()
                .await
                .unwrap()
                .status(),
            401,
            "{path}"
        );
        assert_eq!(
            workspace_get(path, Some(ordinary.expose()))
                .send()
                .await
                .unwrap()
                .status(),
            200,
            "{path}"
        );
        assert_eq!(
            workspace_get(path, None).send().await.unwrap().status(),
            200,
            "{path}"
        );
    }
    assert_eq!(env.get("/v1/hub/whoami", web_token).await.0, 200);
    for token in [web_token, legacy.expose()] {
        let (status, reply) = env
            .call(
                &env.hub,
                Method::POST,
                "/mcp",
                Some(token),
                Some(json!({
                    "jsonrpc":"2.0", "id":1, "method":"tools/call",
                    "params":{"name":"agentscm_promises_list","arguments":{}}
                })),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["result"]["isError"], true, "{reply}");
    }

    assert_eq!(
        with_cookie(
            &env.hub,
            Method::DELETE,
            "/v1/hub/session",
            &hub_cookie,
            Some(hub_csrf)
        )
        .await
        .0,
        204
    );
    assert_eq!(
        with_cookie(&env.hub, Method::GET, "/v1/hub/session", &hub_cookie, None)
            .await
            .0,
        401
    );
    assert_eq!(
        workspace_get("/v1/hub/workspace/session", None)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        workspace_get(&paths[0], None)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn web_value_metrics_resolves_work_only_within_readable_repositories() {
    use agentscm::hub::work::{WorkRole, WorkStart};
    let env = Env::start("github").await;
    env.connect_repo("acme/docs", 4343, ALICE);
    env.connect_repo("acme/secret", 4444, BOB);
    {
        let mut fake = env.fake.lock().unwrap();
        fake.roles
            .insert(("acme/docs".into(), ALICE), ("write", "write"));
        fake.roles
            .insert(("acme/secret".into(), BOB), ("write", "write"));
    }
    let cookie = web_sign_in(&env, ALICE).await;
    let host = env.base.trim_start_matches("http://");
    let create = |user_id, login: &str, repository: &str, repository_id, key: &str, title: &str| {
        let owner = env
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: host.into(),
                    user_id,
                },
                login,
            )
            .unwrap();
        let (token, secret, _) = mint_bound_agent_token(
            &*env.store,
            &owner.id,
            "metrics-fixture",
            repository,
            repository_id,
            600,
            false,
        )
        .unwrap();
        let item = env
            .store
            .work_start(
                &token,
                &WorkStart {
                    issue_choice: None,
                    work_id: None,
                    source: None,
                    request_key: Some(key.into()),
                    title: title.into(),
                    description: None,
                    session: key.into(),
                    role: WorkRole::Lead,
                    scope: vec![format!("src/{key}.rs")],
                    branch: None,
                    worktree: None,
                    next_action: None,
                    depends_on: vec![],
                    related_work: vec![],
                    takeover_generation: None,
                    preflight: None,
                },
                unix_now(),
            )
            .unwrap()
            .item;
        (item, secret.expose().to_owned())
    };
    let (other, _) = create(
        ALICE,
        "alice",
        REPO,
        REPO_ID,
        "other-visible",
        "Other visible Work",
    );
    let (target, target_secret) = create(
        ALICE,
        "alice",
        "acme/docs",
        4343,
        "docs-target",
        "Visible docs target",
    );
    let (hidden, _) = create(
        BOB,
        "bob",
        "acme/secret",
        4444,
        "hidden-target",
        "Private target sentinel",
    );
    let path = format!("/v1/hub/session/value-metrics?work_id={}", target.id);
    let (status, metrics) = with_cookie(&env.hub, Method::GET, &path, &cookie, None).await;
    assert_eq!(status, 200, "{metrics}");
    let scopes = metrics["scope"].as_array().unwrap();
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0]["repository"], "acme/docs");
    let serialized = metrics.to_string();
    for marker in [
        other.id.as_str(),
        hidden.id.as_str(),
        "acme/secret",
        "Private target sentinel",
    ] {
        assert!(!serialized.contains(marker), "{serialized}");
    }
    let unknown_path = "/v1/hub/session/value-metrics?work_id=wrk_ffffffffffffffffffffffffffffffff";
    let (unknown_status, unknown) =
        with_cookie(&env.hub, Method::GET, unknown_path, &cookie, None).await;
    let (hidden_status, inaccessible) = with_cookie(
        &env.hub,
        Method::GET,
        &format!("/v1/hub/session/value-metrics?work_id={}", hidden.id),
        &cookie,
        None,
    )
    .await;
    assert_eq!(unknown_status, 404);
    assert_eq!(hidden_status, 404);
    assert_eq!(unknown, inaccessible);
    let (status, _) = env
        .call(
            &env.hub,
            Method::GET,
            &format!("/v1/hub/standup/value-metrics?work_id={}", other.id),
            Some(&target_secret),
            None,
        )
        .await;
    assert_eq!(status, 400);
    env.fake
        .lock()
        .unwrap()
        .roles
        .remove(&("acme/docs".into(), ALICE));
    // The role answer is kept for 90 seconds; signing out forgets it, so the next sign-in asks.
    let (_, session) = with_cookie(&env.hub, Method::GET, "/v1/hub/session", &cookie, None).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let (status, _) = with_cookie(
        &env.hub,
        Method::DELETE,
        "/v1/hub/session",
        &cookie,
        Some(csrf),
    )
    .await;
    assert_eq!(status, 204);
    let cookie = web_sign_in(&env, ALICE).await;
    let (revoked_status, revoked) = with_cookie(&env.hub, Method::GET, &path, &cookie, None).await;
    assert_eq!(revoked_status, 404);
    assert_eq!(revoked, unknown);
}

const RECOVERY: &str = "/v1/hub/session/work/recovery";
const RECOVERY_PREVIEW: &str = "/v1/hub/session/work/recovery/preview";
const TRANSITIONS: &str = "/v1/hub/session/work/transitions";

#[tokio::test]
async fn an_owner_recovers_a_dead_sessions_work_from_the_hub_through_the_shared_service() {
    let env = Env::start("github").await;
    let (alice, bob) = (page(&env, ALICE).await, page(&env, BOB).await);
    let now = unix_now();
    let (lead, _) = agent_for(&env, &alice.id, "claude-code", false);
    let mut start = work("dead", "dead-session");
    start.scope = vec!["src/dead.rs".into()];
    let item = env.store.work_start(&lead, &start, now).unwrap().item;
    // The agent and its token are gone.
    env.store.revoke_agent_token(&lead).unwrap();

    let query = json!({"repository": REPO, "work_id": item.id});
    let (status, _) = page_post(&env, &alice, false, RECOVERY, query.clone()).await;
    assert_eq!(status, 403);
    let (status, page) = page_post(&env, &alice, true, RECOVERY, query.clone()).await;
    assert_eq!(status, 200, "{page}");
    let entry = &page["work"][0];
    assert_eq!(entry["standing"], "owner");
    let permitted: Vec<&str> = entry["remedies"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|remedy| remedy["permitted"] == true)
        .map(|remedy| remedy["remedy"].as_str().unwrap())
        .collect();
    assert_eq!(
        permitted,
        ["abandon", "complete", "release", "retain", "supersede"]
    );
    // A person never leads, so a resume is refused with what to do instead.
    let resume = entry["remedies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|remedy| remedy["remedy"] == "resume")
        .unwrap();
    assert_eq!(resume["refusal"]["code"], "needs_session");

    let (status, preview) = page_post(
        &env,
        &alice,
        true,
        RECOVERY_PREVIEW,
        json!({"repository": REPO, "work_id": item.id, "remedy": "release"}),
    )
    .await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["apply"]["route"], "/v1/hub/session/work/transition");
    assert!(preview["apply"]["tool"].is_null());
    assert_eq!(preview["effects"]["freed_paths"], json!(["src/dead.rs"]));
    let batch = json!({"repository": REPO, "items": [{"work_id": item.id,
        "operation_id": "hub-release", "generation": preview["generation"],
        "expected_updated_seq": preview["expected_updated_seq"], "transition": "release",
        "reason": "its agent is gone"}]});

    // Another signed-in person sees no remedy and cannot apply one.
    let (status, theirs) = page_post(&env, &bob, true, RECOVERY, query).await;
    assert_eq!(status, 200, "{theirs}");
    assert_eq!(theirs["work"][0]["standing"], "other");
    assert!(
        theirs["work"][0]["remedies"]
            .as_array()
            .unwrap()
            .iter()
            .all(|remedy| remedy["permitted"] == false)
    );
    let (status, refused) = page_post(&env, &bob, true, TRANSITIONS, batch.clone()).await;
    assert_eq!(status, 200, "{refused}");
    assert_eq!(refused["items"][0]["outcome"], "refused");
    assert_eq!(refused["items"][0]["code"], "unauthorized");

    let (status, _) = page_post(&env, &alice, false, TRANSITIONS, batch.clone()).await;
    assert_eq!(status, 403);
    let (status, applied) = page_post(&env, &alice, true, TRANSITIONS, batch.clone()).await;
    assert_eq!(status, 200, "{applied}");
    assert_eq!(applied["items"][0]["outcome"], "applied");
    assert_eq!(applied["items"][0]["result"]["item"]["state"], "paused");
    let (_, again) = page_post(&env, &alice, true, TRANSITIONS, batch).await;
    assert_eq!(again["items"][0]["outcome"], "replayed");
}

/// #293: a refused agent write reaches the operator, with what to run, and no one else's badge.
#[tokio::test]
async fn a_capacity_refusal_reaches_the_operator_with_what_to_run() {
    use agentscm::hub::coordination::MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN;
    let env = Env::start("github").await;
    let (alice, bob) = (page(&env, ALICE).await, page(&env, BOB).await);
    let (lead, lead_secret) = agent_for(&env, &alice.id, "claude-code", false);
    let (_, asker) = agent_for(&env, &bob.id, "codex", false);
    env.store
        .work_start(&lead, &work("billing", "alice-session"), unix_now())
        .unwrap();
    let (_, health) = env
        .call(&env.hub, Method::GET, "/v1/hub/health", None, None)
        .await;
    assert_eq!(health["capacity"]["recent_limits"], json!(0), "{health}");
    // Each overlapping start opens a request, until the per-token cap refuses one.
    let start = |n: i64| {
        json!({"request_key": format!("rounding-{n}"), "title": format!("Rounding {n}"),
            "session": "bob-session", "scope": ["src/billing.rs"]})
    };
    for n in 0..MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN {
        let (status, reply) = env
            .call(
                &env.hub,
                Method::POST,
                "/v1/hub/standup/start",
                Some(&asker),
                Some(start(n)),
            )
            .await;
        assert_ne!(status, 503, "{reply}");
    }
    let (status, refused) = env
        .call(
            &env.hub,
            Method::POST,
            "/v1/hub/standup/start",
            Some(&asker),
            Some(start(MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN)),
        )
        .await;
    assert_eq!(status, 503, "{refused}");
    crate::assert_hub_error!(
        refused,
        json!({"error": "work_storage_capacity", "limit": "coordination_requests"})
    );

    // The admin sees which repository and limit, counted, with the remedy.
    let (admin, _, _) = env.sign_in(ADMIN, "admin's laptop").await;
    let (status, list) = env.get("/v1/hub/admin/capacity-refusals", &admin).await;
    assert_eq!(status, 200, "{list}");
    let rows = list["refusals"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{list}");
    assert_eq!(
        (
            &rows[0]["repository"],
            &rows[0]["repository_id"],
            &rows[0]["limit"],
            &rows[0]["count"],
            &rows[0]["recent"]
        ),
        (
            &json!(REPO),
            &json!(REPO_ID),
            &json!("coordination_requests"),
            &json!(1),
            &json!(true)
        )
    );
    assert!(
        rows[0]["remedy"]
            .as_str()
            .unwrap()
            .starts_with("Resolve or withdraw open coordination requests"),
        "{list}"
    );
    // Not for anyone else, nor for a computer credential or an agent.
    let (person, device, _) = env.sign_in(ALICE, "alice's laptop").await;
    for credential in [&person, &device, &asker] {
        assert_ne!(
            env.get("/v1/hub/admin/capacity-refusals", credential)
                .await
                .0,
            200
        );
    }
    // The public health counts it without naming the repository.
    let (_, health) = env
        .call(&env.hub, Method::GET, "/v1/hub/health", None, None)
        .await;
    assert_eq!(health["capacity"]["recent_limits"], json!(1), "{health}");
    assert!(health["capacity"]["last_refused_at"].is_i64(), "{health}");
    assert!(!health.to_string().contains(REPO));
    // Shared status marks coverage incomplete for the repository; it is nobody's action.
    for token in [&asker, &lead_secret] {
        let (status, facts) = env.get("/v1/hub/standup/status", token).await;
        assert_eq!(status, 200, "{facts}");
        assert!(
            facts["limits"]
                .as_array()
                .unwrap()
                .contains(&json!("capacity_refused:coordination_requests")),
            "{facts}"
        );
        assert_eq!(facts["complete"], json!(false));
    }
}

const PICKUPS: &str = "/v1/hub/session/pickups";

/// The person's record `id`, read through the page route.
async fn pickup(env: &Env, page: &Page, id: &Value) -> Value {
    pickups(env, page)
        .await
        .into_iter()
        .find(|p| p["id"] == *id)
        .unwrap()
}

async fn pickups(env: &Env, page: &Page) -> Vec<Value> {
    let (status, body) = with_cookie(
        &env.hub,
        Method::GET,
        &format!("{PICKUPS}?repository=acme%2Fwidgets"),
        &page.cookie,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["pickups"].as_array().unwrap().clone()
}

#[tokio::test]
async fn a_hand_off_is_picked_up_only_by_its_own_persons_agent_start() {
    let env = Env::start("github").await;
    let (alice, bob) = (page(&env, ALICE).await, page(&env, BOB).await);
    let (alice_agent, _) = agent_for(&env, &alice.id, "claude-code", false);
    let (bob_agent, _) = agent_for(&env, &bob.id, "codex", false);
    let body = json!({"repository": REPO, "title": "Synthetic task"});

    // A page without its CSRF token cannot create one.
    let (status, refused) = page_post(&env, &alice, false, PICKUPS, body.clone()).await;
    assert_eq!((status, refused["error"].as_str()), (403, Some("csrf")));
    let (status, task) = page_post(&env, &alice, true, PICKUPS, body.clone()).await;
    assert_eq!(status, 200, "{task}");
    assert_eq!(task["state"], "waiting");
    let key = task["request_key"].as_str().unwrap().to_owned();
    assert_eq!(key, task["id"].as_str().unwrap());

    // Another person's agent starting with the same key does not pick it up.
    env.store
        .work_start(&bob_agent, &work(&key, "bob-session"), unix_now())
        .unwrap();
    assert_eq!(pickup(&env, &alice, &task["id"]).await["state"], "waiting");
    assert!(pickups(&env, &bob).await.is_empty());

    // Alice's own agent does.
    let mut start = work(&key, "alice-session");
    start.scope = vec!["src/pickup.rs".into()];
    let item = env
        .store
        .work_start(&alice_agent, &start, unix_now())
        .unwrap()
        .item;
    let picked = pickup(&env, &alice, &task["id"]).await;
    assert_eq!(picked["state"], "picked_up", "{picked}");
    assert_eq!(picked["picked_up"]["agent"], "claude-code");
    assert_eq!(picked["picked_up"]["session"], "alice-session");
    assert_eq!(picked["picked_up"]["work_id"], item.id.as_str());

    // A Work hand-off is picked up by a start that resumes that Work.
    let (_, handed) = page_post(
        &env,
        &alice,
        true,
        PICKUPS,
        json!({"repository": REPO, "work_id": item.id}),
    )
    .await;
    assert_eq!(handed["request_key"], Value::Null);
    let mut resume = start.clone();
    (resume.request_key, resume.work_id) = (None, Some(item.id.clone()));
    env.store
        .work_start(&alice_agent, &resume, unix_now())
        .unwrap();
    assert_eq!(
        pickup(&env, &alice, &handed["id"]).await["state"],
        "picked_up"
    );

    // An abandoned hand-off stays abandoned; only its owner can abandon it.
    let (_, dropped) = page_post(&env, &alice, true, PICKUPS, body.clone()).await;
    let abandon = json!({"repository": REPO, "id": dropped["id"]});
    let path = "/v1/hub/session/pickups/abandon";
    assert_eq!(
        page_post(&env, &bob, true, path, abandon.clone()).await.0,
        404
    );
    let (status, abandoned) = page_post(&env, &alice, true, path, abandon).await;
    assert_eq!(
        (status, abandoned["state"].as_str()),
        (200, Some("abandoned"))
    );
    let mut late = work(dropped["id"].as_str().unwrap(), "late-session");
    late.scope = vec!["src/late.rs".into()];
    env.store
        .work_start(&alice_agent, &late, unix_now())
        .unwrap();
    assert_eq!(
        pickup(&env, &alice, &dropped["id"]).await["state"],
        "abandoned"
    );

    // One nobody picked up in time expires and is not picked up afterwards.
    let old = unix_now() - agentscm::hub::pickup::PICKUP_TTL_SECONDS - 1;
    let expired = env
        .store
        .pickup_create(&alice.id, REPO, None, None, old)
        .unwrap();
    let mut slow = work(&expired.id, "slow-session");
    slow.scope = vec!["src/slow.rs".into()];
    env.store
        .work_start(&alice_agent, &slow, unix_now())
        .unwrap();
    let found = pickup(&env, &alice, &json!(expired.id)).await;
    assert_eq!(found["state"], "expired");
}

const SKILL_STATUS: &str = "/v1/hub/session/skill-forge-status";

/// A PUT of the skill-status setting from a web page; `csrf` is withheld to play another site.
async fn put_skill_status(env: &Env, page: &Page, csrf: bool, enabled: bool) -> (u16, Value) {
    let mut request = browser()
        .put(format!("{}{SKILL_STATUS}", env.hub))
        .header(
            header::COOKIE,
            format!("__Host-agentscm_session={}", page.cookie),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"repository": REPO, "enabled": enabled}).to_string());
    if csrf {
        request = request.header("x-csrf-token", &page.csrf);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test]
async fn only_a_repository_or_hub_admin_sets_the_skill_status() {
    let env = Env::start("github").await;
    let (alice, admin) = (page(&env, ALICE).await, page(&env, ADMIN).await);
    let read = |page: &Page| {
        let (hub, cookie) = (env.hub.clone(), page.cookie.clone());
        let path = format!("{SKILL_STATUS}?repository={REPO}");
        async move { with_cookie(&hub, Method::GET, &path, &cookie, None).await }
    };
    // Off by default; a writer sees it but may not change it.
    let (status, seen) = read(&alice).await;
    assert_eq!(status, 200, "{seen}");
    assert_eq!(
        (seen["enabled"].clone(), seen["can_change"].clone()),
        (json!(false), json!(false))
    );
    assert_eq!(put_skill_status(&env, &alice, true, true).await.0, 403);
    assert_eq!(read(&alice).await.1["enabled"], false);

    // A Hub admin sets and unsets it; without the page's CSRF token nothing changes.
    assert_eq!(put_skill_status(&env, &admin, false, true).await.0, 403);
    let (status, set) = put_skill_status(&env, &admin, true, true).await;
    assert_eq!(
        (status, set["enabled"].clone()),
        (200, json!(true)),
        "{set}"
    );
    assert_eq!(read(&alice).await.1["enabled"], true);
    assert_eq!(
        put_skill_status(&env, &admin, true, false).await.1["enabled"],
        false
    );

    // A repository admin may change it too.
    env.role(ALICE, "admin", "admin", 50);
    let (status, set) = put_skill_status(&env, &alice, true, true).await;
    assert_eq!(
        (status, set["can_change"].clone()),
        (200, json!(true)),
        "{set}"
    );

    // No agent or read-only token writes it.
    for read_only in [false, true] {
        let (_, secret) = agent_for(&env, &alice.id, "skill-status", read_only);
        let response = browser()
            .put(format!("{}{SKILL_STATUS}", env.hub))
            .bearer_auth(&secret)
            .header(header::CONTENT_TYPE, "application/json")
            .body(json!({"repository": REPO, "enabled": false}).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401, "read_only={read_only}");
    }
    assert_eq!(read(&alice).await.1["enabled"], true);
}

const DISMISSALS: &str = "/v1/hub/session/dismissals";

/// A PUT from a web page with its CSRF token.
async fn page_put(env: &Env, page: &Page, body: Value) -> (u16, Value) {
    let response = browser()
        .put(format!("{}{DISMISSALS}", env.hub))
        .header(
            header::COOKIE,
            format!("__Host-agentscm_session={}", page.cookie),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-csrf-token", &page.csrf)
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_person_dismisses_and_snoozes_their_own_attention_items() {
    let env = Env::start("github").await;
    let (alice, bob) = (page(&env, ALICE).await, page(&env, BOB).await);
    let list = format!("{DISMISSALS}?repository={REPO}");
    let read = |page: &Page| {
        let (hub, cookie, list) = (env.hub.clone(), page.cookie.clone(), list.clone());
        async move { with_cookie(&hub, Method::GET, &list, &cookie, None).await }
    };
    assert_eq!(read(&alice).await.1, json!({"dismissals": []}));

    let dismiss = json!({"repository": REPO, "key": "stale:wrk_1", "fingerprint": "abc"});
    let (status, page) = page_put(&env, &alice, dismiss.clone()).await;
    assert_eq!(status, 200, "{page}");
    let until = unix_now() + 86_400;
    let snooze =
        json!({"repository": REPO, "key": "pr:12", "fingerprint": "head1", "until": until});
    assert_eq!(page_put(&env, &alice, snooze).await.0, 200);
    // A second dismissal of the same key replaces the first.
    let mut changed = dismiss.clone();
    changed["fingerprint"] = json!("def");
    let (_, page) = page_put(&env, &alice, changed).await;
    let rows = page["dismissals"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{page}");
    assert_eq!(rows[1]["key"], "stale:wrk_1");
    assert_eq!(rows[1]["fingerprint"], "def");
    assert_eq!(rows[1]["until"], Value::Null);
    assert_eq!(rows[0]["until"], json!(until));
    assert_eq!(read(&alice).await.1, page);

    // They are Alice's own; Bob reads none, and a person who cannot read the repository is refused.
    assert_eq!(read(&bob).await.1, json!({"dismissals": []}));
    // Without the page's CSRF token, signed out, or with bad values, nothing changes.
    let remove = format!("{DISMISSALS}?repository={REPO}&key=pr%3A12");
    assert_eq!(
        with_cookie(&env.hub, Method::DELETE, &remove, &alice.cookie, None)
            .await
            .0,
        403
    );
    assert_eq!(
        with_cookie(&env.hub, Method::GET, &list, "nope", None)
            .await
            .0,
        401
    );
    for bad in [
        json!({"repository": REPO, "key": "", "fingerprint": "f"}),
        json!({"repository": REPO, "key": "k".repeat(201), "fingerprint": "f"}),
        json!({"repository": REPO, "key": "k", "fingerprint": "f".repeat(129)}),
        json!({"repository": REPO, "key": "k", "fingerprint": "f", "until": 1}),
        json!({"repository": REPO, "key": "k", "fingerprint": "f", "extra": 1}),
    ] {
        assert_eq!(page_put(&env, &alice, bad.clone()).await.0, 400, "{bad}");
    }

    let (status, page) = with_cookie(
        &env.hub,
        Method::DELETE,
        &remove,
        &alice.cookie,
        Some(&alice.csrf),
    )
    .await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["dismissals"].as_array().unwrap().len(), 1);
    assert_eq!(page["dismissals"][0]["key"], "stale:wrk_1");

    // A lost role reads from the 90-second role answer, without asking the forge, until it
    // expires or bob signs out (and signing in again checks the role).
    let reads = env.fake.lock().unwrap().role_reads;
    env.drop_role(BOB);
    assert_eq!(read(&bob).await.0, 200);
    assert_eq!(env.fake.lock().unwrap().role_reads, reads);
    let (status, _) = with_cookie(
        &env.hub,
        Method::DELETE,
        "/v1/hub/session",
        &bob.cookie,
        Some(&bob.csrf),
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(read(&bob).await.0, 401);
    // With nothing kept, a person without the role is refused and the forge was asked.
    env.role(BOB, "read", "read", 20);
    let bob = self::page(&env, BOB).await;
    env.drop_role(BOB);
    let reads = env.fake.lock().unwrap().role_reads;
    let (status, refused) = read(&bob).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("insufficient_role"))
    );
    assert_eq!(env.fake.lock().unwrap().role_reads, reads + 1);
}

const PERMISSIONS: &str = "/v1/hub/session/permissions";
const PERMISSION_ANSWER: &str = "/v1/hub/session/permissions/answer";

/// A person allows or denies a permission prompt their own agent's session waits at: on the web
/// sign-in only with its CSRF token (a connected app is in `hub_app_prompts`), only for their own
/// active agent, once, and only while the session still waits at that prompt.
#[tokio::test]
async fn a_web_session_answers_a_permission_prompt_only_for_its_own_agent() {
    let env = Env::start("github").await;
    let (alice, bob) = (page(&env, ALICE).await, page(&env, BOB).await);
    let (token, secret) = agent_for(&env, &alice.id, "claude-code", false);
    let session = "11111111-2222-3333-4444-555555555555";
    // What the session's hooks send: the prompt, the host's notice that it is on screen, the
    // registration, and the tool call that follows an answer in the session.
    let agent = |path: &'static str, body: Value| {
        env.call(&env.hub, Method::POST, path, Some(&secret), Some(body))
    };
    let prompt =
        json!({"hook_event_name": "PermissionRequest", "session_id": session, "tool_name": "Bash"});
    let shown = json!({"hook_event_name": "Notification", "session_id": session,
        "notification_type": "permission_prompt"});
    let ran = json!({"hook_event_name": "PostToolUse", "session_id": session, "tool_name": "Bash"});
    let register = |hash: &str, complete: bool| {
        json!({"session": session, "tool": "Bash", "display": "cargo publish",
            "summary": "run a shell command: Publish the crate", "folder": "widgets",
            "complete": complete, "input_hash": hash.repeat(64)})
    };
    let answer = |id: &Value, decision: &str| {
        json!({"repository": REPO,
            "request_id": id, "decision": decision})
    };

    assert_eq!(agent("/v1/hub/hook/claude", prompt.clone()).await.0, 200);
    let (status, made) = agent("/v1/hub/permission/register", register("a", true)).await;
    assert_eq!(
        (status, &made["state"]),
        (200, &json!("unconfirmed")),
        "{made}"
    );
    let id = made["request_id"].clone();
    // Until the host says a person can see the prompt, there is nothing to answer.
    let early = page_post(&env, &alice, true, PERMISSION_ANSWER, answer(&id, "deny")).await;
    assert_eq!(early.0, 404, "{}", early.1);
    assert_eq!(agent("/v1/hub/hook/claude", shown.clone()).await.0, 200);

    // Signed out, a cookie without its CSRF token, another person and an unknown decision.
    let (status, _) = env
        .call(
            &env.hub,
            Method::POST,
            PERMISSION_ANSWER,
            None,
            Some(answer(&id, "allow")),
        )
        .await;
    assert_eq!(status, 401);
    let (status, refused) =
        page_post(&env, &alice, false, PERMISSION_ANSWER, answer(&id, "allow")).await;
    assert_eq!((status, refused["error"].as_str()), (403, Some("csrf")));
    let (status, refused) =
        page_post(&env, &bob, true, PERMISSION_ANSWER, answer(&id, "allow")).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_your_agent"))
    );
    let always = page_post(&env, &alice, true, PERMISSION_ANSWER, answer(&id, "always")).await;
    assert_eq!(always.0, 400);
    // The list is the person's own, and it is where they answer: the request is on it as soon
    // as the host has confirmed the prompt, within the wait's first minute.
    let list = format!("{PERMISSIONS}?repository={REPO}");
    let (status, listed) = with_cookie(&env.hub, Method::GET, &list, &alice.cookie, None).await;
    assert_eq!(status, 200);
    assert_eq!(
        (
            &listed["requests"][0]["request_id"],
            &listed["requests"][0]["state"]
        ),
        (&id, &json!("pending"))
    );
    let theirs = with_cookie(&env.hub, Method::GET, &list, &bob.cookie, None).await;
    assert_eq!(theirs, (200, json!({"requests": []})));

    // Alice allows it once: the hook's poll hears exactly that, and the answer cannot be changed.
    let (status, done) =
        page_post(&env, &alice, true, PERMISSION_ANSWER, answer(&id, "allow")).await;
    assert_eq!(
        (status, &done),
        (200, &json!({"state": "answered", "decision": "allow"}))
    );
    let poll = json!({"request_id": id, "input_hash": "a".repeat(64), "wait_seconds": 0});
    let (_, polled) = agent("/v1/hub/permission/poll", poll).await;
    assert_eq!(
        (&polled["state"], &polled["decision"]),
        (&json!("answered"), &json!("allow"))
    );
    let again = page_post(&env, &alice, true, PERMISSION_ANSWER, answer(&id, "allow")).await;
    assert_eq!(again.0, 200);
    let (status, refused) =
        page_post(&env, &alice, true, PERMISSION_ANSWER, answer(&id, "deny")).await;
    assert_eq!(
        (status, refused["reason"].as_str()),
        (409, Some("already_answered"))
    );

    // The tool ran. The next prompt's request says what of it is not shown, and can be allowed
    // all the same.
    agent("/v1/hub/hook/claude", ran.clone()).await;
    agent("/v1/hub/hook/claude", prompt.clone()).await;
    let mut partial = register("b", true);
    partial.as_object_mut().unwrap().remove("complete");
    partial["missing"] = json!({"cut_from": 31_400, "redacted": 2, "unshown": ["sandbox"]});
    let (_, made) = agent("/v1/hub/permission/register", partial).await;
    let cut = made["request_id"].clone();
    agent("/v1/hub/hook/claude", shown.clone()).await;
    let allowed = page_post(&env, &alice, true, PERMISSION_ANSWER, answer(&cut, "allow")).await;
    assert_eq!(allowed.0, 200, "{}", allowed.1);

    // An older hook says only that its request is incomplete. Once the session has moved on, an
    // answer says so.
    agent("/v1/hub/hook/claude", ran.clone()).await;
    agent("/v1/hub/hook/claude", prompt.clone()).await;
    let (_, made) = agent("/v1/hub/permission/register", register("d", false)).await;
    let late = made["request_id"].clone();
    agent("/v1/hub/hook/claude", shown.clone()).await;
    agent("/v1/hub/hook/claude", ran.clone()).await;
    let (status, refused) =
        page_post(&env, &alice, true, PERMISSION_ANSWER, answer(&late, "deny")).await;
    assert_eq!(
        (status, refused["reason"].as_str()),
        (409, Some("answered_in_session"))
    );

    // A hook that offers one of the session's own choices and passes a typed reason on: what
    // the person picks or types is what that hook's poll hears, and nothing else is added.
    let offering = |hash: &str| {
        let mut body = register(hash, true);
        body["takes_reason"] = json!(true);
        body["choices"] = json!([{"label": "Allow, and do not ask again for this",
            "saves": "the rule Bash(cargo publish), for this session only"}]);
        body
    };
    let heard = |id: &Value, hash: &str| {
        let poll = json!({"request_id": id, "input_hash": hash.repeat(64), "wait_seconds": 0});
        agent("/v1/hub/permission/poll", poll)
    };
    agent("/v1/hub/hook/claude", prompt.clone()).await;
    let (_, made) = agent("/v1/hub/permission/register", offering("e")).await;
    let chosen = made["request_id"].clone();
    agent("/v1/hub/hook/claude", shown.clone()).await;
    let mut pick = answer(&chosen, "allow");
    pick["reason"] = json!("because");
    let unfit = page_post(&env, &alice, true, PERMISSION_ANSWER, pick.clone()).await;
    assert_eq!(unfit.0, 400, "{}", unfit.1);
    pick.as_object_mut().unwrap().remove("reason");
    pick["choice"] = json!("0");
    let picked = page_post(&env, &alice, true, PERMISSION_ANSWER, pick).await;
    assert_eq!(picked.0, 200, "{}", picked.1);
    let (_, polled) = heard(&chosen, "e").await;
    assert_eq!(
        (&polled["decision"], &polled["choice"], polled.get("reason")),
        (&json!("allow"), &json!(0), None)
    );
    agent("/v1/hub/hook/claude", ran.clone()).await;
    agent("/v1/hub/hook/claude", prompt.clone()).await;
    let (_, made) = agent("/v1/hub/permission/register", offering("f")).await;
    let denied = made["request_id"].clone();
    agent("/v1/hub/hook/claude", shown.clone()).await;
    let mut no = answer(&denied, "deny");
    no["reason"] = json!("use the staging key");
    let said = page_post(&env, &alice, true, PERMISSION_ANSWER, no).await;
    assert_eq!(said.0, 200, "{}", said.1);
    let (_, polled) = heard(&denied, "f").await;
    assert_eq!(
        (&polled["decision"], &polled["reason"], polled.get("choice")),
        (&json!("deny"), &json!("use the staging key"), None)
    );
    agent("/v1/hub/hook/claude", ran).await;

    // A person no longer answers for an agent whose token was revoked.
    agent("/v1/hub/hook/claude", prompt).await;
    let (_, made) = agent("/v1/hub/permission/register", register("c", true)).await;
    let last = made["request_id"].clone();
    agent("/v1/hub/hook/claude", shown).await;
    assert!(env.store.revoke_agent_token(&token).unwrap());
    let (status, refused) =
        page_post(&env, &alice, true, PERMISSION_ANSWER, answer(&last, "deny")).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_your_agent"))
    );
    // Unless that was this computer renewing its token, which leaves the person another one for
    // the repository: the answer is taken, and the hook hears it under the new token.
    let (_, renewed) = agent_for(&env, &alice.id, "claude-code-renewed", false);
    let poll = json!({"request_id": last.clone(), "input_hash": "c".repeat(64), "wait_seconds": 0});
    let (status, _) = agent("/v1/hub/permission/poll", poll.clone()).await;
    assert_eq!(status, 401);
    let (status, taken) =
        page_post(&env, &alice, true, PERMISSION_ANSWER, answer(&last, "deny")).await;
    assert_eq!(status, 200, "{taken}");
    let (status, heard) = env
        .call(
            &env.hub,
            Method::POST,
            "/v1/hub/permission/poll",
            Some(&renewed),
            Some(poll),
        )
        .await;
    assert_eq!(
        (status, &heard["decision"]),
        (200, &json!("deny")),
        "{heard}"
    );
}

const AGENT_MESSAGES: &str = "/v1/hub/session/agent-messages";

async fn mcp_call(env: &Env, token: &str, method: &str, params: Value) -> Value {
    let (status, reply) = env
        .call(
            &env.hub,
            Method::POST,
            "/mcp",
            Some(token),
            Some(json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params})),
        )
        .await;
    assert_eq!(status, 200, "{reply}");
    reply["result"].clone()
}

fn texts(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn a_person_messages_only_their_own_session_which_answers_with_receipts() {
    let env = Env::start("github").await;
    let (alice, bob) = (page(&env, ALICE).await, page(&env, BOB).await);
    let (alice_token, alice_secret) = agent_for(&env, &alice.id, "laptop", false);
    let (bob_token, bob_secret) = agent_for(&env, &bob.id, "desk", false);
    let list = format!("{AGENT_MESSAGES}?repository={REPO}");
    let read = || {
        let (hub, cookie, list) = (env.hub.clone(), alice.cookie.clone(), list.clone());
        async move { with_cookie(&hub, Method::GET, &list, &cookie, None).await.1 }
    };
    let page0 = read().await;
    assert_eq!(page0["mode"], "manual", "{page0}");
    assert_eq!(page0["sessions"], json!([]));

    // Gemini CLI: its initialize names it, its first call names its session. A call that names
    // its session is enough to take messages in its tool results (tier mcp).
    mcp_call(
        &env,
        &alice_secret,
        "initialize",
        json!({"protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"gemini-cli","version":"0.9.0"}}),
    )
    .await;
    let status_call = json!({"name":"agentscm_status","arguments":{"session":"s1"}});
    mcp_call(&env, &alice_secret, "tools/call", status_call.clone()).await;
    let seen = read().await;
    let session = seen["sessions"][0].clone();
    assert_eq!(session["tier"], "mcp", "{seen}");
    assert_eq!(session["reach"], "next_step", "{seen}");
    assert_eq!(session["name"], "gemini-cli");
    assert_eq!(session["token_name"], "laptop");
    let s1 = session["session"].as_str().unwrap().to_owned();

    let send = |token: &str, session: &str, note: &str| json!({"repository": REPO, "token": token, "session": session, "note": note});
    // Never to another person's token, and never without the page's CSRF token.
    let (status, _) = page_post(
        &env,
        &alice,
        true,
        AGENT_MESSAGES,
        send(&bob_token, &s1, "x"),
    )
    .await;
    assert_eq!(status, 403);
    let (status, _) = page_post(
        &env,
        &alice,
        false,
        AGENT_MESSAGES,
        send(&alice_token, &s1, "x"),
    )
    .await;
    assert_eq!(status, 403);
    let (status, message) = page_post(
        &env,
        &alice,
        true,
        AGENT_MESSAGES,
        send(&alice_token, &s1, "Please release the stale Work"),
    )
    .await;
    assert_eq!(status, 201, "{message}");
    assert_eq!(message["state"], "queued");
    let id = message["id"].as_str().unwrap().to_owned();
    // Due ten minutes after it was sent, as a session reached at its next step is.
    let sent_at = message["created_at"].as_i64().unwrap();
    assert_eq!(message["deadline_at"], sent_at + 600, "{message}");
    assert_eq!(message["not_picked_up"], false, "{message}");

    // A call that names no session never takes it.
    let no_session = json!({"name":"agentscm_status","arguments":{}});
    let unnamed = texts(&mcp_call(&env, &alice_secret, "tools/call", no_session).await);
    assert!(!unnamed.contains(&id), "{unnamed}");

    // Delivered once, in the session's next tool result with MCP alone, as a notice that names
    // no mode and tells a subagent to leave it.
    let text = texts(&mcp_call(&env, &alice_secret, "tools/call", status_call.clone()).await);
    assert!(text.contains("AgentSCM Hub message"), "{text}");
    assert!(
        text.contains(&id) && text.contains("status accepted"),
        "{text}"
    );
    assert!(text.contains("A subagent leaves it"), "{text}");
    assert!(!text.contains("their yes"), "{text}");
    let again = texts(&mcp_call(&env, &alice_secret, "tools/call", status_call.clone()).await);
    assert!(!again.contains(&id), "{again}");
    let delivered = read().await["messages"][0].clone();
    assert_eq!(delivered["state"], "delivered", "{delivered}");
    assert_eq!(delivered["via"], "mcp", "{delivered}");
    assert!(delivered["delivered_at"].as_i64().unwrap() >= sent_at);
    assert_eq!(
        delivered["read_at"],
        Value::Null,
        "only an answer is a read"
    );

    // Its hook's start carries no subagent marker, and that hook cannot carry context.
    let response = browser()
        .post(format!("{}/v1/hub/hook/generic", env.hub))
        .bearer_auth(&alice_secret)
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"event":"start","agent":"gemini","session":"s1"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let seen = read().await;
    assert_eq!(seen["sessions"][0]["tier"], "mcp", "{seen}");

    // Only the receiving session answers, and only a real id verifies.
    let answer_id = |id: &str, status: &str| {
        json!({"name":"agentscm_hub_message",
            "arguments":{"id": id, "status": status, "session": "s1"}})
    };
    let answer = |status: &str| answer_id(&id, status);
    let refused = mcp_call(&env, &bob_secret, "tools/call", answer("accepted")).await;
    assert_eq!(refused["isError"], true, "{refused}");
    let forged = mcp_call(
        &env,
        &alice_secret,
        "tools/call",
        answer_id("msg_00000000000000000000000000000000", "accepted"),
    )
    .await;
    assert_eq!(forged["isError"], true, "{forged}");
    // Without its session the Hub does not guess one: it asks for the field.
    let unnamed = json!({"name":"agentscm_hub_message",
        "arguments":{"id": id, "status": "accepted"}});
    let unnamed = mcp_call(&env, &alice_secret, "tools/call", unnamed).await;
    assert_eq!(unnamed["isError"], true, "{unnamed}");
    let error = &unnamed["structuredContent"]["error"];
    assert_eq!(error["recovery"]["action"], "provide_field", "{unnamed}");
    assert_eq!(error["fields"][0]["name"], "session", "{unnamed}");
    let ok = mcp_call(&env, &alice_secret, "tools/call", answer("accepted")).await;
    assert_eq!(ok["isError"], false, "{ok}");
    let verified = texts(&ok);
    assert!(
        verified.contains("verified")
            && verified.contains("Please release the stale Work")
            && verified.contains("their yes"),
        "{verified}"
    );
    mcp_call(&env, &alice_secret, "tools/call", answer("done")).await;
    let done = read().await["messages"][0].clone();
    assert_eq!(done["state"], "done", "{done}");
    let (read_at, done_at) = (done["read_at"].as_i64(), done["done_at"].as_i64());
    assert!(read_at.is_some() && done_at >= read_at, "{done}");
    assert_eq!(done["via"], "mcp", "the first delivery's way stays");
    assert_eq!(done["not_picked_up"], false, "{done}");

    // A hook client gets it in the next prompt's added context, with no tool call.
    let hook = |session: &str, subagent: bool| {
        let (hub, secret) = (env.hub.clone(), alice_secret.clone());
        let mut body =
            json!({"hook_event_name":"UserPromptSubmit","session_id":session,"prompt":"hi"});
        if subagent {
            body["agent_id"] = json!("sub-1");
        }
        async move {
            let response = browser()
                .post(format!("{hub}/v1/hub/hook/claude"))
                .bearer_auth(secret)
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_string())
                .send()
                .await
                .unwrap();
            serde_json::from_str::<Value>(&response.text().await.unwrap()).unwrap()
        }
    };
    hook("h1", false).await;
    let h1 = read().await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["hook"] == "hook:claude")
        .cloned()
        .unwrap();
    assert_eq!(h1["tier"], "hook", "{h1}");
    let h1 = h1["session"].as_str().unwrap().to_owned();
    let (status, _) = page_post(
        &env,
        &alice,
        true,
        AGENT_MESSAGES,
        send(&alice_token, &h1, "Auto please"),
    )
    .await;
    assert_eq!(status, 201);
    let reply = hook("h1", false).await;
    let context = reply["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default();
    assert!(context.contains("Auto please"), "{reply}");
    assert_eq!(read().await["messages"][0]["via"], "hook:claude");

    // A session seen only through a subagent's hook calls is offered nothing.
    hook("sub1", true).await;
    let sub = read().await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["hook"] == "hook:claude" && s["session"] != h1.as_str())
        .cloned()
        .unwrap();
    assert_eq!(sub["tier"], "unknown", "{sub}");
    let sub = sub["session"].as_str().unwrap().to_owned();
    let (status, _) = page_post(
        &env,
        &alice,
        true,
        AGENT_MESSAGES,
        send(&alice_token, &sub, "Not for a subagent"),
    )
    .await;
    assert_eq!(status, 201);
    let reply = hook("sub1", true).await;
    assert!(!reply.to_string().contains("Not for a subagent"), "{reply}");
    assert_eq!(read().await["messages"][0]["state"], "queued");

    // Ignore holds everything: sending is refused.
    let mode = |mode: Value| json!({"repository": REPO, "this_repository": true, "mode": mode});
    let response = browser()
        .put(format!("{}{AGENT_MESSAGES}/mode", env.hub))
        .header(
            header::COOKIE,
            format!("__Host-agentscm_session={}", alice.cookie),
        )
        .header("x-csrf-token", &alice.csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(mode(json!("ignore")).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let page = read().await;
    assert_eq!(
        (page["mode"].clone(), page["repository_mode"].clone()),
        (json!("ignore"), json!("ignore"))
    );
    let (status, _) = page_post(
        &env,
        &alice,
        true,
        AGENT_MESSAGES,
        send(&alice_token, &s1, "held"),
    )
    .await;
    assert_eq!(status, 400);
}

/// How a message reaches each session, from what its own calls showed: woken soon only while a
/// Codex waiter's wait is open (a second waiter is refused meanwhile), at its next step once that wait delivered, ended from its
/// `SessionEnd` hook until its next own call.
#[tokio::test]
async fn a_session_reads_as_woken_only_while_its_waiter_waits() {
    let env = Env::start("github").await;
    let alice = page(&env, ALICE).await;
    let (alice_token, alice_secret) = agent_for(&env, &alice.id, "laptop", false);
    let list = format!("{AGENT_MESSAGES}?repository={REPO}");
    let read = || {
        let (hub, cookie, list) = (env.hub.clone(), alice.cookie.clone(), list.clone());
        async move { with_cookie(&hub, Method::GET, &list, &cookie, None).await.1 }
    };
    let reach = |page: &Value, hook: Option<&str>| {
        page["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["hook"].as_str() == hook)
            .map(|s| (s["session"].clone(), s["reach"].clone()))
    };
    let (hub, secret) = (env.hub.clone(), alice_secret.clone());
    let waiting = tokio::spawn(async move {
        let response = browser()
            .post(format!("{hub}/v1/hub/agent-messages/wait"))
            .bearer_auth(secret)
            .header(header::CONTENT_TYPE, "application/json")
            .body(json!({"session": "c1", "agent": "codex"}).to_string())
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    });
    let mut seen = Value::Null;
    for _ in 0..100 {
        seen = read().await;
        if reach(&seen, None).is_some_and(|(_, r)| r == "wake_soon") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (c1, now) = reach(&seen, None).expect("the waiting session is listed");
    assert_eq!(now, "wake_soon", "{seen}");
    assert_eq!(seen["sessions"][0]["waker"], "codex", "{seen}");
    // One waiter per session: a second is refused while the first waits, never swapped in; the
    // shared default session never waits.
    let again = |session: &str| {
        let (hub, secret) = (env.hub.clone(), alice_secret.clone());
        let body = json!({"session": session, "agent": "codex"});
        async move {
            let response = browser()
                .post(format!("{hub}/v1/hub/agent-messages/wait"))
                .bearer_auth(secret)
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_string())
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            (status, response.text().await.unwrap())
        }
    };
    let (status, refused) = again("c1").await;
    assert_eq!(status, 409, "{refused}");
    assert!(refused.contains("waiter_present"), "{refused}");
    assert_eq!(again("default").await.0, 403);

    // Taken by the open wait, recorded as the Codex waker's, due five minutes after it was sent.
    let note = json!({"repository": REPO, "token": alice_token, "session": c1, "note": "Wake up"});
    let (status, message) = page_post(&env, &alice, true, AGENT_MESSAGES, note).await;
    assert_eq!(status, 201, "{message}");
    let sent_at = message["created_at"].as_i64().unwrap();
    assert_eq!(message["deadline_at"], sent_at + 300, "{message}");
    let (status, text) = waiting.await.unwrap();
    assert_eq!(status, 200, "{text}");
    assert!(text.contains(message["id"].as_str().unwrap()), "{text}");
    let page = read().await;
    assert_eq!(page["messages"][0]["via"], "wake:codex", "{page}");
    assert_eq!(
        reach(&page, None).map(|(_, r)| r),
        Some(json!("next_step")),
        "it woke its session and stopped waiting: {page}"
    );

    // A Claude Code session's own end, then its own next prompt.
    let hook = |event: &str| {
        let (hub, secret) = (env.hub.clone(), alice_secret.clone());
        let body = json!({"hook_event_name": event, "session_id": "h1", "prompt": "hi"});
        async move {
            let response = browser()
                .post(format!("{hub}/v1/hub/hook/claude"))
                .bearer_auth(secret)
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_string())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
        }
    };
    hook("UserPromptSubmit").await;
    let claude = Some("hook:claude");
    let (h1, now) = reach(&read().await, claude).unwrap();
    assert_eq!(now, "next_step");
    // Its waiter waits; then the session ends while that wait is still open.
    let (hub, secret) = (env.hub.clone(), alice_secret.clone());
    let waiting = tokio::spawn(async move {
        let response = browser()
            .post(format!("{hub}/v1/hub/agent-messages/wait"))
            .bearer_auth(secret)
            .header(header::CONTENT_TYPE, "application/json")
            .body(json!({"session": "h1"}).to_string())
            .send()
            .await
            .unwrap();
        response.status().as_u16()
    });
    for _ in 0..100 {
        if reach(&read().await, claude).is_some_and(|(_, r)| r == "wake_now") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(reach(&read().await, claude).unwrap().1, "wake_now");
    hook("SessionEnd").await;
    let page = read().await;
    assert_eq!(reach(&page, claude).unwrap().1, "ended", "{page}");
    // A message sent now is not taken by the ended session's wait: it stays queued, due soon.
    let note = json!({"repository": REPO, "token": alice_token, "session": h1, "note": "Gone"});
    let (status, message) = page_post(&env, &alice, true, AGENT_MESSAGES, note).await;
    assert_eq!(status, 201, "{message}");
    assert_eq!(
        message["deadline_at"],
        message["created_at"].as_i64().unwrap() + 120
    );
    assert_eq!(waiting.await.unwrap(), 204);
    assert_eq!(read().await["messages"][0]["state"], "queued");
    hook("UserPromptSubmit").await;
    assert_eq!(reach(&read().await, claude).unwrap().1, "next_step");
}

const APP_SECRET: &str = "example-app-client-secret-0123456789abcdef";
const APP_REDIRECT: &str = "https://app.example.com/callback";
const AUTHORIZE: &str = "/v1/hub/apps/authorize";
const VERIFIER: &str = "example-verifier-0123456789-0123456789-0123456789";

/// A private file, as the Hub requires for its config and secrets.
fn private_file(env: &Env, name: &str, text: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = env.dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path.display().to_string()
}

/// A Hub with one connected app, "Example <App>", whose name needs escaping.
async fn apps_env() -> Env {
    apps_env_with(&[]).await
}

/// The same, plus another app for each `(id, read_only)` in `more`.
async fn apps_env_with(more: &[(&str, bool)]) -> Env {
    let mut env = Env::start("github").await;
    let secret = private_file(&env, "app-secret", &format!("{APP_SECRET}\n"));
    let mut list = vec![json!({"id": "example-app", "name": "Example <App>",
        "redirect_uri": APP_REDIRECT, "secret_file": secret, "via": "slack"})];
    for &(id, read_only) in more {
        let access = if read_only {
            ("read_only", json!(true))
        } else {
            ("via", json!("slack"))
        };
        let mut app = json!({"id": id, "name": id,
            "redirect_uri": APP_REDIRECT, "secret_file": secret});
        app[access.0] = access.1;
        list.push(app);
    }
    let apps = json!({ "apps": list });
    env.apps = Some(private_file(&env, "apps.json", &apps.to_string()));
    env.hub = env.serve("").await;
    env
}

fn encode(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://x/").unwrap();
    url.query_pairs_mut().extend_pairs(pairs);
    url.query().unwrap().to_owned()
}

fn challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier))
}

fn authorize_query(client: &str, redirect: &str) -> String {
    encode(&[
        ("client_id", client),
        ("redirect_uri", redirect),
        ("state", "st-1"),
        ("code_challenge", &challenge(VERIFIER)),
        ("code_challenge_method", "S256"),
    ])
}

async fn authorize_page(env: &Env, query: &str, cookie: Option<&str>) -> (u16, HeaderMap, String) {
    let mut request = browser().get(format!("{}{AUTHORIZE}?{query}", env.hub));
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, format!("__Host-agentscm_session={cookie}"));
    }
    let response = request.send().await.unwrap();
    let (status, headers) = (response.status().as_u16(), response.headers().clone());
    (status, headers, response.text().await.unwrap())
}

/// The approval form's POST; `csrf` None leaves the field out.
async fn approve(env: &Env, cookie: &str, csrf: Option<&str>, decision: &str) -> (u16, String) {
    let ch = challenge(VERIFIER);
    let mut pairs = vec![
        ("client_id", "example-app"),
        ("redirect_uri", APP_REDIRECT),
        ("state", "st-1"),
        ("code_challenge", ch.as_str()),
        ("code_challenge_method", "S256"),
        ("decision", decision),
    ];
    pairs.extend(csrf.map(|csrf| ("csrf", csrf)));
    let response = browser()
        .post(format!("{}{AUTHORIZE}", env.hub))
        .header(header::COOKIE, format!("__Host-agentscm_session={cookie}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(encode(&pairs))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default();
    (status, location)
}

/// A code from Alice's approval.
async fn app_code(env: &Env, page: &Page) -> String {
    let (status, location) = approve(env, &page.cookie, Some(&page.csrf), "allow").await;
    assert_eq!(status, 303);
    let url = reqwest::Url::parse(&location).unwrap();
    assert_eq!(&location[..APP_REDIRECT.len()], APP_REDIRECT);
    let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs["state"], "st-1");
    pairs["code"].clone()
}

async fn exchange(env: &Env, json: bool, fields: &[(&str, &str)]) -> (u16, Value) {
    let request = env.http.post(format!("{}/v1/hub/apps/token", env.hub));
    let request = if json {
        let body: serde_json::Map<String, Value> = fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), json!(v)))
            .collect();
        request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Value::Object(body).to_string())
    } else {
        request
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(encode(fields))
    };
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn token_fields<'a>(code: &'a str, secret: &'a str, verifier: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", APP_REDIRECT),
        ("client_id", "example-app"),
        ("client_secret", secret),
        ("code_verifier", verifier),
    ]
}

/// Alice connects the app and returns its credential.
async fn connect_app(env: &Env, alice: &Page) -> String {
    let code = app_code(env, alice).await;
    let (status, issued) = exchange(env, true, &token_fields(&code, APP_SECRET, VERIFIER)).await;
    assert_eq!(status, 200, "{issued}");
    issued["credential"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_connected_app_is_approved_through_sign_in_consent_and_a_single_use_pkce_code() {
    // No apps file: every app route is 404.
    let plain = Env::start("github").await;
    let query = authorize_query("example-app", APP_REDIRECT);
    assert_eq!(authorize_page(&plain, &query, None).await.0, 404);
    let fields = token_fields("x", APP_SECRET, VERIFIER);
    assert_eq!(exchange(&plain, false, &fields).await.0, 404);

    let env = apps_env_with(&[("notifier", true)]).await;
    // An unknown app, a different redirect or no PKCE gets an error page and never a redirect.
    for bad in [
        authorize_query("other-app", APP_REDIRECT),
        authorize_query("example-app", "https://evil.example/callback"),
        authorize_query("example-app", "https://app.example.com/callback/"),
        encode(&[
            ("client_id", "example-app"),
            ("redirect_uri", APP_REDIRECT),
            ("state", "s"),
        ]),
    ] {
        let (status, headers, body) = authorize_page(&env, &bad, None).await;
        assert_eq!(status, 400, "{bad}");
        assert!(headers.get(header::LOCATION).is_none(), "{bad}");
        assert!(!body.contains("evil.example"), "{body}");
    }

    // Signed out: the page reloads itself from the Hub once (the Strict cookie is not sent from
    // another site), then offers the sign-in that returns here.
    let (status, headers, body) = authorize_page(&env, &query, None).await;
    assert_eq!(status, 200);
    assert!(body.contains("http-equiv=\"refresh\""), "{body}");
    assert!(body.contains("hop=1"), "{body}");
    // A browser that ignores the refresh gets a plain link to the same page.
    assert!(
        body.contains(">Continue</a>") && body.contains("&amp;hop=1\">Continue"),
        "{body}"
    );
    let (_, _, body) = authorize_page(&env, &format!("{query}&hop=1"), None).await;
    assert!(
        !body.contains("http-equiv") && !body.contains("Continue"),
        "{body}"
    );
    let here = format!("{AUTHORIZE}?{query}");
    let sign_in = encode(&[("web", "1"), ("return_to", &here)]);
    assert!(
        body.contains(&format!(
            "/v1/hub/login/github?{}",
            sign_in.replace('&', "&amp;")
        )),
        "{body}"
    );
    assert!(headers[header::CACHE_CONTROL] == "no-store");
    let done = callback_query(&env, ALICE, &sign_in).await;
    assert_eq!(done.status(), 302);
    assert_eq!(done.headers()[header::LOCATION].to_str().unwrap(), here);
    let cookie = set_cookie(&done, "__Host-agentscm_session").unwrap();

    // Signed in: the consent page, escaped, without script, framed by nobody, posting only here
    // and to the app's own origin.
    let (status, headers, body) = authorize_page(&env, &query, Some(&cookie)).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("Example &lt;App&gt;") && !body.contains("<App>"),
        "{body}"
    );
    assert!(
        body.contains("via Slack") && body.contains("Computers"),
        "{body}"
    );
    // A read-only app's page says it only reads.
    let notifier = authorize_query("notifier", APP_REDIRECT);
    let (status, _, page) = authorize_page(&env, &notifier, Some(&cookie)).await;
    assert_eq!(status, 200, "{page}");
    assert!(
        page.contains("It can see which repositories you can read")
            && page.contains("it can't answer for you or change anything")
            && !page.contains("via Slack")
            && !page.contains("allow or deny"),
        "{page}"
    );
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(
        csp.contains("default-src 'none'") && !csp.contains("script-src"),
        "{csp}"
    );
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(
        csp.contains("form-action 'self' https://app.example.com"),
        "{csp}"
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let (_, me) = with_cookie(&env.hub, Method::GET, "/v1/hub/session", &cookie, None).await;
    let alice = Page {
        csrf: me["csrf_token"].as_str().unwrap().to_owned(),
        id: me["user"]["id"].as_str().unwrap().to_owned(),
        cookie,
    };
    assert!(body.contains(&format!("name=\"csrf\" value=\"{}\"", alice.csrf)));

    // A post without the page's CSRF field, or with another, is refused without a redirect.
    assert_eq!(
        approve(&env, &alice.cookie, None, "allow").await,
        (403, String::new())
    );
    assert_eq!(
        approve(&env, &alice.cookie, Some("0".repeat(64).as_str()), "allow").await,
        (403, String::new())
    );
    assert_eq!(
        approve(&env, "nope", Some(&alice.csrf), "allow").await.0,
        403
    );
    // Deny goes back to the app with access_denied.
    let (status, location) = approve(&env, &alice.cookie, Some(&alice.csrf), "deny").await;
    assert_eq!(
        (status, location.as_str()),
        (
            303,
            "https://app.example.com/callback?error=access_denied&state=st-1"
        )
    );

    // The token exchange: a wrong secret does not spend the code; a wrong verifier does.
    let code = app_code(&env, &alice).await;
    let wrong_secret = token_fields(&code, "not-the-secret", VERIFIER);
    let (status, refused) = exchange(&env, false, &wrong_secret).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("invalid_client"))
    );
    let other_verifier = format!("{VERIFIER}x");
    let (status, refused) = exchange(
        &env,
        false,
        &token_fields(&code, APP_SECRET, &other_verifier),
    )
    .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("invalid_grant"))
    );
    let (status, refused) = exchange(&env, false, &token_fields(&code, APP_SECRET, VERIFIER)).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("invalid_grant"))
    );
    let mut bad_grant = token_fields(&code, APP_SECRET, VERIFIER);
    bad_grant[0].1 = "client_credentials";
    let (status, refused) = exchange(&env, false, &bad_grant).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("invalid_request"))
    );

    // A good code, as a form, once.
    let code = app_code(&env, &alice).await;
    let (status, issued) = exchange(&env, false, &token_fields(&code, APP_SECRET, VERIFIER)).await;
    assert_eq!(status, 200, "{issued}");
    assert_eq!(issued["device"]["name"], "Example <App>");
    assert_eq!(
        issued["owner"],
        json!({"provider": "github", "host": env.base.trim_start_matches("http://"),
            "user_id": ALICE, "login": "alice"})
    );
    assert_eq!(issued["credential"].as_str().unwrap().len(), 64);
    let (status, _) = exchange(&env, true, &token_fields(&code, APP_SECRET, VERIFIER)).await;
    assert_eq!(status, 400);
    // A code is bound to its redirect.
    let code = app_code(&env, &alice).await;
    let mut moved = token_fields(&code, APP_SECRET, VERIFIER);
    moved[2].1 = "https://app.example.com/other";
    assert_eq!(
        exchange(&env, true, &moved).await.1["error"],
        "invalid_grant"
    );

    // The person's computers list names the app.
    let (status, list) = with_cookie(
        &env.hub,
        Method::GET,
        "/v1/hub/devices",
        &alice.cookie,
        None,
    )
    .await;
    assert_eq!(status, 200, "{list}");
    let app = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["id"] == issued["device"]["id"])
        .unwrap();
    assert_eq!(
        app["app"],
        json!({"id": "example-app", "name": "Example <App>"})
    );
    assert!(!alice.id.is_empty());

    // One person has at most eight codes waiting.
    for _ in 0..8 {
        app_code(&env, &alice).await;
    }
    let (status, location) = approve(&env, &alice.cookie, Some(&alice.csrf), "allow").await;
    assert_eq!((status, location.as_str()), (429, ""));
}

#[tokio::test]
async fn a_connected_app_reads_mints_read_only_and_answers_only_as_its_person() {
    let env = apps_env().await;
    let (alice, bob) = (page(&env, ALICE).await, page(&env, BOB).await);
    let app = connect_app(&env, &alice).await;

    // What the person's computer reads, and read-only tokens only.
    let (status, me) = env.get("/v1/hub/device", &app).await;
    assert_eq!(
        (status, me["user"]["login"].as_str()),
        (200, Some("alice")),
        "{me}"
    );
    let (status, repos) = env.get("/v1/hub/device/repositories", &app).await;
    assert_eq!(status, 200, "{repos}");
    assert_eq!(repos["repositories"][0]["repository"], REPO);
    let mint = |read_only: Option<bool>| {
        let mut body = json!({"name": "example-read", "repository": REPO});
        if let Some(read_only) = read_only {
            body["read_only"] = json!(read_only);
        }
        env.mint_at(&env.hub, &app, body)
    };
    let (status, minted) = mint(Some(true)).await;
    assert_eq!(
        (status, minted["read_only"].as_bool()),
        (201, Some(true)),
        "{minted}"
    );
    let read_token = minted["token"].as_str().unwrap().to_owned();
    for writable in [Some(false), None] {
        let (status, refused) = mint(writable).await;
        assert_eq!(
            (status, refused["error"].as_str()),
            (403, Some("app_read_only"))
        );
    }

    // Nothing else: computer routes, the person's other routes, agent and admin routes.
    let refused = [
        (
            Method::POST,
            "/v1/hub/device/name",
            json!({"name": "x"}),
            403,
        ),
        (Method::GET, "/v1/hub/devices", Value::Null, 403),
        (Method::POST, "/v1/hub/devices", json!({}), 401),
        (
            Method::POST,
            "/v1/hub/tokens",
            json!({"name": "x", "repository": REPO, "ttl_seconds": 60}),
            401,
        ),
        (
            Method::POST,
            "/v1/hub/session/work/transition",
            json!({}),
            401,
        ),
        (
            Method::POST,
            "/v1/hub/session/work/recovery",
            json!({}),
            401,
        ),
        (
            Method::POST,
            "/v1/hub/session/pickups",
            json!({"repository": REPO}),
            401,
        ),
        (Method::POST, "/v1/hub/session/handoff/ask", json!({}), 401),
        (
            Method::POST,
            "/v1/hub/session/coordination/withdraw",
            json!({}),
            401,
        ),
        (
            Method::POST,
            "/v1/hub/session/read-token",
            json!({"repository": REPO}),
            401,
        ),
        (Method::GET, "/v1/hub/whoami", Value::Null, 401),
        (Method::GET, "/v1/hub/admin/devices", Value::Null, 401),
    ];
    for (method, path, body, expected) in refused {
        let body = (!body.is_null()).then_some(body);
        let (status, reply) = env.call(&env.hub, method, path, Some(&app), body).await;
        assert_eq!(status, expected, "{path}: {reply}");
    }

    // A coordination reply for Alice's own agent, recorded as from Slack.
    let (lead, _) = agent_for(&env, &alice.id, "claude-code", false);
    let (asker, _) = agent_for(&env, &bob.id, "codex", false);
    let now = unix_now();
    env.store
        .work_start(&lead, &work("billing", "alice-session"), now)
        .unwrap();
    let request = env
        .store
        .work_start(&asker, &work("rounding", "bob-session"), now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let body = |token: &str| {
        json!({"token": token, "request_id": request.id, "operation_id": "slack-1",
            "target_work_id": request.targets[0].work_id, "session": "alice-session",
            "expected_updated_seq": request.updated_seq, "kind": "handover", "note": null})
    };
    let (status, refused) = env
        .call(
            &env.hub,
            Method::POST,
            REPLY,
            Some(&app),
            Some(body(&asker)),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_your_agent"))
    );
    let (status, answered) = env
        .call(&env.hub, Method::POST, REPLY, Some(&app), Some(body(&lead)))
        .await;
    assert_eq!(status, 200, "{answered}");
    assert_eq!(answered["responses"][0]["via"], "slack");
    assert_eq!(answered["responses"][0]["responder"]["login"], "alice");
    let (status, replay) = env
        .call(&env.hub, Method::POST, REPLY, Some(&app), Some(body(&lead)))
        .await;
    assert_eq!(
        (status, replay["responses"].as_array().unwrap().len()),
        (200, 1)
    );
    let responded: Vec<_> = env
        .store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "coordination_responded")
        .collect();
    assert!(
        responded[0].data.contains("\"via\":\"slack\""),
        "{}",
        responded[0].data
    );

    // A file handoff answered through the app is recorded as answered from Slack.
    let (holder, _) = agent_for(&env, &alice.id, "claude-code", false);
    let (wanter, _) = agent_for(&env, &bob.id, "codex", false);
    let file = "src/tax.rs";
    env.store
        .touch_session(&holder, &hook_call("ha", &[file]), now)
        .unwrap();
    env.store
        .touch_session(&wanter, &hook_call("hb", &[]), now + 1)
        .unwrap();
    env.store
        .ask_handoff(&wanter, Some("hb"), file, None, now + 2)
        .unwrap();
    let answer = json!({"token": holder, "session": "ha", "path": file, "accept": true});
    let (status, done) = env
        .call(&env.hub, Method::POST, ANSWER, Some(&app), Some(answer))
        .await;
    assert_eq!(
        (status, done["outcome"].as_str()),
        (200, Some("granted")),
        "{done}"
    );
    let path = format!("/v1/hub/session/handoffs?repository={REPO}");
    let (_, mine) = with_cookie(&env.hub, Method::GET, &path, &alice.cookie, None).await;
    let row = mine["handoffs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["path"] == file)
        .unwrap();
    assert_eq!(
        (
            row["answered_via_slack"].as_bool(),
            row["answered_via_hub"].as_bool()
        ),
        (Some(true), Some(false))
    );

    // The read-only token reads as the person.
    assert_eq!(env.whoami(&read_token).await, 200);

    // An app the operator stops registering loses its credential.
    let mut env = env;
    env.apps = None;
    let unregistered = env.serve("").await;
    let (status, _) = env
        .call(
            &unregistered,
            Method::GET,
            "/v1/hub/device",
            Some(&app),
            None,
        )
        .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_revoked_connected_app_is_refused_everywhere() {
    let env = apps_env().await;
    let alice = page(&env, ALICE).await;
    let app = connect_app(&env, &alice).await;
    let (status, minted) = env
        .mint_at(
            &env.hub,
            &app,
            json!({"name": "example-read", "repository": REPO, "read_only": true}),
        )
        .await;
    assert_eq!(status, 201, "{minted}");
    let read_token = minted["token"].as_str().unwrap().to_owned();
    assert_eq!(env.delete("/v1/hub/device", &app).await, 204);
    for (method, path) in [
        (Method::GET, "/v1/hub/device"),
        (Method::GET, "/v1/hub/device/repositories"),
        (Method::POST, "/v1/hub/device/tokens"),
        (Method::DELETE, "/v1/hub/device"),
        (Method::POST, REPLY),
        (Method::POST, ANSWER),
    ] {
        let body = (method == Method::POST).then(|| json!({}));
        let (status, reply) = env.call(&env.hub, method, path, Some(&app), body).await;
        assert_eq!(status, 401, "{path}: {reply}");
    }
    // Its read-only tokens went with it.
    assert_eq!(env.whoami(&read_token).await, 401);

    // The person revokes a second connection from their computers list.
    let app = connect_app(&env, &alice).await;
    // Leaving the allow list stops the app answering for the person.
    let allow_list = env.serve("github:bob").await;
    let (status, refused) = env
        .call(
            &allow_list,
            Method::POST,
            REPLY,
            Some(&app),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("not_allowed"))
    );
    let (_, me) = env.get("/v1/hub/device", &app).await;
    let id = me["device"]["id"].as_str().unwrap();
    let (status, _) = with_cookie(
        &env.hub,
        Method::DELETE,
        &format!("/v1/hub/devices/{id}"),
        &alice.cookie,
        Some(&alice.csrf),
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(env.get("/v1/hub/device/repositories", &app).await.0, 401);
}

#[tokio::test]
async fn reconnecting_leaves_one_credential_per_person_and_app() {
    let env = apps_env().await;
    let alice = page(&env, ALICE).await;
    let first = connect_app(&env, &alice).await;
    let (status, minted) = env
        .mint_at(
            &env.hub,
            &first,
            json!({"name": "example-read", "repository": REPO, "read_only": true}),
        )
        .await;
    assert_eq!(status, 201, "{minted}");
    let read_token = minted["token"].as_str().unwrap().to_owned();
    // Auto-connect is a computer's; an app credential cannot ask for it.
    let (status, refused) = env
        .mint_at(
            &env.hub,
            &first,
            json!({"name": "example-read", "repository": REPO, "read_only": true, "auto": true}),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (403, Some("app_read_only"))
    );
    let second = connect_app(&env, &alice).await;
    assert_eq!(env.get("/v1/hub/device", &first).await.0, 401);
    assert_eq!(env.whoami(&read_token).await, 401);
    assert_eq!(env.get("/v1/hub/device", &second).await.0, 200);
    let created = env
        .store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "device_created")
        .map(|event| serde_json::from_str::<Value>(&event.data).unwrap()["app_id"].clone())
        .collect::<Vec<_>>();
    assert!(created.iter().all(|id| id == "example-app"), "{created:?}");
}

#[tokio::test]
async fn another_apps_credentials_cannot_spend_a_code_and_a_dropped_app_stays_revoked() {
    let mut env = apps_env_with(&[("other-app", false)]).await;
    let alice = page(&env, ALICE).await;
    let code = app_code(&env, &alice).await;
    let mut stranger = token_fields(&code, APP_SECRET, VERIFIER);
    stranger[3].1 = "other-app";
    assert_eq!(
        exchange(&env, true, &stranger).await.1["error"],
        "invalid_grant"
    );
    // The code is still good for its own app.
    let (status, issued) = exchange(&env, true, &token_fields(&code, APP_SECRET, VERIFIER)).await;
    assert_eq!(status, 200, "{issued}");
    let credential = issued["credential"].as_str().unwrap().to_owned();

    // Dropping the app and restarting revokes for good: registering it again does not revive it.
    let apps = env.apps.clone();
    env.apps = None;
    env.serve("").await;
    env.apps = apps;
    let back = env.serve("").await;
    let (status, _) = env
        .call(
            &back,
            Method::GET,
            "/v1/hub/device",
            Some(&credential),
            None,
        )
        .await;
    assert_eq!(status, 401);
}

/// An embedder's access rules for the tests: a forge user id to the repositories they may use.
struct Grants {
    store: Arc<dyn Store>,
    people: Mutex<HashMap<i64, Vec<(&'static str, Level)>>>,
}

impl Access for Grants {
    fn grants(&self, person: &store::User) -> agentscm::hub::Result<Option<Vec<Grant>>> {
        let Some(named) = self
            .people
            .lock()
            .unwrap()
            .get(&person.identity.user_id)
            .cloned()
        else {
            return Ok(None);
        };
        let mut grants = Vec::new();
        for (name, level) in named {
            if let Some(repository) = self.store.repository_named(&person.identity.host, name)? {
                grants.push(Grant { repository, level });
            }
        }
        Ok(Some(grants))
    }
}

async fn whoami(env: &Env, secret: &str) -> (u16, Value) {
    let response = env
        .http
        .get(format!("{}/v1/hub/whoami", env.hub))
        .bearer_auth(secret)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test]
async fn an_embedders_grants_decide_who_signs_in_and_who_may_change_anything() {
    let mut env = Env::start("github").await;
    // A forge role alone no longer lets a person in.
    env.role(EMU, "write", "write", 30);
    let grants = Arc::new(Grants {
        store: env.store.clone(),
        people: Mutex::new(HashMap::from([
            (ALICE, vec![(REPO, Level::Contribute)]),
            (BOB, vec![(REPO, Level::Read)]),
        ])),
    });
    env.access = Some(grants.clone());
    env.hub = env.serve("").await;

    let stranger = web_callback(&env, EMU).await;
    assert!(set_cookie(&stranger, "__Host-agentscm_session").is_none());

    let alice = page(&env, ALICE).await;
    let bob = page(&env, BOB).await;
    for page in [&alice, &bob] {
        let (status, body) = page_post(
            &env,
            page,
            true,
            "/v1/hub/session/read-token",
            json!({"repository": REPO}),
        )
        .await;
        assert_eq!(status, 201, "{body}");
        let (_, listed) = with_cookie(
            &env.hub,
            Method::GET,
            "/v1/hub/session/repositories",
            &page.cookie,
            None,
        )
        .await;
        let rows = listed["repositories"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{listed}");
        assert_eq!(rows[0]["read_only"], json!(page.id == bob.id), "{listed}");
    }

    // A person who may only read changes nothing, through a page or through a token.
    let pickup = json!({"repository": REPO, "title": "Finish the widget"});
    let (status, body) =
        page_post(&env, &bob, true, "/v1/hub/session/pickups", pickup.clone()).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (403, Some("read_only")),
        "{body}"
    );
    let (status, body) = page_post(&env, &alice, true, "/v1/hub/session/pickups", pickup).await;
    assert_eq!(status, 200, "{body}");
    // Bob's writing token, as from before he could only read, now only reads.
    let (bob_token, bob_secret) = agent_for(&env, &bob.id, "bob-agent", false);
    let (alice_token, alice_secret) = agent_for(&env, &alice.id, "alice-agent", false);
    let (status, me) = whoami(&env, &bob_secret).await;
    assert_eq!(
        (status, &me["token"]["read_only"]),
        (200, &json!(true)),
        "{me}"
    );
    let (_, me) = whoami(&env, &alice_secret).await;
    assert_eq!(me["token"]["read_only"], json!(false), "{me}");
    // Nor can his page answer through that token.
    let answer = |token: &str| json!({"token": token, "path": "src/lib.rs", "accept": true});
    let (status, body) = page_post(
        &env,
        &bob,
        true,
        "/v1/hub/session/handoff/answer",
        answer(&bob_token),
    )
    .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (403, Some("not_your_agent")),
        "{body}"
    );
    let (status, body) = page_post(
        &env,
        &alice,
        true,
        "/v1/hub/session/handoff/answer",
        answer(&alice_token),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // Once the embedder stops granting her, Alice's sign-in and her agent's token stop on their
    // next request, without a new sign-in or a restart.
    grants.people.lock().unwrap().remove(&ALICE);
    let (status, body) = with_cookie(
        &env.hub,
        Method::GET,
        "/v1/hub/session",
        &alice.cookie,
        None,
    )
    .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (403, Some("not_allowed")),
        "{body}"
    );
    let (status, body) = whoami(&env, &alice_secret).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (403, Some("insufficient_role")),
        "{body}"
    );
}
