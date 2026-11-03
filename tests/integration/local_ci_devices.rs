//! Trusted local CI devices over HTTP against an in-process fake forge: administrator
//! enrollment, rotation and revocation, and the recheck of current authorization for an
//! offline result. Synthetic keys from fixed seeds; no live accounts.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agentscm::backend::LocalBackend;
use agentscm::evidence::EvaluationClock;
use agentscm::hub::connection::NewConnection;
use agentscm::hub::local_ci_devices::authorize_offline_result;
use agentscm::hub::login::{Login, LoginConfig};
use agentscm::hub::store::{self, LocalCiRequest, Store, mint_bound_agent_token};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::{HubState, router};
use agentscm::local_ci::{
    DeviceAssurance, DeviceAuthorization, DeviceClaim, DeviceDecision, DeviceKey, DeviceRefusal,
    DeviceUnknown, EnrollmentStatement, LOCAL_CI_CHECK_KIND, OwnerIdentity,
};
use agentscm::policy::issuers::issuer_key_id;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const CLIENT_ID: &str = "fake-client-id";
const CLIENT_SECRET: &str = "fake-client-secret-value";
const HUB_TOKEN: &str = "synthetic-hub-connection-token";
const HUB_BOT: i64 = 100;
const ALICE: i64 = 7;
const BOB: i64 = 8;
const ADMIN: i64 = 9;
const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 4242;
/// A synthetic private seed that must never leave the signing side.
const CANARY: &str = "canary-private-seed-do-not-leak";
const DAY_MS: u64 = 86_400_000;

#[derive(Default)]
struct Fake {
    users: HashMap<i64, String>,
    tokens: HashMap<String, i64>,
    approve: i64,
    roles: HashMap<i64, &'static str>,
    hub_reads_roles: bool,
    role_reads: u32,
    /// Runs once while the next role lookup is in flight.
    during_role_read: Option<Box<dyn FnOnce() + Send>>,
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

fn github_repo(fake: &Shared, headers: &HeaderMap) -> Response {
    let fake = fake.lock().unwrap();
    let Some(who) = caller(&fake, headers) else {
        return status(StatusCode::UNAUTHORIZED);
    };
    let mut body = json!({"id": REPO_ID, "full_name": REPO, "private": true});
    if who != HUB_BOT {
        let Some(&permission) = fake.roles.get(&who) else {
            return status(StatusCode::NOT_FOUND);
        };
        body["permissions"] = json!({"pull": true,
            "push": matches!(permission, "write" | "admin"), "triage": false});
    }
    Json(body).into_response()
}

fn github_permission(fake: &Shared, headers: &HeaderMap, login: String) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.role_reads += 1;
    if let Some(hook) = fake.during_role_read.take() {
        hook();
    }
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
    let permission = fake.roles.get(&id).copied().unwrap_or("none");
    Json(json!({"permission": permission, "role_name": permission,
        "user": {"id": id, "login": login}}))
    .into_response()
}

async fn start_fake() -> (Shared, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let fake: Shared = Arc::new(Mutex::new(Fake {
        users: HashMap::from([
            (ALICE, "alice".into()),
            (BOB, "bob".into()),
            (ADMIN, "admin".into()),
            (HUB_BOT, "hubbot".into()),
        ]),
        tokens: HashMap::from([(HUB_TOKEN.into(), HUB_BOT)]),
        hub_reads_roles: true,
        ..Fake::default()
    }));
    type F = Form<HashMap<String, String>>;
    type H = HeaderMap;
    let app =
        Router::new()
            .route(
                "/login/device/code",
                post(|| async {
                    Json(json!({"device_code": "devcode", "user_code": "ABCD-1234",
                "verification_uri": "http://127.0.0.1/device", "expires_in": 900, "interval": 1}))
                }),
            )
            .route(
                "/login/oauth/access_token",
                post(|State(f): State<Shared>, Form(x): F| async move {
                    let mut fake = f.lock().unwrap();
                    if x.get("client_id").map(String::as_str) != Some(CLIENT_ID) {
                        return Json(json!({"error": "invalid_client"}));
                    }
                    let user = fake.approve;
                    let token = format!("synthetic-user-token-{user}-{}", fake.tokens.len());
                    fake.tokens.insert(token.clone(), user);
                    Json(json!({"access_token": token, "token_type": "bearer", "expires_in": 7200}))
                }),
            )
            .route(
                "/user",
                get(|State(f): State<Shared>, h: H| async move {
                    let fake = f.lock().unwrap();
                    match caller(&fake, &h) {
                        Some(id) => {
                            Json(json!({"id": id, "login": fake.users[&id]})).into_response()
                        }
                        None => status(StatusCode::UNAUTHORIZED),
                    }
                }),
            )
            .route(
                "/repos/{o}/{n}",
                get(|State(f): State<Shared>, h: H| async move { github_repo(&f, &h) }),
            )
            .route(
                "/repos/{o}/{n}/collaborators/{l}/permission",
                get(
                    |State(f): State<Shared>,
                     h: H,
                     Path((_, _, l)): Path<(String, String, String)>| async move {
                        github_permission(&f, &h, l)
                    },
                ),
            )
            .with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (fake, base)
}

struct Env {
    hub: String,
    host: String,
    fake: Shared,
    store: Arc<dyn Store>,
    state: HubState,
    http: reqwest::Client,
    /// Every response body seen, for the secret check.
    seen: Mutex<Vec<String>>,
    _dir: TempDir,
}

impl Env {
    async fn start() -> Self {
        let (fake, base) = start_fake().await;
        let dir = TempDir::new().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub = format!("http://{}", listener.local_addr().unwrap());
        let vars: HashMap<String, String> = HashMap::from([
            ("AGENTSCM_GITHUB_CLIENT_ID".into(), CLIENT_ID.into()),
            ("AGENTSCM_GITHUB_CLIENT_SECRET".into(), CLIENT_SECRET.into()),
            ("AGENTSCM_GITHUB_URL".into(), base.clone()),
            ("AGENTSCM_ADMINS".into(), format!("github:#{ADMIN}")),
            ("AGENTSCM_GITHUB_API_URL".into(), base.clone()),
            ("AGENTSCM_GITHUB_PAT".into(), HUB_TOKEN.into()),
        ]);
        let login = LoginConfig::from_lookup(&hub, |name| vars.get(name).cloned()).unwrap();
        let secrets = dir.path().join("secrets");
        sync::prepare_secrets_dir(&secrets, true).unwrap();
        let sync_config =
            SyncConfig::from_lookup(&hub, secrets, 14, |name| vars.get(name).cloned()).unwrap();
        let state = HubState::new(
            store.clone(),
            Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
        )
        .with_login(Login::new(login).unwrap())
        .with_sync(Sync::new(sync_config).unwrap());
        sync::start(&state).await.unwrap();
        let app = router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let host = base.trim_start_matches("http://").to_owned();
        let connection = store.pat_connection(&host).unwrap().unwrap().id;
        store
            .upsert_repository(&connection, &host, REPO_ID, REPO, None, None)
            .unwrap();
        {
            let mut fake = fake.lock().unwrap();
            fake.roles.insert(ALICE, "write");
            fake.roles.insert(BOB, "read");
            fake.roles.insert(ADMIN, "read");
        }
        Self {
            hub,
            host,
            fake,
            store,
            state,
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            seen: Mutex::new(Vec::new()),
            _dir: dir,
        }
    }

    fn label(&self) -> String {
        format!("{}/{REPO}", self.host)
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self.http.request(method, format!("{}{path}", self.hub));
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
        self.seen.lock().unwrap().push(text.clone());
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Device-flow sign-in; returns (session, computer credential).
    async fn sign_in(&self, user: i64) -> (String, String) {
        self.fake.lock().unwrap().approve = user;
        let (status, started) = self
            .call(
                Method::POST,
                "/v1/hub/login/github/device",
                None,
                Some(json!({})),
            )
            .await;
        assert_eq!(status, 200, "{started}");
        let (status, signed) = self
            .call(
                Method::POST,
                "/v1/hub/login/device/poll",
                None,
                Some(json!({"handle": started["handle"], "device": {"name": "synthetic laptop"}})),
            )
            .await;
        assert_eq!(
            (status, signed["status"].as_str()),
            (200, Some("signed_in")),
            "{signed}"
        );
        (
            signed["session"].as_str().unwrap().into(),
            signed["device"]["secret"].as_str().unwrap().into(),
        )
    }

    fn user(&self, forge_id: i64) -> store::User {
        self.store
            .list_users()
            .unwrap()
            .into_iter()
            .find(|user| user.identity.user_id == forge_id)
            .unwrap()
    }

    fn identity(&self, forge_id: i64) -> OwnerIdentity {
        let user = self.user(forge_id);
        OwnerIdentity {
            provider: user.identity.provider,
            host: user.identity.host,
            person_id: user.identity.user_id.to_string(),
        }
    }

    fn kinds(&self) -> Vec<String> {
        self.store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .filter(|kind| kind.starts_with("local_ci_device_"))
            .collect()
    }

    fn device_count(&self) -> usize {
        let repo = self
            .store
            .repository_named(&self.host, REPO)
            .unwrap()
            .unwrap();
        self.store.local_ci_devices(&repo.id, None).unwrap().len()
    }
}

/// Fixed for the process, so a resent request is byte for byte the same.
fn started_ms() -> u64 {
    static START: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *START.get_or_init(now_ms)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn signing(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn statement(
    label: &str,
    owner: &OwnerIdentity,
    device: &str,
    key: &SigningKey,
    generation: u64,
) -> EnrollmentStatement {
    let public_key_hex = hex(key.verifying_key().as_bytes());
    EnrollmentStatement {
        version: 1,
        repository: label.into(),
        owner: owner.clone(),
        device_id: device.into(),
        key_id: issuer_key_id(&public_key_hex).unwrap(),
        public_key_hex,
        generation,
    }
}

fn sign(statement: &EnrollmentStatement, key: &SigningKey) -> String {
    hex(&key.sign(&statement.signing_message().unwrap()).to_bytes())
}

const ENROLL: &str = "/v1/hub/admin/local-ci/devices";

fn enroll_body(
    request_id: &str,
    owner_user: &str,
    statement: &EnrollmentStatement,
    key: &SigningKey,
) -> Value {
    json!({"request_id": request_id, "repository": format!("{}/{REPO}", statement.repository.split('/').next().unwrap()),
        "owner": owner_user, "not_after_ms": started_ms() + DAY_MS,
        "statement": statement, "signature": sign(statement, key)})
}

fn rotate_body(
    request_id: &str,
    expected: u64,
    statement: &EnrollmentStatement,
    key: &SigningKey,
) -> Value {
    json!({"request_id": request_id, "expected_generation": expected,
        "not_after_ms": started_ms() + DAY_MS, "statement": statement, "signature": sign(statement, key)})
}

/// An enrolled device `lcd_one` for alice with key seed 1, and the sessions around it.
struct Enrolled {
    env: Env,
    admin: String,
    alice: String,
    alice_computer: String,
    owner: OwnerIdentity,
    label: String,
}

impl Enrolled {
    async fn new() -> Self {
        let env = Env::start().await;
        let (alice, alice_computer) = env.sign_in(ALICE).await;
        let (admin, _) = env.sign_in(ADMIN).await;
        let owner = env.identity(ALICE);
        let label = env.label();
        let enrolled = Self {
            env,
            admin,
            alice,
            alice_computer,
            owner,
            label,
        };
        let (status, body) = enrolled.enroll("lcd_one", 1, "enroll-one").await;
        assert_eq!(status, 200, "{body}");
        enrolled
    }

    fn alice_id(&self) -> String {
        self.env.user(ALICE).id
    }

    async fn enroll(&self, device: &str, seed: u8, request_id: &str) -> (u16, Value) {
        let key = signing(seed);
        let statement = statement(&self.label, &self.owner, device, &key, 1);
        let body = enroll_body(request_id, &self.alice_id(), &statement, &key);
        self.env
            .call(Method::POST, ENROLL, Some(&self.admin), Some(body))
            .await
    }

    async fn rotate(
        &self,
        device: &str,
        expected: u64,
        seed: u8,
        request_id: &str,
    ) -> (u16, Value) {
        let key = signing(seed);
        let statement = statement(&self.label, &self.owner, device, &key, expected + 1);
        let body = rotate_body(request_id, expected, &statement, &key);
        self.env
            .call(
                Method::POST,
                &format!("{ENROLL}/{device}/rotate"),
                Some(&self.admin),
                Some(body),
            )
            .await
    }

    async fn revoke(&self, device: &str, expected: u64, request_id: &str) -> (u16, Value) {
        let body = json!({"request_id": request_id, "expected_generation": expected});
        self.env
            .call(
                Method::POST,
                &format!("{ENROLL}/{device}/revoke"),
                Some(&self.admin),
                Some(body),
            )
            .await
    }

    async fn list(&self) -> Value {
        let path = format!("{ENROLL}?repository={}", self.label);
        let (status, body) = self
            .env
            .call(Method::GET, &path, Some(&self.admin), None)
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    fn claim(&self, seed: u8, generation: u64) -> DeviceClaim {
        DeviceClaim {
            repository: self.label.clone(),
            owner: self.owner.clone(),
            device_id: "lcd_one".into(),
            key_id: issuer_key_id(&hex(signing(seed).verifying_key().as_bytes())).unwrap(),
            check_kind: LOCAL_CI_CHECK_KIND.into(),
            issued_at_ms: now_ms(),
            authorization_generation: generation,
        }
    }

    async fn decide(&self, claim: &DeviceClaim) -> DeviceDecision {
        authorize_offline_result(
            &self.env.state,
            claim,
            EvaluationClock::Known { unix_ms: now_ms() },
        )
        .await
    }
}

fn refused(why: DeviceRefusal) -> DeviceDecision {
    DeviceDecision::Refused(why)
}

#[tokio::test]
async fn admin_enrolls_rotates_revokes_and_reads_the_audit_trail() {
    let t = Enrolled::new().await;
    let listed = t.list().await;
    let device = &listed["devices"][0];
    assert_eq!(device["device_id"], "lcd_one");
    assert_eq!(device["generation"], 1);
    assert_eq!(device["repository"], t.label);
    assert_eq!(device["audit"][0]["kind"], "local_ci_device_enrolled");
    let admin_id = t.env.user(ADMIN).id;
    let enrolled = t.env.store.events_after(0, 1000).unwrap();
    let enrolled = enrolled
        .iter()
        .find(|e| e.kind == "local_ci_device_enrolled")
        .unwrap();
    let data: Value = serde_json::from_str(&enrolled.data).unwrap();
    assert_eq!(data["by"], admin_id);
    assert_ne!(data["by"], t.alice_id());

    let (status, rotated) = t.rotate("lcd_one", 1, 2, "rotate-two").await;
    assert_eq!(
        (status, &rotated["device"]["generation"]),
        (200, &json!(2)),
        "{rotated}"
    );
    assert_eq!(rotated["replayed"], false);
    let keys = rotated["device"]["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().any(|key| key["rotated_at_ms"].is_number()));

    let (status, revoked) = t.revoke("lcd_one", 2, "revoke-one").await;
    assert_eq!(status, 200, "{revoked}");
    assert_eq!(revoked["device"]["generation"], 3);
    assert!(revoked["device"]["revoked_at_ms"].is_number());
    let kinds: Vec<_> = t.list().await["devices"][0]["audit"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        kinds,
        [
            "local_ci_device_enrolled",
            "local_ci_device_rotated",
            "local_ci_device_revoked"
        ]
    );
    // A revoked device can be neither rotated nor revoked again.
    assert_eq!(
        t.revoke("lcd_one", 3, "revoke-again").await.1["error"],
        "device_revoked"
    );
    assert_eq!(
        t.rotate("lcd_one", 3, 3, "rotate-revoked").await.1["error"],
        "device_revoked"
    );
    // The audit names the acting administrator for changes.
    let events = t.env.store.local_ci_device_events("lcd_one").unwrap();
    assert!(events[1].data.contains(&t.env.user(ADMIN).id));
}

#[tokio::test]
async fn only_an_administrator_session_can_change_devices() {
    let t = Enrolled::new().await;
    let (_, agent) = {
        let (id, secret, _) = mint_bound_agent_token(
            &*t.env.store,
            &t.alice_id(),
            "synthetic-agent",
            REPO,
            REPO_ID,
            600,
            false,
        )
        .unwrap();
        (id, secret.expose().to_owned())
    };
    let (_, read_only) = {
        let (id, secret, _) = mint_bound_agent_token(
            &*t.env.store,
            &t.alice_id(),
            "synthetic-reader",
            REPO,
            REPO_ID,
            600,
            true,
        )
        .unwrap();
        (id, secret.expose().to_owned())
    };
    let key = signing(5);
    let statement = statement(&t.label, &t.owner, "lcd_two", &key, 1);
    let rotation = statement_for_rotation(&t, &key);
    let calls = [
        (
            Method::POST,
            ENROLL.to_owned(),
            enroll_body("r1", &t.alice_id(), &statement, &key),
        ),
        (
            Method::POST,
            format!("{ENROLL}/lcd_one/rotate"),
            rotate_body("r2", 1, &rotation, &key),
        ),
        (
            Method::POST,
            format!("{ENROLL}/lcd_one/revoke"),
            json!({"request_id": "r3", "expected_generation": 1}),
        ),
    ];
    let credentials: [(&str, Option<&str>); 5] = [
        ("signed-in non-admin", Some(&t.alice)),
        ("computer credential", Some(&t.alice_computer)),
        ("agent token", Some(&agent)),
        ("read-only token", Some(&read_only)),
        ("no credential", None),
    ];
    for (name, bearer) in credentials {
        for (method, path, body) in &calls {
            let (status, _) = t
                .env
                .call(method.clone(), path, bearer, Some(body.clone()))
                .await;
            assert!(matches!(status, 401 | 403), "{name} {path}: {status}");
        }
        let (status, _) = t
            .env
            .call(
                Method::GET,
                &format!("{ENROLL}?repository={}", t.label),
                bearer,
                None,
            )
            .await;
        assert!(matches!(status, 401 | 403), "{name} list: {status}");
    }
    assert_eq!(t.env.device_count(), 1);
    assert_eq!(t.env.kinds(), ["local_ci_device_enrolled"]);
}

fn statement_for_rotation(t: &Enrolled, key: &SigningKey) -> EnrollmentStatement {
    statement(&t.label, &t.owner, "lcd_one", key, 2)
}

#[tokio::test]
async fn enrollment_is_refused_unless_it_proves_this_repository_owner_and_generation() {
    let t = Enrolled::new().await;
    let alice = t.alice_id();
    let key = signing(4);
    let post = |body: Value| t.env.call(Method::POST, ENROLL, Some(&t.admin), Some(body));
    let good = statement(&t.label, &t.owner, "lcd_new", &key, 1);

    // Signed by a different key than the one it names.
    let mut body = enroll_body("e1", &alice, &good, &signing(9));
    assert_eq!(post(body.clone()).await.1["error"], "enrollment_invalid");
    // Names another repository.
    let other = statement("other.example/acme/other", &t.owner, "lcd_new", &key, 1);
    body = enroll_body("e2", &alice, &other, &key);
    body["repository"] = json!(t.label);
    assert_eq!(post(body).await.1["error"], "enrollment_invalid");
    // Names another owner.
    let bob = t.env.sign_in(BOB).await;
    drop(bob);
    let wrong_owner = statement(&t.label, &t.env.identity(BOB), "lcd_new", &key, 1);
    assert_eq!(
        post(enroll_body("e3", &alice, &wrong_owner, &key)).await.1["error"],
        "enrollment_invalid"
    );
    // Binds another generation.
    let wrong_generation = statement(&t.label, &t.owner, "lcd_new", &key, 2);
    assert_eq!(
        post(enroll_body("e4", &alice, &wrong_generation, &key))
            .await
            .1["error"],
        "enrollment_invalid"
    );
    // A key already enrolled, or a device id already taken.
    let (status, body) = t.enroll("lcd_new", 1, "e5").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("duplicate_key")),
        "{body}"
    );
    let (status, body) = t.enroll("lcd_one", 6, "e6").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("device_exists")),
        "{body}"
    );
    // Expiry already past, unknown repository, unknown owner and unknown fields.
    let mut late = enroll_body("e7", &alice, &good, &key);
    late["not_after_ms"] = json!(1);
    assert_eq!(post(late).await.1["error"], "invalid_expiry");
    let mut unknown_repo = enroll_body("e8", &alice, &good, &key);
    unknown_repo["repository"] = json!("nowhere.example/acme/none");
    assert_eq!(post(unknown_repo).await.0, 404);
    let mut unknown_owner = enroll_body("e9", &alice, &good, &key);
    unknown_owner["owner"] = json!("usr_00000000000000000000000000000000");
    assert_eq!(post(unknown_owner).await.1["error"], "unknown_owner");
    let mut extra = enroll_body("e10", &alice, &good, &key);
    extra["admin"] = json!(true);
    assert_eq!(post(extra).await.1["error"], "invalid_request");
    assert_eq!(t.env.device_count(), 1);
}

#[tokio::test]
async fn enrollment_needs_a_current_owner_role_and_a_certain_answer() {
    let t = Enrolled::new().await;
    // The owner has lost the role.
    t.env.fake.lock().unwrap().roles.remove(&ALICE);
    let (status, body) = t.enroll("lcd_new", 4, "r1").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("owner_role_missing")),
        "{body}"
    );
    // The forge can't answer.
    t.env.fake.lock().unwrap().roles.insert(ALICE, "write");
    t.env.fake.lock().unwrap().hub_reads_roles = false;
    let (status, body) = t.enroll("lcd_new", 4, "r2").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (503, Some("role_check_unavailable")),
        "{body}"
    );
    t.env.fake.lock().unwrap().hub_reads_roles = true;
    // A lasting failure names its reason and doesn't promise a retry: the forge now reports the
    // login as another person's id.
    t.env
        .fake
        .lock()
        .unwrap()
        .users
        .insert(ALICE, "other".into());
    t.env.fake.lock().unwrap().users.insert(77, "alice".into());
    let (status, body) = t.enroll("lcd_new", 4, "r2b").await;
    assert_eq!(
        (status, body["error"].as_str(), body["reason"].as_str()),
        (
            503,
            Some("role_check_unavailable"),
            Some("sign_in_required")
        ),
        "{body}"
    );
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("owner must sign in to this Hub again"),
        "{message}"
    );
    assert!(!message.contains("try again in a minute"), "{message}");
    t.env.fake.lock().unwrap().users.remove(&77);
    t.env
        .fake
        .lock()
        .unwrap()
        .users
        .insert(ALICE, "alice".into());
    // A disabled owner.
    t.env.store.set_user_disabled(&t.alice_id(), true).unwrap();
    let (status, body) = t.enroll("lcd_new", 4, "r3").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("owner_disabled")),
        "{body}"
    );
    assert_eq!(t.env.device_count(), 1);
}

const OTHER_HOST: &str = "other.example";

/// The same project id on a second forge host, where alice's id and login would match a
/// membership that belongs to an unrelated person if the hosts were not compared.
fn other_host_label(t: &Enrolled) -> String {
    let new = NewConnection {
        id: store::new_connection_id().unwrap(),
        provider: "github".into(),
        host: OTHER_HOST.into(),
        kind: "pat".into(),
        app_id: None,
        app_slug: None,
        acts_as: Some("hubbot".into()),
    };
    let connection = t
        .env
        .store
        .insert_connection(&new, &t.env.user(ADMIN).id)
        .unwrap()
        .id;
    t.env
        .store
        .upsert_repository(&connection, OTHER_HOST, REPO_ID, REPO, None, None)
        .unwrap();
    format!("{OTHER_HOST}/{REPO}")
}

#[tokio::test]
async fn an_owner_from_another_forge_host_is_refused_before_any_role_lookup() {
    let t = Enrolled::new().await;
    let label = other_host_label(&t);
    let key = signing(5);
    let statement = statement(&label, &t.owner, "lcd_far", &key, 1);
    let body = enroll_body("far-enroll", &t.alice_id(), &statement, &key);
    let reads = t.env.fake.lock().unwrap().role_reads;
    let (status, body) = t
        .env
        .call(Method::POST, ENROLL, Some(&t.admin), Some(body))
        .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("owner_wrong_forge")),
        "{body}"
    );
    assert_eq!(t.env.fake.lock().unwrap().role_reads, reads);
    assert_eq!(t.env.device_count(), 1);
}

#[tokio::test]
async fn an_offline_result_for_an_owner_from_another_forge_host_is_refused_before_any_role_lookup()
{
    let t = Enrolled::new().await;
    let label = other_host_label(&t);
    let repository = t
        .env
        .store
        .repository_named(OTHER_HOST, REPO)
        .unwrap()
        .unwrap();
    let key = signing(5);
    let authorization = DeviceAuthorization {
        version: 1,
        device_id: "lcd_far".into(),
        repository: label.clone(),
        owner: t.owner.clone(),
        generation: 1,
        revoked_at_ms: None,
        allowed_check_kinds: vec![LOCAL_CI_CHECK_KIND.into()],
        keys: vec![DeviceKey {
            key_id: issuer_key_id(&hex(key.verifying_key().as_bytes())).unwrap(),
            public_key_hex: hex(key.verifying_key().as_bytes()),
            not_before_ms: 0,
            not_after_ms: started_ms() + DAY_MS,
            rotated_at_ms: None,
        }],
    };
    let request = LocalCiRequest {
        request_id: "far-direct".into(),
        fingerprint: "0".repeat(64),
    };
    t.env
        .store
        .enroll_local_ci_device(
            &repository.id,
            &t.alice_id(),
            &t.env.user(ADMIN).id,
            &authorization,
            &request,
            0,
        )
        .unwrap();
    let claim = DeviceClaim {
        repository: label,
        device_id: "lcd_far".into(),
        key_id: authorization.keys[0].key_id.clone(),
        ..t.claim(5, 1)
    };
    let reads = t.env.fake.lock().unwrap().role_reads;
    assert_eq!(t.decide(&claim).await, refused(DeviceRefusal::WrongOwner));
    assert_eq!(t.env.fake.lock().unwrap().role_reads, reads);
}

#[tokio::test]
async fn concurrent_rotations_and_replays_follow_the_generation() {
    let t = Enrolled::new().await;
    let (a, b) = tokio::join!(
        t.rotate("lcd_one", 1, 2, "rotate-a"),
        t.rotate("lcd_one", 1, 3, "rotate-b")
    );
    let mut statuses = [a.0, b.0];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409], "{a:?} {b:?}");
    let loser = if a.0 == 409 { &a.1 } else { &b.1 };
    assert_eq!(loser["error"], "stale_generation");
    assert_eq!(loser["current_generation"], 2);
    let (winner_request, winner_seed, loser_request, loser_seed) = if a.0 == 200 {
        ("rotate-a", 2, "rotate-b", 3)
    } else {
        ("rotate-b", 3, "rotate-a", 2)
    };
    assert_eq!(t.list().await["devices"][0]["generation"], 2);
    // The same request again returns success; the loser's id was never applied.
    let (status, again) = t.rotate("lcd_one", 1, winner_seed, winner_request).await;
    assert_eq!((status, &again["replayed"]), (200, &json!(true)), "{again}");
    assert_eq!(again["device"]["generation"], 2);
    assert_eq!(
        t.rotate("lcd_one", 1, loser_seed, loser_request).await.1["error"],
        "stale_generation"
    );
    // The same id with a different request conflicts.
    assert_eq!(
        t.rotate("lcd_one", 1, 7, winner_request).await.1["error"],
        "request_conflict"
    );
    assert_eq!(
        t.env.kinds(),
        ["local_ci_device_enrolled", "local_ci_device_rotated"]
    );
    // Enrollment replays too.
    let (status, again) = t.enroll("lcd_one", 1, "enroll-one").await;
    assert_eq!((status, &again["replayed"]), (200, &json!(true)), "{again}");
}

#[tokio::test]
async fn offline_results_are_accepted_only_while_authorization_is_current() {
    let t = Enrolled::new().await;
    let current = t.decide(&t.claim(1, 1)).await;
    assert_eq!(
        current,
        DeviceDecision::Current {
            assurance: DeviceAssurance::TrustedDeviceWorkerAsserted,
            generation: 1
        }
    );

    let mut wrong_repository = t.claim(1, 1);
    wrong_repository.repository = "other.example/acme/other".into();
    assert_eq!(
        t.decide(&wrong_repository).await,
        refused(DeviceRefusal::WrongRepository)
    );
    let mut wrong_owner = t.claim(1, 1);
    wrong_owner.owner = t.env.identity(ALICE);
    wrong_owner.owner.person_id = "999".into();
    assert_eq!(
        t.decide(&wrong_owner).await,
        refused(DeviceRefusal::WrongOwner)
    );
    assert_eq!(
        t.decide(&t.claim(8, 1)).await,
        refused(DeviceRefusal::UnknownKey)
    );
    let mut unknown_device = t.claim(1, 1);
    unknown_device.device_id = "lcd_missing".into();
    assert_eq!(
        t.decide(&unknown_device).await,
        refused(DeviceRefusal::UnknownKey)
    );
    assert_eq!(
        t.decide(&t.claim(1, 5)).await,
        refused(DeviceRefusal::StaleAuthorization)
    );
    let later = EvaluationClock::Known {
        unix_ms: now_ms() + 2 * DAY_MS,
    };
    assert_eq!(
        authorize_offline_result(&t.env.state, &t.claim(1, 1), later).await,
        refused(DeviceRefusal::Expired)
    );

    // Uncertain inputs are Unknown, never Current; the role is asked every time.
    let before = t.env.fake.lock().unwrap().role_reads;
    t.decide(&t.claim(1, 1)).await;
    t.decide(&t.claim(1, 1)).await;
    assert_eq!(t.env.fake.lock().unwrap().role_reads, before + 2);
    assert_eq!(
        authorize_offline_result(&t.env.state, &t.claim(1, 1), EvaluationClock::Uncertain).await,
        DeviceDecision::Unknown(DeviceUnknown::ClockUncertain)
    );
    t.env.fake.lock().unwrap().hub_reads_roles = false;
    assert_eq!(
        t.decide(&t.claim(1, 1)).await,
        DeviceDecision::Unknown(DeviceUnknown::OwnerRoleUnavailable)
    );
    t.env.fake.lock().unwrap().hub_reads_roles = true;

    // The owner is disabled, or lost the forge role.
    t.env.store.set_user_disabled(&t.alice_id(), true).unwrap();
    assert_eq!(
        t.decide(&t.claim(1, 1)).await,
        refused(DeviceRefusal::OwnerDisabled)
    );
    t.env.store.set_user_disabled(&t.alice_id(), false).unwrap();
    t.env.fake.lock().unwrap().roles.remove(&ALICE);
    assert_eq!(
        t.decide(&t.claim(1, 1)).await,
        refused(DeviceRefusal::OwnerRoleMissing)
    );
    t.env.fake.lock().unwrap().roles.insert(ALICE, "write");
    assert_eq!(t.decide(&t.claim(1, 1)).await, current);
}

#[tokio::test]
async fn a_revocation_during_the_role_lookup_is_seen() {
    let t = Enrolled::new().await;
    let (store, request) = (
        t.env.store.clone(),
        LocalCiRequest {
            request_id: "revoke-during".into(),
            fingerprint: "f".repeat(64),
        },
    );
    t.env.fake.lock().unwrap().during_role_read = Some(Box::new(move || {
        let stored = store.local_ci_device("lcd_one").unwrap().unwrap();
        let next = stored.authorization.revoke(now_ms()).unwrap();
        store
            .change_local_ci_device("lcd_one", 1, &stored.owner, &next, &request, 0)
            .unwrap();
    }));
    assert_eq!(
        t.decide(&t.claim(1, 1)).await,
        refused(DeviceRefusal::StaleAuthorization)
    );
}

#[tokio::test]
async fn rotation_and_revocation_end_earlier_claims() {
    let t = Enrolled::new().await;
    let earlier = t.claim(1, 1);
    assert!(matches!(
        t.decide(&earlier).await,
        DeviceDecision::Current { .. }
    ));

    assert_eq!(t.rotate("lcd_one", 1, 2, "rotate-two").await.0, 200);
    // The worker's generation is stale, and the old key is rotated even at the new one.
    assert_eq!(
        t.decide(&earlier).await,
        refused(DeviceRefusal::StaleAuthorization)
    );
    assert_eq!(
        t.decide(&t.claim(1, 2)).await,
        refused(DeviceRefusal::Rotated)
    );
    assert!(matches!(
        t.decide(&t.claim(2, 2)).await,
        DeviceDecision::Current { .. }
    ));

    assert_eq!(t.revoke("lcd_one", 2, "revoke-one").await.0, 200);
    assert_eq!(
        t.decide(&t.claim(2, 2)).await,
        refused(DeviceRefusal::StaleAuthorization)
    );
    assert_eq!(
        t.decide(&t.claim(2, 3)).await,
        refused(DeviceRefusal::Revoked)
    );
}

#[tokio::test]
async fn a_renamed_repository_is_not_current() {
    let t = Enrolled::new().await;
    assert!(matches!(
        t.decide(&t.claim(1, 1)).await,
        DeviceDecision::Current { .. }
    ));
    let connection = t.env.store.pat_connection(&t.env.host).unwrap().unwrap().id;
    t.env
        .store
        .upsert_repository(
            &connection,
            &t.env.host,
            REPO_ID,
            "acme/renamed",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        t.decide(&t.claim(1, 1)).await,
        refused(DeviceRefusal::WrongRepository)
    );
    let mut renamed = t.claim(1, 1);
    renamed.repository = format!("{}/acme/renamed", t.env.host);
    assert_eq!(
        t.decide(&renamed).await,
        refused(DeviceRefusal::WrongRepository)
    );
}

impl Enrolled {
    fn rename_repository(&self, name: &str) {
        let connection = self
            .env
            .store
            .pat_connection(&self.env.host)
            .unwrap()
            .unwrap()
            .id;
        self.env
            .store
            .upsert_repository(&connection, &self.env.host, REPO_ID, name, None, None)
            .unwrap();
    }
}

#[tokio::test]
async fn rotation_is_refused_while_the_repository_name_differs() {
    let t = Enrolled::new().await;
    t.rename_repository("acme/renamed");
    let (status, body) = t.rotate("lcd_one", 1, 2, "rotate-renamed").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("repository_renamed")),
        "{body}"
    );
    // Renaming back restores authorization, and rotation then works.
    t.rename_repository(REPO);
    assert!(matches!(
        t.decide(&t.claim(1, 1)).await,
        DeviceDecision::Current { .. }
    ));
    assert_eq!(t.rotate("lcd_one", 1, 2, "rotate-back").await.0, 200);
}

#[tokio::test]
async fn a_retry_after_a_lost_response_replays_before_other_checks() {
    let t = Enrolled::new().await;
    let (status, first) = t.enroll("lcd_new", 4, "retry-1").await;
    assert_eq!(status, 200, "{first}");
    // The owner is disabled and the forge is unavailable by the time the retry arrives.
    t.env.fake.lock().unwrap().hub_reads_roles = false;
    t.env.store.set_user_disabled(&t.alice_id(), true).unwrap();
    let (status, again) = t.enroll("lcd_new", 4, "retry-1").await;
    assert_eq!((status, &again["replayed"]), (200, &json!(true)), "{again}");
    assert_eq!(again["device"]["device_id"], "lcd_new");
    // The same id with a different request still conflicts.
    let (status, other) = t.enroll("lcd_new", 5, "retry-1").await;
    assert_eq!(
        (status, other["error"].as_str()),
        (409, Some("request_conflict")),
        "{other}"
    );
    // A new request still meets the owner check.
    assert_eq!(
        t.enroll("lcd_other", 6, "retry-2").await.1["error"],
        "owner_disabled"
    );
    assert_eq!(t.env.device_count(), 2);
}

#[tokio::test]
async fn malformed_requests_are_client_errors() {
    let t = Enrolled::new().await;
    let key = signing(4);
    let enroll = statement(&t.label, &t.owner, "lcd_new", &key, 1);
    let rotation = statement(&t.label, &t.owner, "lcd_one", &key, 2);
    let mut cases = vec![];
    for (request_id, not_after, error) in [
        ("bad id", None, "invalid_request_id"),
        ("", None, "invalid_request_id"),
        ("ok-id", Some(u64::MAX), "invalid_expiry"),
        ("ok-id", Some(0), "invalid_expiry"),
    ] {
        let mut enrolling = enroll_body(request_id, &t.alice_id(), &enroll, &key);
        let mut rotating = rotate_body(request_id, 1, &rotation, &key);
        if let Some(not_after) = not_after {
            enrolling["not_after_ms"] = json!(not_after);
            rotating["not_after_ms"] = json!(not_after);
        }
        cases.push((ENROLL.to_owned(), enrolling, error));
        cases.push((format!("{ENROLL}/lcd_one/rotate"), rotating, error));
    }
    cases.push((
        format!("{ENROLL}/lcd_one/revoke"),
        json!({"request_id": "bad id", "expected_generation": 1}),
        "invalid_request_id",
    ));
    for (path, body, error) in cases {
        let (status, reply) = t
            .env
            .call(Method::POST, &path, Some(&t.admin), Some(body))
            .await;
        assert_eq!(
            (status, reply["error"].as_str()),
            (400, Some(error)),
            "{path} {reply}"
        );
    }
    // Only a key already enrolled on the device is a duplicate key.
    let (status, reply) = t.rotate("lcd_one", 1, 1, "duplicate").await;
    assert_eq!(
        (status, reply["error"].as_str()),
        (409, Some("duplicate_key")),
        "{reply}"
    );
    assert_eq!(t.env.device_count(), 1);
}

/// Pins today's threshold: the sign-in rule, so any permission above `none` passes on a
/// private repository, including read. Issue #273 or a maintainer may tighten it.
#[tokio::test]
async fn a_read_only_owner_of_a_private_repository_still_passes_the_role_check() {
    let t = Enrolled::new().await;
    t.env.fake.lock().unwrap().roles.insert(ALICE, "read");
    assert!(matches!(
        t.decide(&t.claim(1, 1)).await,
        DeviceDecision::Current { .. }
    ));
    assert_eq!(t.enroll("lcd_reader", 4, "reader").await.0, 200);
}

#[tokio::test]
async fn an_owner_reads_only_their_own_devices_and_changes_nothing() {
    let t = Enrolled::new().await;
    let (bob, _) = t.env.sign_in(BOB).await;
    let bob_owner = t.env.identity(BOB);
    let key = signing(6);
    let statement = statement(&t.label, &bob_owner, "lcd_bob", &key, 1);
    let body = enroll_body("bob-enroll", &t.env.user(BOB).id, &statement, &key);
    assert_eq!(
        t.env
            .call(Method::POST, ENROLL, Some(&t.admin), Some(body))
            .await
            .0,
        200
    );

    let path = format!("/v1/hub/local-ci/devices?repository={}", t.label);
    for (session, expected) in [
        (&t.alice, "lcd_one"),
        (&bob, "lcd_bob"),
        (&t.alice_computer, "lcd_one"),
    ] {
        let (status, body) = t.env.call(Method::GET, &path, Some(session), None).await;
        assert_eq!(status, 200, "{body}");
        let devices = body["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 1, "{body}");
        assert_eq!(devices[0]["device_id"], expected);
        assert_eq!(devices[0]["generation"], 1);
        assert!(devices[0].get("audit").is_none());
    }
    assert_eq!(t.env.call(Method::GET, &path, None, None).await.0, 401);
    // An agent token is not an owner credential.
    let (_, agent, _) = mint_bound_agent_token(
        &*t.env.store,
        &t.alice_id(),
        "synthetic-agent",
        REPO,
        REPO_ID,
        600,
        false,
    )
    .unwrap();
    let (status, _) = t
        .env
        .call(Method::GET, &path, Some(agent.expose()), None)
        .await;
    assert!(matches!(status, 401 | 403), "agent token: {status}");
    // An unknown repository is an empty list, like a repository with no devices, so the
    // route doesn't reveal which repositories are connected.
    let path = format!(
        "/v1/hub/local-ci/devices?repository={}/acme/unconnected",
        t.env.host
    );
    let (status, body) = t.env.call(Method::GET, &path, Some(&t.alice), None).await;
    assert_eq!((status, &body["devices"]), (200, &json!([])), "{body}");
    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        let (status, _) = t
            .env
            .call(
                method,
                "/v1/hub/local-ci/devices",
                Some(&t.alice),
                Some(json!({})),
            )
            .await;
        assert_eq!(status, 405);
    }
    assert_eq!(t.env.device_count(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn the_device_cli_request_is_accepted_by_the_enroll_route() {
    let env = Env::start().await;
    let (admin, _) = env.sign_in(ADMIN).await;
    env.sign_in(ALICE).await;
    let (owner, label) = (env.identity(ALICE), env.label());
    let dir = TempDir::new().unwrap();
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let key = dir.path().canonicalize().unwrap().join("device.key");
    // The server runs on this runtime, so the child process is awaited off-thread.
    let run = |args: Vec<String>| {
        let cwd = dir.path().to_owned();
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_agentscm"))
                .current_dir(cwd)
                .args(["local-ci", "device"])
                .args(args)
                .output()
                .unwrap()
        })
    };
    let key_path = key.display().to_string();
    let generated = run(vec![
        "key-generate".into(),
        "--key".into(),
        key_path.clone(),
    ])
    .await
    .unwrap();
    assert!(generated.status.success(), "{generated:?}");
    let request = run([
        "enroll-request",
        "--key",
        &key_path,
        "--device-id",
        "lcd_cli",
        "--repository",
        &label,
        "--owner-provider",
        &owner.provider,
        "--owner-host",
        &owner.host,
        "--owner-person",
        &owner.person_id,
        "--generation",
        "1",
    ]
    .map(String::from)
    .to_vec())
    .await
    .unwrap();
    assert!(request.status.success(), "{request:?}");
    let request: Value = serde_json::from_slice(&request.stdout).unwrap();
    let body = json!({"request_id": "cli-enroll", "repository": label,
        "owner": env.user(ALICE).id, "not_after_ms": started_ms() + DAY_MS,
        "statement": request["statement"], "signature": request["signature"]});
    let (status, reply) = env
        .call(Method::POST, ENROLL, Some(&admin), Some(body))
        .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(env.device_count(), 1);
}

#[tokio::test]
async fn no_secret_or_extra_surface_is_exposed() {
    let env = Env::start().await;
    let (alice, _) = env.sign_in(ALICE).await;
    let (admin, _) = env.sign_in(ADMIN).await;
    drop(alice);
    let mut seed = [0u8; 32];
    seed[..CANARY.len()].copy_from_slice(CANARY.as_bytes());
    let secret = SigningKey::from_bytes(&seed);
    let (owner, label) = (env.identity(ALICE), env.label());
    let alice_id = env.user(ALICE).id;
    let enrolled = statement(&label, &owner, "lcd_canary", &secret, 1);
    let body = enroll_body("canary-enroll", &alice_id, &enrolled, &secret);
    assert_eq!(
        env.call(Method::POST, ENROLL, Some(&admin), Some(body))
            .await
            .0,
        200
    );
    let rotated = statement(&label, &owner, "lcd_canary", &signing(2), 2);
    let body = rotate_body("canary-rotate", 1, &rotated, &signing(2));
    let path = format!("{ENROLL}/lcd_canary/rotate");
    assert_eq!(
        env.call(Method::POST, &path, Some(&admin), Some(body.clone()))
            .await
            .0,
        200
    );
    // An error reply too.
    assert_eq!(
        env.call(Method::POST, &path, Some(&admin), Some(body))
            .await
            .0,
        200
    );
    let stale = rotate_body("canary-stale", 1, &rotated, &signing(2));
    assert_eq!(
        env.call(Method::POST, &path, Some(&admin), Some(stale))
            .await
            .0,
        409
    );
    env.call(
        Method::GET,
        &format!("{ENROLL}?repository={label}"),
        Some(&admin),
        None,
    )
    .await;

    let mut everything = env.seen.lock().unwrap().join("\n");
    for event in env.store.events_after(0, 1000).unwrap() {
        everything.push_str(&event.data);
    }
    for secret in [CANARY.to_owned(), hex(CANARY.as_bytes()), hex(&seed)] {
        assert!(!everything.contains(&secret));
    }
    // Only public key material is stored.
    assert!(everything.contains(&hex(secret.verifying_key().as_bytes())));

    // Nothing here submits results, edits policy or publishes forge statuses.
    for path in [
        "/v1/hub/admin/local-ci/results",
        "/v1/hub/admin/local-ci/policy",
        "/v1/hub/local-ci/results",
        "/v1/hub/local-ci/devices/lcd_canary/results",
    ] {
        let (status, _) = env
            .call(Method::POST, path, Some(&admin), Some(json!({})))
            .await;
        assert!(matches!(status, 404 | 405), "{path}: {status}");
    }
}
