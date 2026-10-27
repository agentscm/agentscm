//! GitHub coordination sync against an in-process fake GitHub: no live accounts.
//! Sign-in and minting rules are in hub_login.rs, where the fake forge speaks OAuth.
#[path = "support/fake_github.rs"]
mod fake_github;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscm::backend::LocalBackend;
use agentscm::hub::connection::ConnectedRepository;
use agentscm::hub::login::{Login, LoginConfig};
use agentscm::hub::store::{
    self, ForgeIdentity, Store, mint_agent_token, mint_bound_agent_token, unix_now,
};
use agentscm::hub::sync::{self, PollReport, Sync, SyncConfig};
use agentscm::hub::{HubState, router};
use axum::extract::Form;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use fake_github::{Branch, Comment, Failure, FakeGithub, Installation, Pull, Repo};
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 100;
const PAT: &str = "ghp_synthetic_pat";
const PAT_HOOK_SECRET: &str = "pat-hook-secret-synthetic";

// ---- a tiny OAuth server so an administrator can sign in ----

async fn oauth_shim() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let shown = base.clone();
    let app = Router::new()
        .route(
            "/login/device/code",
            post(move |Form(_): Form<HashMap<String, String>>| {
                let base = shown.clone();
                async move {
                    Json(json!({"device_code": "dev", "user_code": "ABCD-1234",
                        "verification_uri": format!("{base}/device"), "expires_in": 900, "interval": 1}))
                }
            }),
        )
        .route(
            "/login/oauth/access_token",
            post(|Form(_): Form<HashMap<String, String>>| async {
                Json(json!({"access_token": "shim-token", "expires_in": 7200}))
            }),
        )
        .route(
            "/user",
            get(|headers: HeaderMap| async move {
                if headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
                    == Some("Bearer shim-token")
                {
                    Json(json!({"id": 9, "login": "adminal"})).into_response()
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            }),
        )
        .with_state(());
    tokio::spawn(async move { axum::serve(listener, app).await });
    base
}

// ---- the hub under test ----

struct Env {
    fake: FakeGithub,
    url: String,
    state: HubState,
    store: Arc<dyn Store>,
    secrets: std::path::PathBuf,
    http: reqwest::Client,
    admin: String,
    /// Every body the hub sent, for secrecy checks.
    log: std::sync::Mutex<Vec<String>>,
    _dir: TempDir,
}

fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn days_ago(days: i64) -> String {
    rfc3339(unix_now() - days * 86_400)
}

impl Env {
    async fn start() -> Self {
        Self::start_with(false).await
    }

    async fn start_with(pat: bool) -> Self {
        Self::start_hub(pat, true).await
    }

    /// Without `login`, no one can sign in: the state of a hub on its first run.
    async fn start_hub(pat: bool, login: bool) -> Self {
        let fake = FakeGithub::start().await;
        if pat {
            fake.with(|s| {
                s.pats
                    .insert(PAT.into(), ("robot".into(), 77, [REPO_ID].into()));
            });
        }
        let oauth = oauth_shim().await;
        let dir = TempDir::new().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let secrets = dir.path().join("secrets");
        sync::prepare_secrets_dir(&secrets, true).unwrap();
        let base = fake.base.clone();
        let sync_config = SyncConfig::from_lookup(&url, secrets.clone(), 14, |name| match name {
            "AGENTSCM_GITHUB_URL" | "AGENTSCM_GITHUB_API_URL" => Some(base.clone()),
            "AGENTSCM_GITHUB_PAT" if pat => Some(PAT.into()),
            "AGENTSCM_GITHUB_PAT_WEBHOOK_SECRET" if pat => Some(PAT_HOOK_SECRET.into()),
            _ => None,
        })
        .unwrap();
        let login_config = LoginConfig::from_lookup(&url, |name| match name {
            "AGENTSCM_GITHUB_CLIENT_ID" => Some("client".into()),
            "AGENTSCM_GITHUB_CLIENT_SECRET" => Some("client-secret-for-login".into()),
            "AGENTSCM_GITHUB_URL" | "AGENTSCM_GITHUB_API_URL" => Some(oauth.clone()),
            "AGENTSCM_ADMINS" => Some("github:#9".into()),
            _ => None,
        })
        .unwrap();
        let mut state = HubState::new(
            store.clone(),
            std::sync::Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
        );
        if login {
            state = state.with_login(Login::new(login_config).unwrap());
        }
        let state = state.with_sync(Sync::new(sync_config).unwrap());
        sync::start(&state).await.unwrap();
        let app = router(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let mut env = Self {
            fake,
            url,
            state,
            store,
            secrets,
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            admin: String::new(),
            log: Default::default(),
            _dir: dir,
        };
        if login {
            env.admin = env.sign_in_admin().await;
        }
        env
    }

    async fn sign_in_admin(&self) -> String {
        let (_, started) = self
            .post("/v1/hub/login/github/device", None, json!({}))
            .await;
        let (status, signed) = self
            .post(
                "/v1/hub/login/device/poll",
                None,
                json!({"handle": started["handle"]}),
            )
            .await;
        assert_eq!(
            (status, signed["admin"].clone()),
            (200, json!(true)),
            "{signed}"
        );
        signed["session"].as_str().unwrap().to_owned()
    }

    fn host(&self) -> String {
        self.fake.base.trim_start_matches("http://").to_owned()
    }

    async fn raw(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (u16, String) {
        let mut request = self.http.request(method, format!("{}{path}", self.url));
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
        self.log.lock().unwrap().push(text.clone());
        (status, text)
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let (status, text) = self.raw(method, path, bearer, body).await;
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str, bearer: Option<&str>) -> (u16, Value) {
        self.call(Method::GET, path, bearer, None).await
    }

    async fn post(&self, path: &str, bearer: Option<&str>, body: Value) -> (u16, Value) {
        self.call(Method::POST, path, bearer, Some(body)).await
    }

    /// Creates the GitHub App through the manifest flow; returns the connection id.
    async fn connect_app(&self) -> String {
        let admin = self.admin.clone();
        let (status, created) = self
            .post(
                "/v1/hub/github/apps",
                Some(&admin),
                json!({"name": "agentscm-test"}),
            )
            .await;
        assert_eq!(status, 200, "{created}");
        let setup = created["setup_url"].as_str().unwrap();
        let secret = setup.rsplit('/').next().unwrap();
        self.fake.with(|s| {
            s.manifest_codes.insert("code-1".into());
        });
        let (status, _) = self
            .raw(
                Method::GET,
                &format!("/v1/hub/github/setup/callback?code=code-1&state={secret}"),
                None,
                None,
            )
            .await;
        assert_eq!(status, 200);
        created["connection"].as_str().unwrap().to_owned()
    }

    /// The fake has `acme/widgets` (id 100) installed for the app.
    fn install_widgets(&self) {
        self.fake.with(|s| {
            let mut repo = Repo::new(REPO_ID, REPO);
            repo.private = true;
            s.repos.insert(REPO_ID, repo);
            s.installations.push(Installation {
                id: 7,
                account: "acme".into(),
                repos: vec![REPO_ID],
            });
        });
    }

    async fn refresh(&self, connection: &str) -> Vec<Value> {
        let admin = self.admin.clone();
        let (status, reply) = self
            .post(
                &format!("/v1/hub/connections/{connection}/refresh"),
                Some(&admin),
                json!({}),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        reply["reports"].as_array().unwrap().clone()
    }

    fn repo(&self) -> ConnectedRepository {
        self.store
            .repository_named(&self.host(), REPO)
            .unwrap()
            .unwrap()
    }

    async fn poll(&self) -> Vec<PollReport> {
        sync::poll_once(&self.state).await.unwrap()
    }

    /// An agent token for `REPO` owned by a new user on the fake's host.
    fn agent(&self, user_id: i64, login: &str, name: &str) -> String {
        let owner = self
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: self.host(),
                    user_id,
                },
                login,
            )
            .unwrap();
        let (_, secret) = mint_agent_token(&*self.store, &owner.id, name, REPO, 3600).unwrap();
        secret.expose().to_owned()
    }

    async fn announce(&self, token: &str, n: u128, paths: &[&str], branch: Option<&str>) -> Value {
        let body = json!({"intent_id": format!("int_{n:032x}"), "task": "rework billing",
            "branch": branch, "paths": paths, "ttl_s": 3600});
        let (status, reply) = self
            .post("/v1/hub/repos/acme%2Fwidgets/intents", Some(token), body)
            .await;
        assert_eq!(status, 200, "{reply}");
        reply
    }

    async fn work(&self, token: &str) -> Value {
        let (status, work) = self
            .get("/v1/hub/repos/acme%2Fwidgets/work", Some(token))
            .await;
        assert_eq!(status, 200);
        work
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| serde_json::from_str(&e.data).unwrap())
            .collect()
    }

    fn event_count(&self) -> usize {
        self.store.events_after(0, 1000).unwrap().len()
    }

    fn observed(&self, kind: &str) -> BTreeSet<String> {
        self.store
            .observations(&self.repo().id, kind)
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect()
    }

    fn pr_data(&self, number: u64) -> Value {
        let rows = self
            .store
            .observations(&self.repo().id, "pull_request")
            .unwrap();
        let row = rows
            .iter()
            .find(|o| o.key == number.to_string())
            .expect("observed pull request");
        serde_json::from_str(&row.data).unwrap()
    }

    fn fake_writes(&self) -> usize {
        self.fake.with(|s| {
            s.requests
                .iter()
                .filter(|r| {
                    r.method != "GET"
                        && (r.path_and_query.contains("/comments")
                            || r.path_and_query.contains("/statuses/"))
                })
                .count()
        })
    }

    fn pull(&self, number: u64, f: impl FnOnce(&mut Pull)) {
        self.fake.with(|s| {
            f(s.repos
                .get_mut(&REPO_ID)
                .unwrap()
                .pulls
                .get_mut(&number)
                .unwrap())
        });
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn add_pull(env: &Env, number: u64, files: &[&str]) {
    env.fake.with(|s| {
        let repo = s.repos.get_mut(&REPO_ID).unwrap();
        repo.pulls
            .insert(number, Pull::new(number, &format!("feat/{number}"), files));
    });
}

fn add_branch(env: &Env, name: &str, age_days: i64) {
    env.fake.with(|s| {
        s.repos
            .get_mut(&REPO_ID)
            .unwrap()
            .branches
            .push(Branch::new(name, &days_ago(age_days)));
    });
}

fn is_mode_0600(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777 == 0o600
}

// ---- 1. the manifest flow ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manifest_flow_creates_the_app_and_keeps_secrets_off_the_wire() {
    let env = Env::start().await;
    let admin = env.admin.clone();
    // Only an administrator starts it.
    let body = json!({"name": "agentscm-test"});
    assert_eq!(
        env.post("/v1/hub/github/apps", None, body.clone()).await.0,
        401
    );
    assert_eq!(
        env.post(
            "/v1/hub/github/apps",
            Some(&admin),
            json!({"name": "bad/name"})
        )
        .await
        .0,
        400
    );
    let (status, created) = env.post("/v1/hub/github/apps", Some(&admin), body).await;
    assert_eq!(status, 200);
    let (setup, connection) = (
        created["setup_url"].as_str().unwrap().to_owned(),
        created["connection"].as_str().unwrap().to_owned(),
    );
    let secret = setup.rsplit('/').next().unwrap().to_owned();
    assert!(setup.starts_with(&format!("{}/v1/hub/github/setup/", env.url)));

    // The setup page posts the manifest to GitHub, carrying the state.
    let (status, page) = env
        .raw(
            Method::GET,
            &format!("/v1/hub/github/setup/{secret}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert!(page.contains(&format!(
        "{}/settings/apps/new?state={secret}",
        env.fake.base
    )));
    // This hub is on a loopback http URL, which GitHub cannot call: no webhook is declared.
    assert!(page.contains("name=manifest") && !page.contains("&quot;default_events&quot;"));
    assert!(!page.contains(&format!("/v1/hub/webhooks/github/{connection}")));
    assert!(page.contains("/v1/hub/github/setup/callback"));
    assert_eq!(
        env.raw(Method::GET, "/v1/hub/github/setup/nope", None, None)
            .await
            .0,
        404
    );

    // GitHub calls back with a code; the hub converts it and stores the credentials.
    env.fake.with(|s| {
        s.manifest_codes.insert("code-1".into());
    });
    let callback = format!("/v1/hub/github/setup/callback?code=code-1&state={secret}");
    let (status, done) = env.raw(Method::GET, &callback, None, None).await;
    assert_eq!(status, 200);
    assert!(done.contains("Created app agentscm-test"), "{done}");
    assert!(done.contains("/apps/agentscm-test/installations/new"));
    // Device sign-in is a checkbox the manifest cannot set, so the page names it.
    assert!(
        done.contains("Enable Device Flow") && done.contains("/settings/apps/agentscm-test"),
        "{done}"
    );
    // The state is single use.
    assert_eq!(env.raw(Method::GET, &callback, None, None).await.0, 400);

    for name in [
        format!("{connection}.json"),
        format!("{connection}.client_secret"),
    ] {
        let path = env.secrets.join(&name);
        assert!(is_mode_0600(&path), "{name}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains(fake_github::CLIENT_SECRET)
        );
    }
    let file: Value = serde_json::from_str(
        &std::fs::read_to_string(env.secrets.join(format!("{connection}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(file["app_id"], fake_github::APP_ID);
    assert_eq!(file["webhook_secret"], fake_github::WEBHOOK_SECRET);
    assert!(
        file["pem"]
            .as_str()
            .unwrap()
            .contains("BEGIN RSA PRIVATE KEY")
    );

    // No response and no event holds the key, the webhook secret or the client secret.
    let (_, listed) = env.get("/v1/hub/connections", Some(&admin)).await;
    assert_eq!(
        listed["connections"][0]["label"],
        "GitHub App agentscm-test"
    );
    assert_eq!(listed["connections"][0]["id"], json!(connection));
    let key_line = fake_github::private_pem()
        .lines()
        .nth(2)
        .unwrap()
        .to_owned();
    let events: String = env
        .store
        .events_after(0, 1000)
        .unwrap()
        .iter()
        .map(|e| e.data.clone())
        .collect();
    let bodies = env.log.lock().unwrap().join("\n");
    for needle in [
        key_line.as_str(),
        fake_github::WEBHOOK_SECRET,
        fake_github::CLIENT_SECRET,
    ] {
        assert!(
            !bodies.contains(needle) && !events.contains(needle),
            "{needle}"
        );
    }
    // The client id is public: the success page prints it for the sign-in settings.
    assert!(done.contains("AGENTSCM_GITHUB_CLIENT_ID=Iv1.synthetic"));
    assert!(!events.contains("Iv1.synthetic"));
    assert_eq!(env.events("connection_created").len(), 1);

    // A code GitHub does not know creates nothing.
    let (_, again) = env
        .post(
            "/v1/hub/github/apps",
            Some(&admin),
            json!({"name": "second"}),
        )
        .await;
    let second = again["setup_url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let bad = format!("/v1/hub/github/setup/callback?code=unknown&state={second}");
    assert_eq!(env.raw(Method::GET, &bad, None, None).await.0, 503);
    assert_eq!(env.events("connection_created").len(), 1);
}

// ---- 1b. first-run bootstrap ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_run_writes_a_one_time_setup_link_that_creates_the_app() {
    let env = Env::start_hub(false, false).await;
    let file = env.secrets.join("bootstrap-setup-url");
    let path = sync::bootstrap(&env.state, false, |_| None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(path, file);
    assert!(is_mode_0600(&file));
    let link = std::fs::read_to_string(&file).unwrap();
    let secret = link.rsplit('/').next().unwrap().to_owned();
    assert_eq!(link, format!("{}/v1/hub/github/setup/{secret}", env.url));
    assert!(secret.len() >= 32 && secret.bytes().all(|b| b.is_ascii_hexdigit()));

    // The link works once; the app is made without a signed-in admin.
    env.fake.with(|s| {
        s.manifest_codes.insert("code-1".into());
    });
    let setup = format!("/v1/hub/github/setup/{secret}");
    let (status, page) = env.raw(Method::GET, &setup, None, None).await;
    assert_eq!(status, 200);
    assert!(page.contains("agentscm-hub"), "{page}");
    let callback = format!("/v1/hub/github/setup/callback?code=code-1&state={secret}");
    let (status, done) = env.raw(Method::GET, &callback, None, None).await;
    assert_eq!(status, 200, "{done}");
    let connection = env.store.connections().unwrap()[0].id.clone();
    assert!(
        done.contains("AGENTSCM_GITHUB_CLIENT_ID=Iv1.synthetic"),
        "{done}"
    );
    assert!(done.contains(&format!(
        "AGENTSCM_GITHUB_CLIENT_SECRET_FILE={}",
        env.secrets.join(format!("{connection}.client_secret")).display()
    )));
    assert!(done.contains("AGENTSCM_ADMINS=github:"));
    assert!(!done.contains(fake_github::CLIENT_SECRET));
    assert_eq!(env.raw(Method::GET, &callback, None, None).await.0, 400);
    assert_eq!(env.raw(Method::GET, &setup, None, None).await.0, 404);
    assert!(!file.exists(), "the link goes once it is used");
    assert_eq!(env.events("connection_created")[0]["by"], "bootstrap");

    // A connection now exists: a restart makes no second link.
    assert!(
        sync::bootstrap(&env.state, false, |_| None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!file.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_run_link_only_without_users_connections_or_github_sign_in() {
    let env = Env::start_hub(false, false).await;
    let file = env.secrets.join("bootstrap-setup-url");
    // GitHub sign-in configured: an administrator can use the API instead.
    assert!(
        sync::bootstrap(&env.state, true, |_| None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!file.exists());
    // A bad app name is refused and writes nothing.
    let bad = |name: &str| (name == "AGENTSCM_GITHUB_APP_NAME").then(|| "bad/name".to_owned());
    assert!(sync::bootstrap(&env.state, false, bad).await.is_err());
    assert!(!file.exists());
    // The configured name and organization are used.
    let named = |name: &str| match name {
        "AGENTSCM_GITHUB_APP_NAME" => Some("team-hub".to_owned()),
        "AGENTSCM_GITHUB_APP_ORG" => Some("acme".to_owned()),
        _ => None,
    };
    assert!(
        sync::bootstrap(&env.state, false, named)
            .await
            .unwrap()
            .is_some()
    );
    let link = std::fs::read_to_string(&file).unwrap();
    let (_, page) = env
        .raw(
            Method::GET,
            &format!("/v1/hub/github/setup/{}", link.rsplit('/').next().unwrap()),
            None,
            None,
        )
        .await;
    assert!(
        page.contains("/organizations/acme/settings/apps/new"),
        "{page}"
    );
    assert!(page.contains("team-hub"));
    // Any user ends it, and removes the stale link.
    env.store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: env.host(),
                user_id: 5,
            },
            "someone",
        )
        .unwrap();
    assert!(
        sync::bootstrap(&env.state, false, |_| None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!file.exists());
    // An unusable secrets directory is an error that does not leak a link.
    let blocked = Env::start_hub(false, false).await;
    std::fs::remove_dir_all(&blocked.secrets).unwrap();
    std::fs::write(&blocked.secrets, b"not a directory").unwrap();
    let error = sync::bootstrap(&blocked.state, false, |_| None)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("/v1/hub/github/setup/"), "{error}");
}

// ---- 2. installation discovery ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_connects_the_installations_repositories() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.fake.with(|s| {
        s.repos.insert(101, Repo::new(101, "acme/gadgets"));
        s.repos.insert(102, Repo::new(102, "acme/hidden"));
        s.installations[0].repos.push(101);
    });
    // Installed repositories are not connected until the hub looks.
    assert!(env.store.repositories(&connection).unwrap().is_empty());
    let reports = env.refresh(&connection).await;
    assert_eq!(reports.len(), 2);
    let connected = env.events("repository_connected");
    let mut names: Vec<_> = connected
        .iter()
        .map(|e| e["full_name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["acme/gadgets", "acme/widgets"]);
    let widgets = connected.iter().find(|e| e["full_name"] == REPO).unwrap();
    assert_eq!(
        (
            widgets["repository_id"].clone(),
            widgets["node_id"].clone(),
            widgets["installation_id"].clone()
        ),
        (json!(100), json!("R_100"), json!(7))
    );
    // The app's own calls used a JWT; repository reads used an installation token.
    env.fake.with(|s| {
        let auth = |path: &str| {
            s.requests
                .iter()
                .find(|r| r.path_and_query.starts_with(path))
                .unwrap()
                .auth
                .clone()
                .unwrap()
        };
        assert!(auth("/app/installations?").starts_with("Bearer ey"));
        assert!(auth("/installation/repositories").starts_with("Bearer ghs_"));
        assert!(auth("/repos/acme/widgets/pulls").starts_with("Bearer ghs_"));
        // One installation token served every read of the installation.
        assert_eq!(s.installation_tokens.len(), 1);
    });
    // A rename on GitHub keeps the row and records the change.
    env.fake
        .with(|s| s.repos.get_mut(&REPO_ID).unwrap().full_name = "acme/widgets-renamed".into());
    env.refresh(&connection).await;
    assert_eq!(env.store.repositories(&connection).unwrap().len(), 2);
    assert_eq!(env.events("repository_renamed")[0]["previous"], REPO);
    let renamed = env
        .store
        .repository_by_id(&env.host(), REPO_ID)
        .unwrap()
        .unwrap();
    assert_eq!(renamed.full_name, "acme/widgets-renamed");
    assert_eq!(env.events("repository_connected").len(), 2);
}

// ---- 3. webhooks ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_are_verified_deduplicated_and_re_read() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.refresh(&connection).await;
    add_pull(&env, 5, &["src/a.rs"]);
    let secret = fake_github::WEBHOOK_SECRET;
    let hook = |event: &'static str, delivery: &'static str, body: Value, secret: &'static str| {
        let url = env.url.clone();
        let connection = connection.clone();
        async move { fake_github::deliver(&url, &connection, event, delivery, &body, secret).await }
    };
    let payload = json!({"action": "opened", "number": 5, "repository": {"id": REPO_ID},
        "pull_request": {"number": 5, "title": "never trusted"}});

    assert_eq!(
        hook("pull_request", "d-1", payload.clone(), secret).await,
        202
    );
    eventually("the pull request", || {
        env.observed("pull_request").len() == 1
    })
    .await;
    let observed = env.pr_data(5);
    // State comes from GitHub, not from the payload.
    assert_eq!(observed["title"], "Pull request 5");
    assert_eq!(observed["files"], json!(["src/a.rs"]));
    let events = env.events("pull_request_observed");
    assert_eq!(
        (
            events[0]["repository_id"].clone(),
            events[0]["number"].clone(),
            events[0]["node_id"].clone()
        ),
        (json!(100), json!(5), json!("PR_5"))
    );
    assert!(events[0]["head_sha"].as_str().unwrap().len() == 40);

    // A bad signature changes nothing, and does not use up the delivery id.
    env.pull(5, |p| p.title = "changed on github".into());
    let before = (env.event_count(), env.fake.with(|s| s.requests.len()));
    assert_eq!(
        hook("pull_request", "d-2", payload.clone(), "wrong-secret").await,
        401
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        (env.event_count(), env.fake.with(|s| s.requests.len())),
        before
    );
    assert_eq!(env.pr_data(5)["title"], "Pull request 5");
    // Missing signature or delivery header, and an unknown connection.
    let url = format!("{}/v1/hub/webhooks/github/{connection}", env.url);
    let bare = env.http.post(&url).body("{}").send().await.unwrap();
    assert_eq!(bare.status(), 401);
    let body = serde_json::to_vec(&payload).unwrap();
    let no_delivery = env
        .http
        .post(&url)
        .header("x-hub-signature-256", fake_github::sign(secret, &body))
        .header("x-github-event", "ping")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(no_delivery.status(), 400);
    let unknown =
        fake_github::deliver(&env.url, "con_unknown", "ping", "d-9", &payload, secret).await;
    assert_eq!(unknown, 404);
    // The same delivery with the right secret works once, then is a duplicate.
    assert_eq!(
        hook("pull_request", "d-2", payload.clone(), secret).await,
        202
    );
    eventually("the changed title", || {
        env.pr_data(5)["title"] == "changed on github"
    })
    .await;
    let count = env.event_count();
    let requests = env.fake.with(|s| s.requests.len());
    let resend = env
        .http
        .post(&url)
        .header("x-hub-signature-256", fake_github::sign(secret, &body))
        .header("x-github-event", "pull_request")
        .header("x-github-delivery", "d-2")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resend.status(), 200);
    assert_eq!(
        serde_json::from_str::<Value>(&resend.text().await.unwrap()).unwrap(),
        json!({"duplicate": true})
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        (env.event_count(), env.fake.with(|s| s.requests.len())),
        (count, requests)
    );

    // The body cap is 1 MiB: more than the other routes allow, but not unlimited.
    let padded = json!({"repository": {"id": REPO_ID}, "pad": "x".repeat(100 * 1024)});
    assert_eq!(hook("ping", "d-big", padded, secret).await, 202);
    let huge = json!({"repository": {"id": REPO_ID}, "pad": "x".repeat(2 * 1024 * 1024)});
    let before = env.event_count();
    assert_eq!(hook("ping", "d-huge", huge, secret).await, 413);
    assert_eq!(env.event_count(), before);
    assert_eq!(
        hook("ping", "d-huge", json!({}), secret).await,
        202,
        "the huge delivery id was not recorded"
    );

    // A push re-reads the branch; a push that deleted it forgets it.
    add_branch(&env, "feat/x", 1);
    let push = json!({"ref": "refs/heads/feat/x", "repository": {"id": REPO_ID}});
    assert_eq!(hook("push", "d-3", push.clone(), secret).await, 202);
    eventually("the branch", || env.observed("branch").contains("feat/x")).await;
    let branch: Value =
        serde_json::from_str(&env.store.observations(&env.repo().id, "branch").unwrap()[0].data)
            .unwrap();
    assert_eq!(branch["sha"].as_str().unwrap().len(), 40);
    env.fake
        .with(|s| s.repos.get_mut(&REPO_ID).unwrap().branches.clear());
    assert_eq!(hook("push", "d-4", push, secret).await, 202);
    eventually("the branch to go", || env.observed("branch").is_empty()).await;
    assert_eq!(env.events("branch_deleted").len(), 1);
    // Tags and unknown repositories are ignored.
    let tag = json!({"ref": "refs/tags/v1", "repository": {"id": REPO_ID}});
    assert_eq!(hook("push", "d-5", tag, secret).await, 202);
    let other = json!({"number": 5, "repository": {"id": 999}});
    assert_eq!(hook("pull_request", "d-6", other, secret).await, 202);

    // A merged pull request is forgotten with its merge commit.
    env.pull(5, |p| {
        p.state = "closed".into();
        p.merged = true;
        p.merge_commit_sha = Some("m".repeat(40));
    });
    assert_eq!(hook("pull_request", "d-7", payload, secret).await, 202);
    eventually("the pull request to go", || {
        env.observed("pull_request").is_empty()
    })
    .await;
    let closed = &env.events("pull_request_closed")[0];
    assert_eq!(
        (
            closed["merged"].clone(),
            closed["merge_commit_sha"].clone(),
            closed["repository_id"].clone()
        ),
        (json!(true), json!("m".repeat(40)), json!(100))
    );
    assert!(env.fake.with(|s| {
        s.requests
            .iter()
            .all(|r| !r.path_and_query.contains("/999"))
    }));
}

// ---- 4. polling when every webhook is lost ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catchup_discovers_ordinary_merges_without_an_open_observation() {
    use agentscm::hub::catchup::{CatchupCoverageState, CatchupQuery};

    let env = Env::start().await;
    env.connect_app().await;
    env.install_widgets();
    add_pull(&env, 7, &["src/new.rs", "src/old.rs"]);
    env.pull(7, |pull| {
        pull.state = "closed".into();
        pull.merged = true;
        pull.merge_commit_sha = Some("a".repeat(40));
    });
    let owner = env
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: env.host(),
                user_id: 77,
            },
            "reader",
        )
        .unwrap();
    let (token, _, _) =
        mint_bound_agent_token(&*env.store, &owner.id, "reader", REPO, REPO_ID, 3600, true)
            .unwrap();
    env.poll().await;
    assert!(env.observed("pull_request").is_empty());
    let page = env
        .store
        .work_catchup(&token, &CatchupQuery::default(), unix_now())
        .unwrap();
    assert_eq!(page.merges.len(), 1);
    let merge = &page.merges[0];
    assert_eq!(merge.pull_number, 7);
    assert_eq!(merge.merge_commit, "a".repeat(40));
    assert_eq!(merge.paths, ["src/new.rs", "src/old.rs"]);
    assert!(merge.paths_complete);
    assert_eq!(merge.occurred_at, None);
    assert!(page.complete);
    assert!(
        page.source_coverage
            .iter()
            .any(|coverage| { coverage.state == CatchupCoverageState::ObservedOnly })
    );
    let writes = env.fake_writes();
    let detail_reads = env.fake.with(|state| {
        state
            .requests
            .iter()
            .filter(|request| {
                request.path_and_query.split('?').next() == Some("/repos/acme/widgets/pulls/7")
            })
            .count()
    });
    env.poll().await;
    let repeated = env
        .store
        .work_catchup(&token, &CatchupQuery::default(), unix_now())
        .unwrap();
    assert_eq!(repeated.merges, page.merges);
    assert_eq!(env.fake_writes(), writes);
    assert_eq!(
        env.fake.with(|state| state
            .requests
            .iter()
            .filter(|request| {
                request.path_and_query.split('?').next() == Some("/repos/acme/widgets/pulls/7")
            })
            .count()),
        detail_reads,
        "known merges do not repeat detail or file requests"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn polling_converges_without_webhooks_and_does_not_rewrite_what_is_unchanged() {
    let env = Env::start().await;
    env.connect_app().await;
    env.install_widgets();
    for n in 1..=3 {
        add_pull(&env, n, &[&format!("src/f{n}.rs")]);
    }
    add_branch(&env, "feat/a", 1);
    add_branch(&env, "feat/b", 2);
    // The poll discovers the installation's repositories itself.
    let reports = env.poll().await;
    assert_eq!(reports.len(), 1);
    assert_eq!(
        (
            reports[0].pull_requests,
            reports[0].branches,
            reports[0].not_modified
        ),
        (3, 2, false)
    );
    assert_eq!(
        env.observed("pull_request"),
        BTreeSet::from(["1".into(), "2".into(), "3".into()])
    );
    assert_eq!(
        env.observed("branch"),
        BTreeSet::from(["feat/a".into(), "feat/b".into()])
    );

    // Nothing changed: the list answers 304 and nothing is written.
    let events = env.event_count();
    let reports = env.poll().await;
    assert!(reports[0].not_modified);
    assert_eq!(env.event_count(), events);
    env.fake.with(|s| {
        let pulls: Vec<_> = s
            .requests
            .iter()
            .filter(|r| r.path_and_query.contains("/pulls?state=open"))
            .collect();
        assert!(pulls.last().unwrap().if_none_match.is_some());
        assert!(pulls.first().unwrap().if_none_match.is_none());
        assert_eq!(
            s.requests
                .iter()
                .filter(|request| { request.path_and_query.contains("/pulls?state=closed") })
                .count(),
            2,
            "catch-up independently checks bounded closed proposals"
        );
    });

    // Changes while no webhook arrives: a new head, a merge, a new pull request, a deleted
    // branch and a new one.
    env.pull(1, |p| {
        p.head_sha = "f".repeat(40);
        p.files = vec!["src/f1.rs".into(), "src/extra.rs".into()];
    });
    env.pull(2, |p| {
        p.state = "closed".into();
        p.merged = true;
        p.merge_commit_sha = Some("c".repeat(40));
    });
    add_pull(&env, 4, &["src/f4.rs"]);
    env.fake.with(|s| {
        s.repos
            .get_mut(&REPO_ID)
            .unwrap()
            .branches
            .retain(|b| b.name != "feat/a")
    });
    add_branch(&env, "feat/c", 1);
    let reports = env.poll().await;
    assert_eq!(reports[0].pull_requests, 3);
    assert_eq!(
        env.observed("pull_request"),
        BTreeSet::from(["1".into(), "3".into(), "4".into()])
    );
    assert_eq!(
        env.observed("branch"),
        BTreeSet::from(["feat/b".into(), "feat/c".into()])
    );
    assert_eq!(
        env.pr_data(1)["files"],
        json!(["src/extra.rs", "src/f1.rs"])
    );
    assert_eq!(env.pr_data(1)["head_sha"], "f".repeat(40));
    let closed = env.events("pull_request_closed");
    assert_eq!(closed.len(), 1);
    assert_eq!(
        (
            closed[0]["number"].clone(),
            closed[0]["merge_commit_sha"].clone()
        ),
        (json!(2), json!("c".repeat(40)))
    );
    // Aged out of the recent window and not named by an intent: dropped, not deleted.
    assert_eq!(env.events("branch_unobserved").len(), 1);
    assert!(env.events("branch_deleted").is_empty());

    // Files are fetched only when the head moved: pull 3 was not touched again.
    let file_reads = env.fake.with(|s| {
        s.requests
            .iter()
            .filter(|r| r.path_and_query.contains("/pulls/3/files"))
            .count()
    });
    assert_eq!(file_reads, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn more_than_300_open_pull_requests_are_cut_short_and_said_so() {
    let env = Env::start().await;
    env.connect_app().await;
    env.install_widgets();
    for n in 1..=350 {
        add_pull(&env, n, &["src/a.rs"]);
    }
    let reports = env.poll().await;
    assert_eq!(reports[0].pull_requests, 300);
    assert_eq!(reports[0].truncated, json!({"pull_requests": 1}));
    // Newest first: pulls 51 to 350 are observed.
    let observed = env.observed("pull_request");
    assert_eq!(observed.len(), 300);
    assert!(observed.contains("350") && observed.contains("51") && !observed.contains("50"));
    // The view says so too, and the cut survives a poll that changes nothing.
    let agent = env.agent(1, "alice", "agent");
    assert_eq!(
        env.work(&agent).await["truncated"],
        json!({"pull_requests": 1})
    );
    let events = env.event_count();
    let again = env.poll().await;
    assert_eq!(again[0].truncated, json!({"pull_requests": 1}));
    assert_eq!(env.event_count(), events);
    assert_eq!(
        env.work(&agent).await["truncated"],
        json!({"pull_requests": 1})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_discovery_invalidates_previously_observed_catchup_coverage() {
    use agentscm::hub::work::WorkStart;
    use agentscm::hub::work_preflight::WorkPreflightStatus;

    let env = Env::start().await;
    env.connect_app().await;
    env.install_widgets();
    env.poll().await;
    let owner = env
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: env.host(),
                user_id: 77,
            },
            "reader",
        )
        .unwrap();
    let (token, _, _) =
        mint_bound_agent_token(&*env.store, &owner.id, "reader", REPO, REPO_ID, 3600, true)
            .unwrap();
    let start: WorkStart = serde_json::from_value(json!({
        "request_key":"coverage-failure", "title":"Inspect the source", "session":"coverage-failure",
        "scope":["src/check.rs"],
        "preflight":{
            "source_revision":"ffffffffffffffffffffffffffffffffffffffff",
            "merge_after":0,"merge_through":0,"source_relation_checked":true,"checked_merges":[]
        }
    })).unwrap();
    assert_eq!(
        env.store
            .work_preflight(&token, &start, unix_now())
            .unwrap()
            .status,
        WorkPreflightStatus::ClearWithinCheckedWindow
    );
    env.fake.with(|state| {
        state.fail.push_back(Failure {
            path_contains: "/app/installations".into(),
            status: 401,
            headers: vec![],
        })
    });
    assert!(env.poll().await.is_empty());
    let after = env
        .store
        .work_preflight(&token, &start, unix_now())
        .unwrap();
    assert_eq!(after.status, WorkPreflightStatus::Unknown);
    assert!(
        serde_json::to_value(&after.reasons)
            .unwrap()
            .as_array()
            .unwrap()
            .contains(&json!("merge_coverage_unknown"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_credentials_and_rate_limits_change_the_connection_not_the_data() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    add_pull(&env, 1, &["src/a.rs"]);
    env.poll().await;
    assert_eq!(env.observed("pull_request").len(), 1);
    let admin = env.admin.clone();
    let status = || async {
        let (_, listed) = env.get("/v1/hub/connections", Some(&admin)).await;
        (
            listed["connections"][0]["status"]
                .as_str()
                .unwrap()
                .to_owned(),
            listed["connections"][0]["status_detail"].clone(),
        )
    };
    assert_eq!(status().await.0, "enabled");

    // 401: the connection is marked, with no token in the detail, and the data stays.
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/app/installations".into(),
            status: 401,
            headers: vec![],
        });
    });
    let events = env.event_count();
    assert!(env.poll().await.is_empty());
    let (state, detail) = status().await;
    assert_eq!(
        (state.as_str(), detail),
        ("auth_failed", json!("401 from app"))
    );
    assert_eq!(env.observed("pull_request").len(), 1);
    assert_eq!(env.event_count(), events + 1);
    // A 403 with no rate-limit headers is the same; the same detail adds no event.
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/app/installations".into(),
            status: 401,
            headers: vec![],
        });
    });
    env.poll().await;
    assert_eq!(env.event_count(), events + 1);
    // A working call puts it back.
    assert_eq!(env.poll().await.len(), 1);
    assert_eq!(status().await, ("enabled".to_owned(), Value::Null));
    assert_eq!(env.events("connection_status").len(), 2);

    // 429 with Retry-After: the next poll moves out and the connection is left alone.
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/app/installations".into(),
            status: 429,
            headers: vec![("retry-after".into(), "600".into())],
        });
    });
    assert!(env.poll().await.is_empty());
    let next = env
        .store
        .connection(&connection)
        .unwrap()
        .unwrap()
        .next_poll_at;
    assert!(
        (unix_now() + 595..=unix_now() + 605).contains(&next),
        "{next}"
    );
    assert_eq!(status().await.0, "enabled");
    let requests = env.fake.with(|s| s.requests.len());
    assert!(env.poll().await.is_empty(), "skipped while backing off");
    assert_eq!(env.fake.with(|s| s.requests.len()), requests);
    // An administrator's refresh does not wait.
    assert_eq!(env.refresh(&connection).await.len(), 1);

    // A 403 that says the quota is spent is a rate limit too, and 5xx only skips the repository.
    // One reset time for the header and the assertion, so a second boundary can't split them.
    let reset = unix_now() + 4000;
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/app/installations".into(),
            status: 403,
            headers: vec![
                ("x-ratelimit-remaining".into(), "0".into()),
                ("x-ratelimit-reset".into(), reset.to_string()),
            ],
        });
    });
    env.refresh(&connection).await;
    assert_eq!(
        env.store
            .connection(&connection)
            .unwrap()
            .unwrap()
            .next_poll_at,
        reset
    );
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/pulls?".into(),
            status: 502,
            headers: vec![],
        });
    });
    assert!(env.refresh(&connection).await.is_empty());
    assert_eq!(status().await.0, "enabled");
    assert_eq!(env.observed("pull_request").len(), 1);
}

// ---- 5. recent and intent-named branches ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_recent_and_intent_named_branches_are_observed() {
    let env = Env::start().await;
    env.connect_app().await;
    env.install_widgets();
    for i in 0..290 {
        add_branch(&env, &format!("old/{i}"), 100);
    }
    for i in 0..8 {
        add_branch(&env, &format!("recent/{i}"), 1 + i % 3);
    }
    add_branch(&env, "old-a", 120);
    add_branch(&env, "old-b", 130);
    let agent = env.agent(1, "alice", "agent");
    env.announce(&agent, 1, &["src/a.rs"], Some("old-a")).await;
    env.announce(&agent, 2, &["src/b.rs"], Some("old-b")).await;
    env.announce(&agent, 3, &["src/c.rs"], Some("not-a-branch"))
        .await;
    let reports = env.poll().await;
    assert_eq!(reports[0].branches, 10);
    let observed = env.observed("branch");
    assert_eq!(observed.len(), 10);
    assert!(
        observed.contains("old-a") && observed.contains("old-b") && observed.contains("recent/0")
    );
    assert!(!observed.contains("old/0"));
    // A named branch that is deleted is forgotten on the next poll.
    env.fake.with(|s| {
        s.repos
            .get_mut(&REPO_ID)
            .unwrap()
            .branches
            .retain(|b| b.name != "old-a")
    });
    env.poll().await;
    assert_eq!(env.observed("branch").len(), 9);
    assert_eq!(env.events("branch_deleted").len(), 1);
    assert!(env.events("branch_unobserved").is_empty());
}

// ---- 6. overlap, notices and the opt-in comment and status ----

async fn notices(env: &Env, agent: &str, after: i64) -> Value {
    let path = format!("/v1/hub/repos/acme%2Fwidgets/notices?after={after}");
    env.get(&path, Some(agent)).await.1
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_pulls_notice_the_intent_and_outbound_is_opt_in() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.refresh(&connection).await;
    let agent = env.agent(1, "alice", "billing-agent");
    let reply = env.announce(&agent, 1, &["src/billing.rs"], None).await;
    assert_eq!(reply["pull_requests"], json!([]));
    add_pull(&env, 5, &["src/billing.rs", "README.md"]);
    add_pull(&env, 7, &["docs/unrelated.md"]);
    env.poll().await;

    // The intent's token is told, once, with exact ids.
    let first = notices(&env, &agent, 0).await;
    let list = first["notices"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["kind"], "pr_notice");
    assert_eq!(list[0]["intent"], format!("int_{:032x}", 1));
    let notice = &list[0]["notice"];
    assert_eq!(notice["about"], "pull_request");
    assert_eq!(
        (
            notice["number"].clone(),
            notice["node_id"].clone(),
            notice["repository_id"].clone()
        ),
        (json!(5), json!("PR_5"), json!(100))
    );
    assert_eq!(notice["their_paths"], json!(["src/billing.rs"]));
    assert_eq!(notice["your_paths"], json!(["src/billing.rs"]));
    assert_eq!(notice["head_sha"].as_str().unwrap().len(), 40);
    assert_eq!(notice["author"], json!({"login": "octocat", "id": 1}));
    // A push that adds no overlapping file adds none.
    env.pull(5, |p| p.head_sha = "e".repeat(40));
    env.poll().await;
    assert_eq!(
        notices(&env, &agent, 0).await["notices"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // The announce reply lists the observed pull request that overlaps.
    let again = env.announce(&agent, 2, &["README.md"], None).await;
    assert_eq!(again["pull_requests"][0]["number"], 5);
    assert_eq!(
        again["pull_requests"][0]["their_paths"],
        json!(["README.md"])
    );
    assert_eq!(
        again["pull_requests"][0]["your_paths"],
        json!(["README.md"])
    );

    // Outbound is off by default: nothing was written to GitHub.
    assert_eq!(env.fake_writes(), 0);

    // Turn it on: one comment and one status for the overlapping pull request only.
    let admin = env.admin.clone();
    let path = format!("/v1/hub/connections/{connection}/outbound");
    assert_eq!(
        env.post(&path, Some(&admin), json!({"enabled": true}))
            .await
            .0,
        204
    );
    assert_eq!(
        env.post(&path, Some(&admin), json!({"enabled": true}))
            .await
            .0,
        204
    );
    assert_eq!(env.events("connection_outbound").len(), 1);
    env.poll().await;
    let comments = env.fake.with(|s| s.repos[&REPO_ID].comments.clone());
    assert_eq!(
        comments.keys().copied().collect::<Vec<_>>(),
        [5],
        "{comments:?}"
    );
    assert_eq!(comments[&5].len(), 1);
    let body = comments[&5][0].body.clone();
    assert!(body.starts_with("<!-- agentscm-hub:100:5 -->"), "{body}");
    assert!(
        body.contains("billing-agent") && body.contains("alice") && body.contains("src/billing.rs")
    );
    let statuses = env.fake.with(|s| s.repos[&REPO_ID].statuses.clone());
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].0, "e".repeat(40));
    assert_eq!(
        statuses[0].1,
        json!({"state": "success", "context": "agentscm/coordination",
            "description": "2 overlapping declared intents"})
    );
    // Both intents (billing and README) overlap pull 5, so the count is 2.
    let writes = env.fake_writes();
    env.poll().await;
    assert_eq!(
        env.fake_writes(),
        writes,
        "nothing changed, nothing written"
    );

    // A second change edits the same comment and adds a status for the new head.
    env.pull(5, |p| p.head_sha = "d".repeat(40));
    env.poll().await;
    let comments = env.fake.with(|s| s.repos[&REPO_ID].comments.clone());
    assert_eq!(comments[&5].len(), 1);
    assert!(
        comments[&5][0].body.contains("ddddddd"),
        "{}",
        comments[&5][0].body
    );
    assert_eq!(env.fake.with(|s| s.repos[&REPO_ID].statuses.len()), 2);
    assert_eq!(env.events("pr_comment_written").len(), 2);
    assert_eq!(env.events("pr_status_written").len(), 2);

    // A reply lost after the comment was created: the retry adopts it, not a second one.
    add_pull(&env, 6, &["src/billing.rs"]);
    env.fake.with(|s| s.lose_next_comment_reply = true);
    env.poll().await;
    assert_eq!(env.fake.with(|s| s.repos[&REPO_ID].comments[&6].len()), 1);
    assert!(env.store.outbound(&env.repo().id, 6).unwrap().is_none());
    env.poll().await;
    assert_eq!(env.fake.with(|s| s.repos[&REPO_ID].comments[&6].len()), 1);
    let recorded = env.store.outbound(&env.repo().id, 6).unwrap().unwrap();
    assert!(recorded.comment_id.is_some() && recorded.status_sha.is_some());
    assert_eq!(
        env.fake.with(|s| s.repos[&REPO_ID]
            .statuses
            .iter()
            .filter(|(sha, _)| *sha == env.pr_data(6)["head_sha"].as_str().unwrap())
            .count()),
        1
    );

    // Releasing every overlapping intent edits the comment to say so.
    for n in [1, 2] {
        let (status, _) = env
            .call(
                Method::DELETE,
                &format!("/v1/hub/repos/acme%2Fwidgets/intents/int_{n:032x}"),
                Some(&agent),
                None,
            )
            .await;
        assert_eq!(status, 200);
    }
    // Comments and statuses follow on the next poll, never from the request itself.
    assert!(
        env.fake
            .with(|s| !s.repos[&REPO_ID].comments[&5][0].body.contains("No live"))
    );
    env.poll().await;
    assert!(env.fake.with(|s| {
        s.repos[&REPO_ID].comments[&5][0]
            .body
            .contains("No live declared intent")
    }));
    assert!(env.fake.with(|s| {
        s.repos[&REPO_ID]
            .statuses
            .iter()
            .any(|(_, st)| st["description"] == "0 overlapping declared intents")
    }));
    // Pull 7 never overlapped anything: no comment, no status.
    assert!(env.fake.with(|s| {
        !s.repos[&REPO_ID].comments.contains_key(&7)
            && s.repos[&REPO_ID]
                .statuses
                .iter()
                .all(|(sha, _)| *sha != env.pr_data(7)["head_sha"].as_str().unwrap())
    }));

    // Switched off again: a change writes nothing.
    assert_eq!(
        env.post(&path, Some(&admin), json!({"enabled": false}))
            .await
            .0,
        204
    );
    let writes = env.fake_writes();
    env.pull(5, |p| p.head_sha = "b".repeat(40));
    env.poll().await;
    assert_eq!(env.fake_writes(), writes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbound_off_makes_no_comment_or_status_calls_at_all() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.refresh(&connection).await;
    let agent = env.agent(1, "alice", "billing-agent");
    env.announce(&agent, 1, &["src/billing.rs"], None).await;
    add_pull(&env, 5, &["src/billing.rs"]);
    env.poll().await;
    env.pull(5, |p| p.head_sha = "c".repeat(40));
    env.poll().await;
    env.announce(&agent, 2, &["src/billing.rs"], None).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let calls = env.fake.with(|s| {
        s.requests
            .iter()
            .filter(|r| {
                r.path_and_query.contains("/comments") || r.path_and_query.contains("/statuses/")
            })
            .count()
    });
    assert_eq!(calls, 0);
    assert_eq!(env.events("pr_notice").len(), 1);
}

// ---- 7. a personal access token ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pat_connection_is_created_once_and_acts_as_its_owner() {
    let env = Env::start_with(true).await;
    let admin = env.admin.clone();
    env.fake.with(|s| {
        s.repos.insert(REPO_ID, Repo::new(REPO_ID, REPO));
        s.repos.insert(101, Repo::new(101, "acme/secret"));
    });
    let (_, listed) = env.get("/v1/hub/connections", Some(&admin)).await;
    let all = listed["connections"].as_array().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["label"], "PAT, acts as robot");
    assert_eq!(
        (all[0]["kind"].clone(), all[0]["outbound"].clone()),
        (json!("pat"), json!(false))
    );
    let connection = all[0]["id"].as_str().unwrap().to_owned();
    // A restart finds it again.
    sync::start(&env.state).await.unwrap();
    assert_eq!(env.store.connections().unwrap().len(), 1);

    // Repositories are added one at a time, only if the PAT can read them.
    let path = format!("/v1/hub/connections/{connection}/repositories");
    let (status, added) = env
        .post(&path, Some(&admin), json!({"full_name": REPO}))
        .await;
    assert_eq!(
        (status, added["repository_id"].clone()),
        (200, json!(REPO_ID)),
        "{added}"
    );
    for refused in ["acme/secret", "acme/missing"] {
        let (status, body) = env
            .post(&path, Some(&admin), json!({"full_name": refused}))
            .await;
        assert_eq!(
            (status, body),
            (403, json!({"error": "forbidden"})),
            "{refused}"
        );
    }
    assert_eq!(
        env.post(&path, Some(&admin), json!({"full_name": "bad name"}))
            .await
            .0,
        400
    );
    assert_eq!(
        env.post(&path, None, json!({"full_name": REPO})).await.0,
        401
    );
    assert_eq!(env.store.repositories(&connection).unwrap().len(), 1);

    // Polling reads with the PAT, and its webhooks use the PAT's own secret.
    add_pull(&env, 3, &["src/a.rs"]);
    let reports = env.poll().await;
    assert_eq!(reports.len(), 1);
    assert_eq!(env.observed("pull_request").len(), 1);
    env.fake.with(|s| {
        let pulls: Vec<_> = s
            .requests
            .iter()
            .filter(|r| r.path_and_query.contains("/pulls"))
            .collect();
        assert!(!pulls.is_empty());
        assert!(
            pulls
                .iter()
                .all(|r| r.auth.as_deref() == Some(&format!("Bearer {PAT}")))
        );
    });
    let payload =
        json!({"number": 3, "repository": {"id": REPO_ID}, "pull_request": {"number": 3}});
    let wrong = fake_github::deliver(
        &env.url,
        &connection,
        "ping",
        "p-1",
        &payload,
        fake_github::WEBHOOK_SECRET,
    )
    .await;
    assert_eq!(wrong, 401);
    let right = fake_github::deliver(
        &env.url,
        &connection,
        "ping",
        "p-1",
        &payload,
        PAT_HOOK_SECRET,
    )
    .await;
    assert_eq!(right, 202);
    // The PAT never appears in a response or an event.
    let events: String = env
        .store
        .events_after(0, 1000)
        .unwrap()
        .iter()
        .map(|e| e.data.clone())
        .collect();
    assert!(!events.contains(PAT) && !env.log.lock().unwrap().join("").contains(PAT));
    // An app-only action on a PAT connection, and the opposite, are refused.
    let app = env.connect_app().await;
    assert_eq!(
        env.post(
            &format!("/v1/hub/connections/{app}/repositories"),
            Some(&admin),
            json!({"full_name": REPO})
        )
        .await
        .0,
        400
    );
}

// ---- 9. the agent's view ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_merges_intents_pulls_and_branches_for_the_tokens_repository() {
    let env = Env::start().await;
    let agent = env.agent(1, "alice", "agent");
    // Before the repository is connected there is only the intent side.
    env.announce(&agent, 1, &["src/a.rs"], Some("feat/x")).await;
    let before = env.work(&agent).await;
    assert_eq!(before["connected"], false);
    assert_eq!(before["intents"].as_array().unwrap().len(), 1);
    assert_eq!(before["pull_requests"], json!([]));

    env.connect_app().await;
    env.install_widgets();
    add_pull(&env, 5, &["src/a.rs"]);
    add_branch(&env, "feat/x", 1);
    env.poll().await;
    let work = env.work(&agent).await;
    assert_eq!(work["connected"], true);
    assert_eq!(work["intents"][0]["branch"], "feat/x");
    assert_eq!(work["pull_requests"][0]["number"], 5);
    assert_eq!(work["pull_requests"][0]["files"], json!(["src/a.rs"]));
    assert_eq!(work["branches"][0]["name"], "feat/x");
    assert_eq!(work["truncated"], json!({}));

    // Another repository is a 404, as for intents; so is no token.
    let (status, body) = env
        .get("/v1/hub/repos/acme%2Fother/work", Some(&agent))
        .await;
    assert_eq!((status, body), (404, json!({"error": "not_found"})));
    assert_eq!(
        env.get("/v1/hub/repos/acme%2Fwidgets/work", None).await.0,
        401
    );
}

// ---- configuration ----

#[test]
fn sync_configuration_is_vetted() {
    let dir = TempDir::new().unwrap();
    let config = |vars: &[(&str, &str)], days: i64| {
        let vars: HashMap<_, _> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        SyncConfig::from_lookup("http://127.0.0.1:7332", dir.path().to_owned(), days, |n| {
            vars.get(n).cloned()
        })
        .map(|c| (c.web, c.api, c.host))
        .map_err(|e| e.to_string())
    };
    assert_eq!(
        config(&[], 14).unwrap(),
        (
            "https://github.com".into(),
            "https://api.github.com".into(),
            "github.com".into()
        )
    );
    assert_eq!(
        config(&[("AGENTSCM_GITHUB_URL", "https://ghe.example.com")], 14)
            .unwrap()
            .1,
        "https://ghe.example.com/api/v3"
    );
    assert!(config(&[("AGENTSCM_GITHUB_URL", "http://ghe.example.com")], 14).is_err());
    assert!(config(&[], 0).is_err() && config(&[], 366).is_err());
    assert!(
        config(
            &[
                ("AGENTSCM_GITHUB_PAT", "x"),
                ("AGENTSCM_GITHUB_PAT_FILE", "/x")
            ],
            14
        )
        .is_err()
    );
    assert!(config(&[("AGENTSCM_GITHUB_PAT_WEBHOOK_SECRET", "x")], 14).is_err());
    let error = config(
        &[
            ("AGENTSCM_GITHUB_PAT", "ghp_secret_value"),
            ("AGENTSCM_GITHUB_URL", "http://bad"),
        ],
        14,
    )
    .unwrap_err();
    assert!(!error.contains("ghp_secret_value"));
}

#[cfg(unix)]
#[test]
fn the_secrets_directory_is_private_or_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let fresh = dir.path().join("a/secrets");
    // Startup leaves a missing directory alone (a read-only root needs no secrets).
    sync::prepare_secrets_dir(&fresh, false).unwrap();
    assert!(!fresh.exists());
    sync::prepare_secrets_dir(&fresh, true).unwrap();
    assert_eq!(
        fresh.metadata().unwrap().permissions().mode() & 0o777,
        0o700
    );
    sync::prepare_secrets_dir(&fresh, false).unwrap();
    std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o750)).unwrap();
    let error = sync::prepare_secrets_dir(&fresh, false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("group or others"), "{error}");
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    assert!(sync::prepare_secrets_dir(&file, false).is_err());
}

// ---- review round: guards, caps and fairness ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branch_names_that_could_leave_the_url_are_refused_or_encoded() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.refresh(&connection).await;
    let agent = env.agent(1, "alice", "agent");
    let announce = |n: u128, branch: Option<&str>, base: Option<&str>| {
        let body = json!({"intent_id": format!("int_{n:032x}"), "task": "t", "branch": branch,
            "base": base, "paths": ["src/a.rs"], "ttl_s": 600});
        env.post("/v1/hub/repos/acme%2Fwidgets/intents", Some(&agent), body)
    };
    for bad in [
        "../../x", "a/../b", "a..b", "/abs", "a//b", "x y", "a:b", "a\\b", "a@{1}", "a.lock",
    ] {
        assert_eq!(announce(1, Some(bad), None).await.0, 400, "branch {bad}");
        assert_eq!(announce(2, None, Some(bad)).await.0, 400, "base {bad}");
    }
    assert_eq!(announce(3, Some("feat/ok"), Some("main")).await.0, 200);
    // Odd but legal names are encoded, so they stay inside the branches path.
    assert_eq!(announce(4, Some("%2e%2e/x"), None).await.0, 200);
    assert_eq!(announce(5, Some("a%2Fb"), None).await.0, 200);
    env.poll().await;
    let paths: Vec<String> = env.fake.with(|s| {
        s.requests
            .iter()
            .filter(|r| r.path_and_query.contains("/branches/"))
            .map(|r| r.path_and_query.clone())
            .collect()
    });
    assert!(paths.len() >= 3, "{paths:?}");
    for path in paths {
        assert!(path.starts_with("/repos/acme/widgets/branches/"), "{path}");
        let lower = path.to_lowercase();
        assert!(
            !lower.contains("..") && !lower.contains("%2f") && !lower.contains("%2e"),
            "{path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_names_with_control_characters_never_reach_notices_or_comments() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.refresh(&connection).await;
    let admin = env.admin.clone();
    let on = format!("/v1/hub/connections/{connection}/outbound");
    assert_eq!(
        env.post(&on, Some(&admin), json!({"enabled": true}))
            .await
            .0,
        204
    );
    let agent = env.agent(1, "alice", "docs-agent");
    env.announce(&agent, 1, &["docs/**"], None).await;
    add_pull(&env, 5, &["docs/ok.md", "docs/a\n@org/team"]);
    env.poll().await;
    let told = notices(&env, &agent, 0).await;
    assert_eq!(
        told["notices"][0]["notice"]["their_paths"],
        json!(["docs/ok.md"])
    );
    assert!(!told.to_string().contains("org/team"));
    let body = env
        .fake
        .with(|s| s.repos[&REPO_ID].comments[&5][0].body.clone());
    assert!(
        body.contains("docs/ok.md") && !body.contains("org/team"),
        "{body}"
    );
    let again = env.announce(&agent, 2, &["docs/**"], None).await;
    assert!(!again.to_string().contains("org/team"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_for_a_bad_id_or_a_backing_off_connection_do_nothing() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.refresh(&connection).await;
    add_pull(&env, 5, &["src/a.rs"]);
    let secret = fake_github::WEBHOOK_SECRET;
    let payload = json!({"repository": {"id": REPO_ID}, "pull_request": {"number": 5}});
    for bad in [
        "not-a-connection",
        "con_ABCDEF",
        &format!("con_{}", "g".repeat(32)),
        "con_",
    ] {
        let status = fake_github::deliver(&env.url, bad, "ping", "x-1", &payload, secret).await;
        assert_eq!(status, 404, "{bad}");
    }
    env.store
        .set_next_poll(&connection, unix_now() + 600)
        .unwrap();
    let requests = env.fake.with(|s| s.requests.len());
    let status = fake_github::deliver(
        &env.url,
        &connection,
        "pull_request",
        "x-2",
        &payload,
        secret,
    )
    .await;
    assert_eq!(status, 202);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(env.observed("pull_request").is_empty());
    assert_eq!(
        env.fake.with(|s| s.requests.len()),
        requests,
        "no GitHub call while backing off"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_follows_the_repository_id_across_a_rename_and_a_name_taken_over() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    add_pull(&env, 5, &["src/a.rs"]);
    env.refresh(&connection).await;
    let owner = env
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: env.host(),
                user_id: 1,
            },
            "alice",
        )
        .unwrap();
    let (_, bound, _) =
        mint_bound_agent_token(&*env.store, &owner.id, "bound", REPO, REPO_ID, 3600, false)
            .unwrap();
    let bound = bound.expose().to_owned();
    // The repository is renamed and another one takes the old name.
    env.fake.with(|s| {
        s.repos.get_mut(&REPO_ID).unwrap().full_name = "acme/widgets-renamed".into();
        let mut taken = Repo::new(200, REPO);
        taken.pulls.insert(9, Pull::new(9, "feat/9", &["src/z.rs"]));
        s.repos.insert(200, taken);
        s.installations[0].repos.push(200);
    });
    env.refresh(&connection).await;
    env.poll().await;
    let numbers = |work: &Value| -> Vec<i64> {
        work["pull_requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["number"].as_i64().unwrap())
            .collect()
    };
    assert_eq!(numbers(&env.work(&bound).await), [5]);
    // A token with no repository id can only go by name, and gets the repository named so now.
    let unbound = env.agent(2, "bob", "unbound");
    assert_eq!(numbers(&env.work(&unbound).await), [9]);
    let reply = env.announce(&bound, 1, &["src/a.rs"], None).await;
    assert_eq!(reply["pull_requests"][0]["number"], 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_our_own_marked_comment_is_adopted_and_a_failed_edit_makes_a_new_one() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.refresh(&connection).await;
    let admin = env.admin.clone();
    let on = format!("/v1/hub/connections/{connection}/outbound");
    assert_eq!(
        env.post(&on, Some(&admin), json!({"enabled": true}))
            .await
            .0,
        204
    );
    let agent = env.agent(1, "alice", "billing-agent");
    env.announce(&agent, 1, &["src/billing.rs"], None).await;
    add_pull(&env, 5, &["src/billing.rs"]);
    // Someone else's comment carrying our marker is not ours.
    env.fake.with(|s| {
        s.repos.get_mut(&REPO_ID).unwrap().comments.insert(
            5,
            vec![Comment {
                id: 900,
                body: "<!-- agentscm-hub:100:5 -->\nnot from the hub".into(),
                author: "mallory".into(),
                ..Default::default()
            }],
        );
    });
    env.poll().await;
    let comments = env.fake.with(|s| s.repos[&REPO_ID].comments[&5].clone());
    assert_eq!(comments.len(), 2);
    assert_eq!(
        (comments[0].id, comments[0].body.as_str()),
        (900, "<!-- agentscm-hub:100:5 -->\nnot from the hub")
    );
    assert_eq!(
        comments[1].author,
        format!("{}[bot]", fake_github::APP_SLUG)
    );
    assert_eq!(
        env.store
            .outbound(&env.repo().id, 5)
            .unwrap()
            .unwrap()
            .comment_id,
        Some(comments[1].id as i64)
    );

    // A reply lost after a create, then an edit that fails: write a new comment, not nothing.
    add_pull(&env, 6, &["src/billing.rs"]);
    env.fake.with(|s| s.lose_next_comment_reply = true);
    env.poll().await;
    env.pull(6, |p| p.head_sha = "9".repeat(40));
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/issues/comments/".into(),
            status: 502,
            headers: vec![],
        });
    });
    env.poll().await;
    let comments = env.fake.with(|s| s.repos[&REPO_ID].comments[&6].clone());
    assert_eq!(comments.len(), 2);
    let recorded = env.store.outbound(&env.repo().id, 6).unwrap().unwrap();
    assert_eq!(recorded.comment_id, Some(comments[1].id as i64));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_etag_waits_for_a_complete_single_page() {
    let env = Env::start().await;
    env.connect_app().await;
    env.install_widgets();
    for n in 1..=150 {
        add_pull(&env, n, &["src/a.rs"]);
    }
    env.poll().await;
    assert_eq!(env.observed("pull_request").len(), 150);
    for n in 1..=120 {
        env.pull(n, |p| p.state = "closed".into());
    }
    // 120 closed: 100 are re-read now, 20 are left, and the 30 open fit one page.
    let reports = env.poll().await;
    assert_eq!(reports[0].truncated, json!({"rereads_deferred": 20}));
    assert_eq!(env.repo().pulls_etag, None);
    let reports = env.poll().await;
    assert!(!reports[0].not_modified);
    assert_eq!(reports[0].truncated, Value::Null);
    assert_eq!(env.observed("pull_request").len(), 30);
    assert!(env.repo().pulls_etag.is_some());
    assert!(env.poll().await[0].not_modified);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branches_that_age_out_are_unobserved_and_the_cap_is_reported() {
    let env = Env::start().await;
    env.connect_app().await;
    env.install_widgets();
    add_branch(&env, "aging", 1);
    env.poll().await;
    assert!(env.observed("branch").contains("aging"));
    env.fake.with(|s| {
        s.repos.get_mut(&REPO_ID).unwrap().branches[0].committed_at = days_ago(100);
    });
    env.poll().await;
    assert!(env.observed("branch").is_empty());
    assert_eq!(env.events("branch_unobserved").len(), 1);
    assert!(env.events("branch_deleted").is_empty());
    for i in 0..210 {
        add_branch(&env, &format!("busy/{i}"), 1);
    }
    let reports = env.poll().await;
    assert_eq!(reports[0].branches, 200);
    assert_eq!(reports[0].truncated, json!({"branches": 200}));
    assert_eq!(
        env.work(&env.agent(1, "alice", "a")).await["truncated"],
        json!({"branches": 200})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_401_on_one_repository_skips_it_and_each_round_starts_elsewhere() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.fake.with(|s| {
        s.repos.insert(101, Repo::new(101, "acme/gadgets"));
        s.installations[0].repos.push(101);
    });
    env.poll().await;
    // A 401 from a repository endpoint is not the connection's credential failing.
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/pulls?".into(),
            status: 401,
            headers: vec![],
        });
    });
    let reports = env.poll().await;
    assert_eq!(reports.len(), 1, "one repository skipped, one read");
    let (_, listed) = env
        .get("/v1/hub/connections", Some(&env.admin.clone()))
        .await;
    assert_eq!(listed["connections"][0]["status"], "enabled");
    assert!(env.events("connection_status").is_empty());
    // The first repository read changes from round to round.
    let first_read = |from: usize| {
        env.fake.with(|s| {
            s.requests[from..]
                .iter()
                .find(|r| r.path_and_query.contains("/pulls?"))
                .unwrap()
                .path_and_query
                .clone()
        })
    };
    let a = env.fake.with(|s| s.requests.len());
    env.refresh(&connection).await;
    let one = first_read(a);
    let a = env.fake.with(|s| s.requests.len());
    env.refresh(&connection).await;
    assert_ne!(one, first_read(a));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_overlapping_file_notices_again_but_a_re_added_one_does_not() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.refresh(&connection).await;
    let agent = env.agent(1, "alice", "agent");
    env.announce(&agent, 1, &["src/**"], None).await;
    add_pull(&env, 5, &["src/a.rs"]);
    let push = |files: &[&str], head: &str| {
        let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
        env.pull(5, |p| {
            p.files = files;
            p.head_sha = head.repeat(40);
        });
    };
    let count = || async {
        notices(&env, &agent, 0).await["notices"]
            .as_array()
            .unwrap()
            .len()
    };
    env.poll().await;
    assert_eq!(count().await, 1);
    push(&["src/a.rs", "src/b.rs"], "1");
    env.poll().await;
    assert_eq!(count().await, 2);
    let second = notices(&env, &agent, 0).await["notices"][1]["notice"]["their_paths"].clone();
    assert_eq!(second, json!(["src/a.rs", "src/b.rs"]));
    push(&["src/a.rs"], "2");
    env.poll().await;
    push(&["src/a.rs", "src/b.rs"], "3");
    env.poll().await;
    assert_eq!(count().await, 2, "removed and added back is not new");
    // Once the pull request is gone the memory goes with it.
    env.pull(5, |p| p.state = "closed".into());
    env.poll().await;
    env.pull(5, |p| p.state = "open".into());
    env.poll().await;
    assert_eq!(count().await, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_hosts_and_repositories_get_no_notice_and_a_vanished_pull_is_forgotten() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.refresh(&connection).await;
    let here = env.agent(1, "alice", "here");
    env.announce(&here, 1, &["src/billing.rs"], None).await;
    // The same repository name on another host, and another repository on this one.
    let mint = |host: &str, repo: &str, id: i64| {
        let owner = env
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: host.into(),
                    user_id: id,
                },
                "x",
            )
            .unwrap();
        mint_agent_token(&*env.store, &owner.id, "t", repo, 3600)
            .unwrap()
            .1
            .expose()
            .to_owned()
    };
    let elsewhere = mint("ghe.example.com", REPO, 2);
    let other = mint(&env.host(), "acme/other", 3);
    let body = |n: u128| {
        json!({"intent_id": format!("int_{n:032x}"), "task": "t",
        "paths": ["src/billing.rs"], "ttl_s": 600})
    };
    assert_eq!(
        env.post(
            "/v1/hub/repos/acme%2Fwidgets/intents",
            Some(&elsewhere),
            body(2)
        )
        .await
        .0,
        200
    );
    assert_eq!(
        env.post("/v1/hub/repos/acme%2Fother/intents", Some(&other), body(3))
            .await
            .0,
        200
    );
    add_pull(&env, 5, &["src/billing.rs"]);
    env.poll().await;
    // (The other host's announce does notice `here` as an intent overlap; that is not a PR notice.)
    let told = notices(&env, &here, 0).await;
    let kinds: Vec<_> = told["notices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["kind"].clone())
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| *k == "pr_notice").count(),
        1,
        "{kinds:?}"
    );
    assert!(
        notices(&env, &elsewhere, 0).await["notices"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (_, none) = env
        .get("/v1/hub/repos/acme%2Fother/notices", Some(&other))
        .await;
    assert!(none["notices"].as_array().unwrap().is_empty());

    // Gone from GitHub altogether (a 404 on re-read), not merely closed.
    env.fake.with(|s| {
        s.repos.get_mut(&REPO_ID).unwrap().pulls.remove(&5);
    });
    env.poll().await;
    assert!(env.observed("pull_request").is_empty());
    assert_eq!(env.events("pull_request_closed")[0]["state"], "gone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pat_that_changed_owner_stops_startup_and_a_shared_repository_stays_with_its_first_connection()
 {
    let env = Env::start_with(true).await;
    let admin = env.admin.clone();
    env.fake.with(|s| {
        s.repos.insert(102, Repo::new(102, "acme/shared"));
        s.pats.get_mut(PAT).unwrap().2.insert(102);
    });
    // The app connection gets there first.
    let app = env.connect_app().await;
    env.fake.with(|s| {
        s.installations.push(Installation {
            id: 7,
            account: "acme".into(),
            repos: vec![102],
        });
    });
    env.refresh(&app).await;
    let pat = env.store.pat_connection(&env.host()).unwrap().unwrap();
    let (status, body) = env
        .post(
            &format!("/v1/hub/connections/{}/repositories", pat.id),
            Some(&admin),
            json!({"full_name": "acme/shared"}),
        )
        .await;
    assert_eq!((status, body), (409, json!({"error": "already_connected"})));

    // A different account behind the same token: the label would be wrong.
    env.fake
        .with(|s| s.pats.get_mut(PAT).unwrap().0 = "someone-else".into());
    let error = sync::start(&env.state).await.unwrap_err().to_string();
    assert!(error.contains("label would be wrong"), "{error}");
    // A blip while checking is not a reason to stay down.
    env.fake.with(|s| {
        s.pats.get_mut(PAT).unwrap().0 = "robot".into();
        s.fail.push_back(Failure {
            path_contains: "/user".into(),
            status: 502,
            headers: vec![],
        });
    });
    sync::start(&env.state).await.unwrap();
}

// ---- CLI ----

fn cli(config: &TempDir, args: &[&str]) -> (bool, String, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("hub")
        .args(args)
        .env("XDG_CONFIG_HOME", config.path())
        // Tests never open a real browser: sign-in uses the code flow, or prints its URL.
        .env("AGENTSCM_NO_BROWSER", "1")
        .env("NO_PROXY", "127.0.0.1")
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cli_sets_up_and_manages_connections() {
    let env = Env::start_with(true).await;
    env.fake.with(|s| {
        s.repos.insert(REPO_ID, Repo::new(REPO_ID, REPO));
        s.repos.insert(101, Repo::new(101, "acme/secret"));
    });
    let config = TempDir::new().unwrap();
    let hub = env.url.clone();
    let (ok, _, stderr) = cli(&config, &["login", "--hub", &hub, "--provider", "github"]);
    assert!(ok, "{stderr}");
    let connection = env.store.connections().unwrap()[0].id.clone();

    let (ok, stdout, stderr) = cli(&config, &["connections", "list"]);
    assert!(ok, "{stderr}");
    assert_eq!(
        stdout.trim(),
        format!("{connection} pat enabled outbound=off repos=0 PAT, acts as robot")
    );
    let (ok, stdout, stderr) = cli(&config, &["connections", "add-repo", &connection, REPO]);
    assert!(ok && stdout.is_empty(), "{stderr}");
    let (ok, _, stderr) = cli(
        &config,
        &["connections", "add-repo", &connection, "acme/secret"],
    );
    assert!(!ok && stderr.contains("refused"), "{stderr}");
    assert!(
        !cli(
            &config,
            &["connections", "add-repo", &connection, "not a repo"]
        )
        .0
    );
    assert!(!cli(&config, &["connections", "refresh", "../x"]).0);

    add_pull(&env, 4, &["src/a.rs"]);
    let (ok, stdout, stderr) = cli(&config, &["connections", "refresh", &connection]);
    assert!(ok, "{stderr}");
    assert!(
        stdout.starts_with("acme/widgets pulls=1 changed=1 branches=0 not_modified=false"),
        "{stdout}"
    );
    let (ok, _, stderr) = cli(&config, &["connections", "outbound", &connection, "on"]);
    assert!(ok, "{stderr}");
    assert!(env.store.connection(&connection).unwrap().unwrap().outbound);
    let (ok, stdout, _) = cli(&config, &["connections", "list"]);
    assert!(ok && stdout.contains("outbound=on repos=1"), "{stdout}");
    assert!(cli(&config, &["connections", "outbound", &connection, "off"]).0);
    assert!(!cli(&config, &["connections", "outbound", &connection, "maybe"]).0);

    let (ok, stdout, stderr) = cli(
        &config,
        &[
            "github",
            "setup",
            "--name",
            "agentscm-test",
            "--org",
            "acme",
        ],
    );
    assert!(ok, "{stderr}");
    assert!(
        stdout
            .trim()
            .starts_with(&format!("{hub}/v1/hub/github/setup/")),
        "{stdout}"
    );
    assert_eq!(stdout.trim().lines().count(), 1);
    // An organization setup posts to the organization's settings.
    let secret = stdout.trim().rsplit('/').next().unwrap().to_owned();
    let (_, page) = env
        .raw(
            Method::GET,
            &format!("/v1/hub/github/setup/{secret}"),
            None,
            None,
        )
        .await;
    assert!(
        page.contains("/organizations/acme/settings/apps/new?state="),
        "{page}"
    );

    // Without a session, or as a non-administrator, nothing works.
    let other = TempDir::new().unwrap();
    let (ok, _, stderr) = cli(&other, &["connections", "list"]);
    assert!(!ok && stderr.contains("not signed in"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merge_tells_overlapping_intents_to_sync_but_not_the_merged_branch() {
    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    env.refresh(&connection).await;
    let other = env.agent(1, "alice", "other");
    let own = env.agent(2, "bob", "own");
    env.announce(&other, 1, &["src/billing.rs"], None).await;
    env.announce(&own, 2, &["src/billing.rs"], Some("feat/5"))
        .await;
    add_pull(&env, 5, &["src/billing.rs"]);
    env.poll().await;
    let merged = |agent: &str| {
        let agent = agent.to_owned();
        let env = &env;
        async move {
            notices(env, &agent, 0).await["notices"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|n| n["notice"]["merged"] == true)
                .map(|n| n["notice"].clone())
                .collect::<Vec<_>>()
        }
    };
    assert!(merged(&other).await.is_empty(), "open is not merged");

    // Closed without merging tells nobody to sync.
    env.pull(5, |p| p.state = "closed".into());
    env.poll().await;
    assert!(merged(&other).await.is_empty());

    env.pull(5, |p| p.state = "open".into());
    env.poll().await;
    env.pull(5, |p| {
        p.state = "closed".into();
        p.merged = true;
        p.merge_commit_sha = Some("c".repeat(40));
    });
    env.poll().await;
    let told = merged(&other).await;
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["number"], 5);
    assert_eq!(told[0]["their_paths"], json!(["src/billing.rs"]));
    assert_eq!(told[0]["merge_commit_sha"], "c".repeat(40));
    assert!(
        merged(&own).await.is_empty(),
        "the merged branch's own intent"
    );
}

// ---- supported skills adoption ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lock_file_on_the_default_branch_becomes_the_adoption_and_wins_over_a_checkout() {
    use agentscm::skill_workflow::supported::{LOCK_PATH, SkillLock, SkillPin, catalog_entry};

    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    let agent = env.agent(1, "dev", "dev");
    let entry = catalog_entry("pr-gate", 1).unwrap();
    let lock = |pins: Vec<SkillPin>| {
        serde_json::to_value(SkillLock {
            version: 1,
            skills: pins,
        })
        .unwrap()
    };
    let adoption = "/v1/hub/repos/acme%2Fwidgets/skills/adoption";

    // Not connected to the forge yet: a checkout's report applies.
    let (status, reply) = env
        .call(Method::PUT, adoption, Some(&agent), Some(lock(vec![])))
        .await;
    assert_eq!(
        (status, reply["source"].clone(), reply["applied"].clone()),
        (200, json!("reported_by_checkout"), json!(true)),
        "{reply}"
    );

    // The lock file appears on the default branch; the next poll reads it.
    let text = lock(vec![SkillPin::first_party(entry, None)]).to_string();
    env.fake.with(|s| {
        s.repos
            .get_mut(&REPO_ID)
            .unwrap()
            .files
            .insert(LOCK_PATH.into(), text);
    });
    env.refresh(&connection).await;
    let (status, library) = env
        .get("/v1/hub/skills?repo=acme%2Fwidgets", Some(&agent))
        .await;
    assert_eq!(status, 200, "{library}");
    assert_eq!(library["adoption"]["source"], "default_branch");
    assert_eq!(library["adoption"]["default_branch"], "main");
    assert_eq!(library["adoption"]["pins"][0]["name"], "pr-gate");
    assert_eq!(library["adoption"]["pins"][0]["status"], "adopted");

    // Once the repository is connected, a checkout report is refused and the forge's lock stays.
    let (status, reply) = env
        .call(Method::PUT, adoption, Some(&agent), Some(lock(vec![])))
        .await;
    assert_eq!(
        (status, reply["error"].clone()),
        (409, json!("forge_authoritative")),
        "{reply}"
    );
    let (_, library) = env
        .get("/v1/hub/skills?repo=acme%2Fwidgets", Some(&agent))
        .await;
    assert_eq!(library["adoption"]["source"], "default_branch");
    assert_eq!(library["adoption"]["pins"][0]["status"], "adopted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connecting_the_forge_drops_a_checkout_adoption_when_the_default_branch_has_no_lock() {
    use agentscm::skill_workflow::supported::{SkillLock, SkillPin, catalog_entry};

    let env = Env::start().await;
    let connection = env.connect_app().await;
    env.install_widgets();
    let agent = env.agent(1, "dev", "dev");
    let pinned = serde_json::to_value(SkillLock {
        version: 1,
        skills: vec![SkillPin::first_party(
            catalog_entry("pr-gate", 1).unwrap(),
            None,
        )],
    })
    .unwrap();

    // A feature branch reports a lock before the repository is connected.
    let (status, reply) = env
        .call(
            Method::PUT,
            "/v1/hub/repos/acme%2Fwidgets/skills/adoption",
            Some(&agent),
            Some(pinned),
        )
        .await;
    assert_eq!(
        (status, reply["applied"].clone()),
        (200, json!(true)),
        "{reply}"
    );

    // The forge is read and its default branch has no lock: only that branch decides.
    env.refresh(&connection).await;
    let (status, library) = env
        .get("/v1/hub/skills?repo=acme%2Fwidgets", Some(&agent))
        .await;
    assert_eq!(status, 200, "{library}");
    assert_eq!(library["adoption"]["source"], "none", "{library}");
    assert_eq!(library["adoption"]["pins"], json!([]), "{library}");
}
