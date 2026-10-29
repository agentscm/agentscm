//! GitLab coordination sync against an in-process fake self-managed GitLab (served under a
//! path prefix): no live accounts. Sign-in and minting rules are in hub_login.rs.
use crate::fake_gitlab;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscm::backend::LocalBackend;
use agentscm::hub::connection::{ConnectedRepository, Observed};
use agentscm::hub::login::{Login, LoginConfig};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::sync::{self, PollReport, Sync, SyncConfig};
use agentscm::hub::{HubState, router};
use axum::extract::Form;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use fake_gitlab::{Branch, Failure, FakeGitlab, Mr, Note, Project, Token};
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 100;
const TOKEN: &str = "synthetic-gitlab-token";
const HOOK: &str = "synthetic-gitlab-hook";
const API: &str = "/gitlab/api/v4";

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
    fake: FakeGitlab,
    url: String,
    state: HubState,
    store: Arc<dyn Store>,
    http: reqwest::Client,
    admin: String,
    host: String,
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
    /// The fake has `acme/widgets` (id 100, private) and a token that reads it.
    async fn start() -> Self {
        let env = Self::start_bare().await;
        env.add_widgets().await;
        env
    }

    async fn start_bare() -> Self {
        let fake = FakeGitlab::start().await;
        fake.with(|s| {
            s.projects
                .insert(REPO_ID, Project::new(REPO_ID, REPO, "private"));
            s.tokens.insert(
                TOKEN.into(),
                Token {
                    username: "robot".into(),
                    user_id: 77,
                    access: [(REPO_ID, 30)].into(),
                },
            );
        });
        let oauth = oauth_shim().await;
        let dir = TempDir::new().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let base = fake.base.clone();
        let sync_config =
            SyncConfig::from_lookup(&url, dir.path().join("secrets"), 14, |name| match name {
                "AGENTSCM_GITLAB_URL" => Some(base.clone()),
                "AGENTSCM_GITLAB_TOKEN" => Some(TOKEN.into()),
                "AGENTSCM_GITLAB_WEBHOOK_SECRET" => Some(HOOK.into()),
                _ => None,
            })
            .unwrap();
        let login = LoginConfig::from_lookup(&url, |name| match name {
            "AGENTSCM_GITHUB_CLIENT_ID" => Some("client".into()),
            "AGENTSCM_GITHUB_CLIENT_SECRET" => Some("client-secret-for-login".into()),
            "AGENTSCM_GITHUB_URL" | "AGENTSCM_GITHUB_API_URL" => Some(oauth.clone()),
            "AGENTSCM_ADMINS" => Some("github:#9".into()),
            _ => None,
        })
        .unwrap();
        let state = HubState::new(
            store.clone(),
            Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
        )
        .with_login(Login::new(login).unwrap())
        .with_sync(Sync::new(sync_config).unwrap());
        sync::start(&state).await.unwrap();
        let app = router(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let host = base
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .to_owned();
        let mut env = Self {
            fake,
            url,
            state,
            store,
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            admin: String::new(),
            host,
            log: Default::default(),
            _dir: dir,
        };
        env.admin = env.sign_in_admin().await;
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

    /// The token connection the hub made at startup.
    fn connection(&self) -> String {
        let all = self.store.connections().unwrap();
        assert_eq!(all.len(), 1);
        all[0].id.clone()
    }

    async fn add_repository(&self, full_name: &str) -> (u16, Value) {
        let admin = self.admin.clone();
        self.post(
            &format!("/v1/hub/connections/{}/repositories", self.connection()),
            Some(&admin),
            json!({"full_name": full_name}),
        )
        .await
    }

    async fn add_widgets(&self) {
        let (status, added) = self.add_repository(REPO).await;
        assert_eq!(status, 200, "{added}");
    }

    async fn set_outbound(&self, enabled: bool) {
        let admin = self.admin.clone();
        let path = format!("/v1/hub/connections/{}/outbound", self.connection());
        assert_eq!(
            self.post(&path, Some(&admin), json!({"enabled": enabled}))
                .await
                .0,
            204
        );
    }

    async fn refresh(&self) -> Vec<Value> {
        let admin = self.admin.clone();
        let (status, reply) = self
            .post(
                &format!("/v1/hub/connections/{}/refresh", self.connection()),
                Some(&admin),
                json!({}),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        reply["reports"].as_array().unwrap().clone()
    }

    fn repo(&self) -> ConnectedRepository {
        self.store
            .repository_named(&self.host, REPO)
            .unwrap()
            .unwrap()
    }

    async fn poll(&self) -> Vec<PollReport> {
        sync::poll_once(&self.state).await.unwrap()
    }

    fn restarted_sync(&self) -> HubState {
        let base = self.fake.base.clone();
        let sync_config = SyncConfig::from_lookup(
            &self.url,
            self._dir.path().join("restart-secrets"),
            14,
            |name| match name {
                "AGENTSCM_GITLAB_URL" => Some(base.clone()),
                "AGENTSCM_GITLAB_TOKEN" => Some(TOKEN.into()),
                "AGENTSCM_GITLAB_WEBHOOK_SECRET" => Some(HOOK.into()),
                _ => None,
            },
        )
        .unwrap();
        HubState::new(
            self.store.clone(),
            Arc::new(
                LocalBackend::open(self._dir.path().join("restart-objects"))
                    .expect("restart object store"),
            ),
        )
        .with_sync(Sync::new(sync_config).unwrap())
    }

    /// An agent token for `REPO` owned by a new GitLab user on the fake's host.
    fn agent(&self, user_id: i64, login: &str, name: &str) -> String {
        let owner = self
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: "gitlab".into(),
                    host: self.host.clone(),
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

    async fn notices(&self, agent: &str) -> Value {
        self.get("/v1/hub/repos/acme%2Fwidgets/notices?after=0", Some(agent))
            .await
            .1
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

    fn pr_data(&self, number: i64) -> Value {
        let rows = self
            .store
            .observations(&self.repo().id, "pull_request")
            .unwrap();
        let row = rows
            .iter()
            .find(|o| o.key == number.to_string())
            .expect("observed merge request");
        serde_json::from_str(&row.data).unwrap()
    }

    /// Notes and statuses written to the fake.
    fn fake_writes(&self) -> usize {
        self.fake.with(|s| {
            s.requests
                .iter()
                .filter(|r| {
                    r.method != "GET"
                        && (r.path_and_query.contains("/notes")
                            || r.path_and_query.contains("/statuses/"))
                })
                .count()
        })
    }

    fn requests(&self) -> usize {
        self.fake.with(|s| s.requests.len())
    }

    fn mr(&self, iid: i64, f: impl FnOnce(&mut Mr)) {
        self.fake.with(|s| {
            f(s.projects
                .get_mut(&REPO_ID)
                .unwrap()
                .mrs
                .get_mut(&iid)
                .unwrap())
        });
    }

    fn notes(&self, iid: i64) -> Vec<Note> {
        self.fake.with(|s| {
            s.projects[&REPO_ID]
                .notes
                .get(&iid)
                .cloned()
                .unwrap_or_default()
        })
    }

    fn statuses(&self) -> Vec<(String, Value)> {
        self.fake.with(|s| s.projects[&REPO_ID].statuses.clone())
    }

    async fn connection_status(&self) -> (String, Value) {
        let admin = self.admin.clone();
        let (_, listed) = self.get("/v1/hub/connections", Some(&admin)).await;
        (
            listed["connections"][0]["status"]
                .as_str()
                .unwrap()
                .to_owned(),
            listed["connections"][0]["status_detail"].clone(),
        )
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn add_mr(env: &Env, iid: i64, files: &[&str]) {
    env.fake.with(|s| {
        s.projects
            .get_mut(&REPO_ID)
            .unwrap()
            .mrs
            .insert(iid, Mr::new(iid, &format!("feat/{iid}"), files));
    });
}

fn add_branch(env: &Env, name: &str, age_days: i64) {
    env.fake.with(|s| {
        s.projects
            .get_mut(&REPO_ID)
            .unwrap()
            .branches
            .push(Branch::new(name, &days_ago(age_days)));
    });
}

fn fail_once(env: &Env, path_contains: &str, status: u16, headers: &[(&str, &str)]) {
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: path_contains.into(),
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        });
    });
}

// ---- 1. the token connection and adding projects ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_connection_is_created_once_and_adds_only_readable_projects() {
    let env = Env::start_bare().await;
    env.fake.with(|s| {
        s.projects
            .insert(101, Project::new(101, "grp/sub/nested", "private"));
        s.projects
            .insert(102, Project::new(102, "acme/secret", "private"));
        s.tokens.get_mut(TOKEN).unwrap().access.insert(101, 20);
    });
    let admin = env.admin.clone();
    let (_, listed) = env.get("/v1/hub/connections", Some(&admin)).await;
    let all = listed["connections"].as_array().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["label"], "GitLab token, acts as robot");
    assert_eq!(
        (
            all[0]["provider"].clone(),
            all[0]["kind"].clone(),
            all[0]["host"].clone(),
            all[0]["outbound"].clone()
        ),
        (json!("gitlab"), json!("pat"), json!(env.host), json!(false))
    );
    // A restart finds it again.
    sync::start(&env.state).await.unwrap();
    assert_eq!(env.store.connections().unwrap().len(), 1);

    let (status, added) = env.add_repository(REPO).await;
    assert_eq!(
        (status, added["repository_id"].clone()),
        (200, json!(REPO_ID)),
        "{added}"
    );
    // Nested groups are fine, and the project id is what identifies it.
    let (status, added) = env.add_repository("grp/sub/nested").await;
    assert_eq!(
        (
            status,
            added["repository_id"].clone(),
            added["full_name"].clone()
        ),
        (200, json!(101), json!("grp/sub/nested")),
        "{added}"
    );
    let stored = env
        .store
        .repository_named(&env.host, REPO)
        .unwrap()
        .unwrap();
    assert_eq!(stored.node_id.as_deref(), Some("gid://gitlab/Project/100"));
    for refused in ["acme/secret", "acme/missing"] {
        assert_eq!(
            env.add_repository(refused).await,
            (403, json!({"error": "forbidden"})),
            "{refused}"
        );
    }
    assert_eq!(env.add_repository("bad name").await.0, 400);
    let connection = env.connection();
    let path = format!("/v1/hub/connections/{connection}/repositories");
    assert_eq!(
        env.post(&path, None, json!({"full_name": REPO})).await.0,
        401
    );
    assert_eq!(env.store.repositories(&connection).unwrap().len(), 2);

    // Polling reads with the token and by project id; a GitHub webhook route is not its route.
    add_mr(&env, 3, &["src/a.rs"]);
    env.fake.with(|s| s.requests.clear());
    assert_eq!(env.poll().await.len(), 2);
    env.fake.with(|s| {
        assert!(!s.requests.is_empty());
        assert!(
            s.requests
                .iter()
                .all(|r| r.auth.as_deref() == Some(&format!("Bearer {TOKEN}")))
        );
        assert!(s.requests.iter().any(|r| {
            r.path_and_query
                .starts_with(&format!("{API}/projects/100/merge_requests?"))
        }));
    });
    let hook = format!("{}/v1/hub/webhooks/github/{connection}", env.url);
    assert_eq!(
        env.http
            .post(&hook)
            .body("{}")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    // The token never appears in a response or an event.
    let events: String = env
        .store
        .events_after(0, 1000)
        .unwrap()
        .iter()
        .map(|e| e.data.clone())
        .collect();
    assert!(!events.contains(TOKEN) && !env.log.lock().unwrap().join("").contains(TOKEN));
}

// ---- 2. webhooks ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_are_verified_deduplicated_and_re_read() {
    let env = Env::start().await;
    let connection = env.connection();
    add_mr(&env, 5, &["src/a.rs"]);
    let hook = |event: &'static str, uuid: &'static str, body: Value, token: &'static str| {
        let (url, connection) = (env.url.clone(), connection.clone());
        async move { fake_gitlab::deliver(&url, &connection, event, uuid, &body, token).await }
    };
    let payload = json!({"object_kind": "merge_request", "project": {"id": REPO_ID},
        "object_attributes": {"iid": 5, "title": "never trusted"}});

    assert_eq!(
        hook("Merge Request Hook", "u-1", payload.clone(), HOOK).await,
        202
    );
    eventually("the merge request", || {
        env.observed("pull_request").len() == 1
    })
    .await;
    let observed = env.pr_data(5);
    // State comes from GitLab, not from the payload.
    assert_eq!(observed["title"], "Merge request 5");
    assert_eq!(observed["files"], json!(["src/a.rs"]));
    assert_eq!(observed["node_id"], "gid://gitlab/MergeRequest/9005");

    // A wrong token changes nothing and does not use up the delivery id.
    env.mr(5, |m| m.title = "changed on gitlab".into());
    let before = (env.event_count(), env.requests());
    assert_eq!(
        hook("Merge Request Hook", "u-2", payload.clone(), "wrong").await,
        401
    );
    assert_eq!(
        hook("Merge Request Hook", "u-2", payload.clone(), "").await,
        401
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!((env.event_count(), env.requests()), before);
    assert_eq!(env.pr_data(5)["title"], "Merge request 5");
    // No delivery uuid, an unknown connection, and a connection id of the wrong shape.
    let url = format!("{}/v1/hub/webhooks/gitlab/{connection}", env.url);
    let no_uuid = env
        .http
        .post(&url)
        .header("x-gitlab-token", HOOK)
        .header("x-gitlab-event", "Merge Request Hook")
        .body(payload.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(no_uuid.status(), 400);
    assert_eq!(
        fake_gitlab::deliver(&env.url, "con_unknown", "Push Hook", "u-9", &payload, HOOK).await,
        404
    );
    assert_eq!(
        fake_gitlab::deliver(&env.url, "x", "Push Hook", "u-9", &payload, HOOK).await,
        404
    );
    // Right token: works once, then is a duplicate and does nothing.
    assert_eq!(
        hook("Merge Request Hook", "u-2", payload.clone(), HOOK).await,
        202
    );
    eventually("the changed title", || {
        env.pr_data(5)["title"] == "changed on gitlab"
    })
    .await;
    let (count, requests) = (env.event_count(), env.requests());
    let resend = env
        .http
        .post(&url)
        .header("x-gitlab-token", HOOK)
        .header("x-gitlab-event", "Merge Request Hook")
        .header("x-gitlab-event-uuid", "u-2")
        .body(payload.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resend.status(), 200);
    assert_eq!(
        serde_json::from_str::<Value>(&resend.text().await.unwrap()).unwrap(),
        json!({"duplicate": true})
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!((env.event_count(), env.requests()), (count, requests));

    // The body cap is 1 MiB.
    let padded = json!({"project": {"id": REPO_ID}, "pad": "x".repeat(100 * 1024)});
    assert_eq!(hook("System Hook", "u-big", padded, HOOK).await, 202);
    let huge = json!({"project": {"id": REPO_ID}, "pad": "x".repeat(2 * 1024 * 1024)});
    let before = env.event_count();
    assert_eq!(hook("System Hook", "u-huge", huge, HOOK).await, 413);
    assert_eq!(env.event_count(), before);

    // A push hook observes the branch, and a push that deleted it forgets it.
    add_branch(&env, "feat/x", 1);
    let push = json!({"project_id": REPO_ID, "ref": "refs/heads/feat/x"});
    assert_eq!(hook("Push Hook", "u-3", push.clone(), HOOK).await, 202);
    eventually("the branch", || env.observed("branch").len() == 1).await;
    env.fake
        .with(|s| s.projects.get_mut(&REPO_ID).unwrap().branches.clear());
    assert_eq!(hook("Push Hook", "u-4", push, HOOK).await, 202);
    eventually("the deletion", || env.observed("branch").is_empty()).await;
    assert_eq!(env.events("branch_deleted").len(), 1);
    // Other event types are acknowledged and ignored, before any delivery record or job.
    let before = (env.event_count(), env.requests());
    let ignored = env
        .http
        .post(&url)
        .header("x-gitlab-token", HOOK)
        .header("x-gitlab-event", "Pipeline Hook")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(ignored.status(), 202);
    assert_eq!(
        serde_json::from_str::<Value>(&ignored.text().await.unwrap()).unwrap(),
        json!({"ignored": true})
    );
    // A missing token header is refused like a wrong one.
    let bare = env
        .http
        .post(&url)
        .header("x-gitlab-event", "Push Hook")
        .header("x-gitlab-event-uuid", "u-bare")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), 401);
    assert_eq!((env.event_count(), env.requests()), before);
    // A tag push, or a ref that is not a branch name, is ignored.
    let before = env.requests();
    for (n, body) in [
        json!({"project_id": REPO_ID, "ref": "refs/tags/v1"}),
        json!({"project_id": REPO_ID, "ref": "refs/heads/../x"}),
    ]
    .into_iter()
    .enumerate()
    {
        let uuid = ["u-t0", "u-t1"][n];
        assert_eq!(hook("Push Hook", uuid, body, HOOK).await, 202);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(env.requests(), before);
}

// ---- 3. polling when webhooks are lost ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn polling_converges_without_webhooks_and_does_not_rewrite_what_is_unchanged() {
    let env = Env::start().await;
    for n in 1..=3 {
        add_mr(&env, n, &[&format!("src/f{n}.rs")]);
    }
    add_branch(&env, "feat/a", 1);
    add_branch(&env, "feat/b", 2);
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

    // GitLab has no conditional list: the list is read again, and nothing is written.
    let events = env.event_count();
    let reports = env.poll().await;
    assert!(!reports[0].not_modified);
    assert_eq!(env.event_count(), events);
    let lists = env.fake.with(|s| {
        s.requests
            .iter()
            .filter(|r| r.path_and_query.contains("/merge_requests?state=opened"))
            .count()
    });
    assert_eq!(lists, 2);
    assert_eq!(
        env.fake.with(|state| state
            .requests
            .iter()
            .filter(|request| {
                request
                    .path_and_query
                    .contains("/merge_requests?state=merged")
            })
            .count()),
        2,
        "catch-up independently checks bounded merged proposals"
    );

    // Changes while no webhook arrives: a new head, a merge, a new MR, a deleted and a new branch.
    env.mr(1, |m| {
        m.sha = "f".repeat(40);
        m.files = vec!["src/f1.rs".into(), "src/extra.rs".into()];
    });
    env.mr(2, |m| {
        m.state = "merged".into();
        m.merge_commit_sha = Some("c".repeat(40));
    });
    add_mr(&env, 4, &["src/f4.rs"]);
    env.fake.with(|s| {
        s.projects
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
            closed[0]["merged"].clone(),
            closed[0]["merge_commit_sha"].clone()
        ),
        (json!(2), json!(true), json!("c".repeat(40)))
    );
    assert_eq!(env.events("branch_unobserved").len(), 1);
    // Files are read only when the head moved: MR 3 was not touched again.
    let file_reads = env.fake.with(|s| {
        s.requests
            .iter()
            .filter(|r| r.path_and_query.contains("/merge_requests/3/diffs"))
            .count()
    });
    assert_eq!(file_reads, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn polling_advances_persisted_future_observations_after_restart() {
    let env = Env::start().await;
    add_mr(&env, 1, &["src/one.rs"]);
    add_mr(&env, 2, &["src/two.rs"]);
    add_branch(&env, "feat/a", 1);
    add_branch(&env, "feat/b", 1);
    env.poll().await;

    let repository = env.repo();
    let future = (unix_now() + 60) * 1_000;
    let mr1 = env.pr_data(1).to_string();
    assert_eq!(
        env.store
            .observe(&repository.id, "pull_request", "1", &mr1, future)
            .unwrap(),
        Observed::Unchanged
    );
    assert!(
        env.store
            .forget(
                &repository.id,
                "pull_request",
                "2",
                future + 1,
                &json!({"number": 2, "state": "closed"}).to_string(),
                "pull_request_closed",
            )
            .unwrap()
    );
    let branches = env.store.observations(&repository.id, "branch").unwrap();
    let branch_a = &branches
        .iter()
        .find(|row| row.key == "feat/a")
        .unwrap()
        .data;
    assert_eq!(
        env.store
            .observe(&repository.id, "branch", "feat/a", branch_a, future + 2)
            .unwrap(),
        Observed::Unchanged
    );
    assert!(
        env.store
            .forget(
                &repository.id,
                "branch",
                "feat/b",
                future + 3,
                &json!({"name": "feat/b", "deleted": true}).to_string(),
                "branch_deleted",
            )
            .unwrap()
    );

    // The new Sync seeds its logical clock from active rows and tombstones. It can therefore
    // close future-stamped active state and recreate future-stamped tombstones after wall-clock
    // rollback, while subsequent reads keep advancing in this process.
    env.mr(1, |mr| {
        mr.state = "merged".into();
        mr.merge_commit_sha = Some("c".repeat(40));
    });
    env.fake.with(|state| {
        state
            .projects
            .get_mut(&REPO_ID)
            .unwrap()
            .branches
            .retain(|branch| branch.name != "feat/a")
    });
    let restarted = env.restarted_sync();
    let reports = sync::refresh(&restarted, &env.connection()).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(env.observed("pull_request"), BTreeSet::from(["2".into()]));
    assert_eq!(env.observed("branch"), BTreeSet::from(["feat/b".into()]));
    for row in env
        .store
        .observations(&repository.id, "pull_request")
        .unwrap()
        .into_iter()
        .chain(env.store.observations(&repository.id, "branch").unwrap())
    {
        assert!(row.observed_at > future + 3, "{row:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn more_than_300_open_merge_requests_are_cut_short_and_said_so() {
    let env = Env::start().await;
    for n in 1..=350 {
        add_mr(&env, n, &["src/a.rs"]);
    }
    let reports = env.poll().await;
    assert_eq!(reports[0].pull_requests, 300);
    assert_eq!(reports[0].truncated, json!({"pull_requests": 1}));
    // Newest first: MRs 51 to 350 are observed.
    let observed = env.observed("pull_request");
    assert_eq!(observed.len(), 300);
    assert!(observed.contains("350") && observed.contains("51") && !observed.contains("50"));
    let agent = env.agent(1, "alice", "agent");
    let work = env
        .get("/v1/hub/repos/acme%2Fwidgets/work", Some(&agent))
        .await
        .1;
    assert_eq!(work["truncated"], json!({"pull_requests": 1}));
    // The cut survives a poll that changes nothing.
    let events = env.event_count();
    assert_eq!(env.poll().await[0].truncated, json!({"pull_requests": 1}));
    assert_eq!(env.event_count(), events);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_credentials_and_rate_limits_change_the_connection_not_the_data() {
    let env = Env::start().await;
    add_mr(&env, 1, &["src/a.rs"]);
    env.poll().await;
    assert_eq!(env.observed("pull_request").len(), 1);
    assert_eq!(env.connection_status().await.0, "enabled");

    // 401: the connection is marked, with no token in the detail, and the data stays.
    fail_once(&env, "/merge_requests?", 401, &[]);
    let events = env.event_count();
    assert!(env.poll().await.is_empty());
    assert_eq!(
        env.connection_status().await,
        ("auth_failed".to_owned(), json!("401 from projects"))
    );
    assert_eq!(env.observed("pull_request").len(), 1);
    assert_eq!(env.event_count(), events + 1);
    // The failed credential waits out a backoff; a refresh does not wait.
    let requests = env.requests();
    assert!(env.poll().await.is_empty(), "skipped while backing off");
    assert_eq!(env.requests(), requests);
    // The same failure again adds no event; a working call puts it back.
    fail_once(&env, "/merge_requests?", 401, &[]);
    env.refresh().await;
    assert_eq!(env.event_count(), events + 1);
    assert_eq!(env.refresh().await.len(), 1);
    assert_eq!(
        env.connection_status().await,
        ("enabled".to_owned(), Value::Null)
    );
    assert_eq!(env.events("connection_status").len(), 2);
    // A 403 is one project saying no, not the credential failing.
    fail_once(&env, "/merge_requests?", 403, &[]);
    assert!(env.poll().await.is_empty());
    assert_eq!(env.connection_status().await.0, "enabled");
    assert_eq!(env.poll().await.len(), 1);

    // 429 with Retry-After: the next poll moves out and the connection is left alone.
    fail_once(&env, "/merge_requests?", 429, &[("retry-after", "600")]);
    assert!(env.poll().await.is_empty());
    let next = env
        .store
        .connection(&env.connection())
        .unwrap()
        .unwrap()
        .next_poll_at;
    assert!(
        (unix_now() + 595..=unix_now() + 605).contains(&next),
        "{next}"
    );
    assert_eq!(env.connection_status().await.0, "enabled");
    let requests = env.requests();
    assert!(env.poll().await.is_empty(), "skipped while backing off");
    assert_eq!(env.requests(), requests);
    // An administrator's refresh does not wait; RateLimit-Reset is honoured too.
    assert_eq!(env.refresh().await.len(), 1);
    let reset = (unix_now() + 4000).to_string();
    fail_once(
        &env,
        "/merge_requests?",
        429,
        &[("ratelimit-reset", &reset)],
    );
    env.refresh().await;
    let next = env
        .store
        .connection(&env.connection())
        .unwrap()
        .unwrap()
        .next_poll_at;
    assert!(
        (unix_now() + 3995..=unix_now() + 4005).contains(&next),
        "{next}"
    );
    // A 5xx only skips the repository.
    fail_once(&env, "/merge_requests?", 502, &[]);
    assert!(env.refresh().await.is_empty());
    assert_eq!(env.connection_status().await.0, "enabled");
    assert_eq!(env.observed("pull_request").len(), 1);
}

// ---- 4. recent and intent-named branches ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_recent_and_intent_named_branches_are_observed() {
    let env = Env::start().await;
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
    // The walk stopped at the first old branch instead of reading all 300.
    let pages = env.fake.with(|s| {
        s.requests
            .iter()
            .filter(|r| r.path_and_query.contains("/repository/branches?"))
            .count()
    });
    assert_eq!(pages, 1);
    // A named branch that is deleted is forgotten on the next poll.
    env.fake.with(|s| {
        s.projects
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

// ---- 5. overlap, notices and the opt-in note and status ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_merge_requests_notice_the_intent_and_outbound_is_opt_in() {
    let env = Env::start().await;
    let agent = env.agent(1, "alice", "billing-agent");
    env.announce(&agent, 1, &["src/billing.rs"], None).await;
    add_mr(&env, 5, &["src/billing.rs", "README.md"]);
    add_mr(&env, 7, &["docs/unrelated.md"]);
    env.poll().await;

    let list = env.notices(&agent).await["notices"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(list.len(), 1);
    let notice = &list[0]["notice"];
    assert_eq!(notice["about"], "pull_request");
    assert_eq!(
        (
            notice["number"].clone(),
            notice["node_id"].clone(),
            notice["repository_id"].clone()
        ),
        (
            json!(5),
            json!("gid://gitlab/MergeRequest/9005"),
            json!(100)
        )
    );
    assert_eq!(notice["their_paths"], json!(["src/billing.rs"]));
    assert_eq!(notice["author"], json!({"login": "tanuki", "id": 1}));
    assert_eq!(env.fake_writes(), 0, "outbound is off by default");

    // Turn it on: one note and one status for the overlapping MR only.
    env.set_outbound(true).await;
    env.poll().await;
    let notes = env.notes(5);
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].body.starts_with("<!-- agentscm-hub:100:5 -->"),
        "{}",
        notes[0].body
    );
    assert!(notes[0].body.contains("billing-agent") && notes[0].body.contains("src/billing.rs"));
    assert_eq!(notes[0].author_username, "robot");
    let statuses = env.statuses();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].0, env.pr_data(5)["head_sha"].as_str().unwrap());
    assert_eq!(
        statuses[0].1,
        json!({"state": "success", "name": "agentscm/coordination",
            "description": "1 overlapping declared intents"})
    );
    let writes = env.fake_writes();
    env.poll().await;
    assert_eq!(
        env.fake_writes(),
        writes,
        "nothing changed, nothing written"
    );

    // A change edits the same note and adds a status for the new head.
    env.mr(5, |m| m.sha = "d".repeat(40));
    env.poll().await;
    let notes = env.notes(5);
    assert_eq!(notes.len(), 1);
    assert!(notes[0].body.contains("ddddddd"), "{}", notes[0].body);
    assert_eq!(env.statuses().len(), 2);
    assert_eq!(env.events("pr_comment_written").len(), 2);

    // A reply lost after the note was created: the retry adopts it, not a second one.
    add_mr(&env, 6, &["src/billing.rs"]);
    env.fake.with(|s| s.lose_next_note_reply = true);
    env.poll().await;
    assert_eq!(env.notes(6).len(), 1);
    assert!(env.store.outbound(&env.repo().id, 6).unwrap().is_none());
    env.poll().await;
    assert_eq!(env.notes(6).len(), 1);
    let recorded = env.store.outbound(&env.repo().id, 6).unwrap().unwrap();
    assert_eq!(recorded.comment_id, Some(env.notes(6)[0].id));

    // Releasing the intent edits the note to say so.
    let (status, _) = env
        .call(
            Method::DELETE,
            &format!("/v1/hub/repos/acme%2Fwidgets/intents/int_{:032x}", 1),
            Some(&agent),
            None,
        )
        .await;
    assert_eq!(status, 200);
    env.poll().await;
    assert!(env.notes(5)[0].body.contains("No live declared intent"));
    // MR 7 never overlapped anything: no note, no status.
    assert!(env.notes(7).is_empty());
    let sha7 = env.pr_data(7)["head_sha"].as_str().unwrap().to_owned();
    assert!(env.statuses().iter().all(|(sha, _)| *sha != sha7));

    // Switched off again: a change writes nothing.
    env.set_outbound(false).await;
    let writes = env.fake_writes();
    env.mr(5, |m| m.sha = "b".repeat(40));
    env.poll().await;
    assert_eq!(env.fake_writes(), writes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_our_own_marked_note_is_adopted() {
    let env = Env::start().await;
    env.set_outbound(true).await;
    let agent = env.agent(1, "alice", "billing-agent");
    env.announce(&agent, 1, &["src/billing.rs"], None).await;
    add_mr(&env, 5, &["src/billing.rs"]);
    // Someone else's note carrying our marker is not ours.
    env.fake.with(|s| {
        s.projects.get_mut(&REPO_ID).unwrap().notes.insert(
            5,
            vec![Note {
                id: 900,
                body: "<!-- agentscm-hub:100:5 -->\nnot from the hub".into(),
                author_username: "mallory".into(),
            }],
        );
    });
    env.poll().await;
    let notes = env.notes(5);
    assert_eq!(notes.len(), 2);
    assert_eq!(
        (notes[0].id, notes[0].body.as_str()),
        (900, "<!-- agentscm-hub:100:5 -->\nnot from the hub")
    );
    assert_eq!(notes[1].author_username, "robot");
    assert_eq!(
        env.store
            .outbound(&env.repo().id, 5)
            .unwrap()
            .unwrap()
            .comment_id,
        Some(notes[1].id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbound_off_makes_no_note_or_status_calls_at_all() {
    let env = Env::start().await;
    let agent = env.agent(1, "alice", "billing-agent");
    env.announce(&agent, 1, &["src/billing.rs"], None).await;
    add_mr(&env, 5, &["src/billing.rs"]);
    env.poll().await;
    env.mr(5, |m| m.sha = "c".repeat(40));
    env.poll().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let calls = env.fake.with(|s| {
        s.requests
            .iter()
            .filter(|r| {
                r.path_and_query.contains("/notes") || r.path_and_query.contains("/statuses/")
            })
            .count()
    });
    assert_eq!(calls, 0);
    assert_eq!(env.events("pr_notice").len(), 1);
}

// ---- 6. branch names that could leave the URL ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branch_names_that_could_leave_the_url_are_refused_or_encoded() {
    let env = Env::start().await;
    let agent = env.agent(1, "alice", "agent");
    let announce = |n: u128, branch: Option<&str>| {
        let body = json!({"intent_id": format!("int_{n:032x}"), "task": "t", "branch": branch,
            "paths": ["src/a.rs"], "ttl_s": 600});
        env.post("/v1/hub/repos/acme%2Fwidgets/intents", Some(&agent), body)
    };
    for bad in [
        "../../x", "a/../b", "a..b", "/abs", "a//b", "x y", "a:b", "a\\b", "a@{1}", "a.lock",
    ] {
        assert_eq!(announce(1, Some(bad)).await.0, 400, "branch {bad}");
    }
    assert_eq!(announce(3, Some("feat/ok")).await.0, 200);
    // Odd but legal names are encoded, so they stay inside the branches path.
    assert_eq!(announce(4, Some("%2e%2e/x")).await.0, 200);
    assert_eq!(announce(5, Some("a%2Fb")).await.0, 200);
    env.poll().await;
    let paths: Vec<String> = env.fake.with(|s| {
        s.requests
            .iter()
            .filter(|r| r.path_and_query.contains("/repository/branches/"))
            .map(|r| r.path_and_query.clone())
            .collect()
    });
    assert!(paths.len() >= 3, "{paths:?}");
    for path in paths {
        let name = path
            .strip_prefix(&format!("{API}/projects/100/repository/branches/"))
            .unwrap_or_else(|| panic!("{path}"));
        assert!(
            !name.contains('/') && !name.contains("..") && !name.to_lowercase().contains("%2e"),
            "{path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hooks_for_closed_merge_requests_unconnected_projects_and_github_connections() {
    let env = Env::start().await;
    let connection = env.connection();
    add_mr(&env, 5, &["src/a.rs"]);
    env.poll().await;
    assert_eq!(env.observed("pull_request").len(), 1);
    // A merged MR is forgotten, with its merge commit.
    env.mr(5, |m| {
        m.state = "merged".into();
        m.merge_commit_sha = Some("c".repeat(40));
    });
    let payload =
        |project: i64| json!({"project": {"id": project}, "object_attributes": {"iid": 5}});
    let status = fake_gitlab::deliver(
        &env.url,
        &connection,
        "Merge Request Hook",
        "m-1",
        &payload(REPO_ID),
        HOOK,
    )
    .await;
    assert_eq!(status, 202);
    eventually("the merge request to be forgotten", || {
        env.observed("pull_request").is_empty()
    })
    .await;
    assert_eq!(env.events("pull_request_closed").len(), 1);
    // A project that is not connected does nothing at all.
    let before = (env.event_count(), env.requests());
    let status = fake_gitlab::deliver(
        &env.url,
        &connection,
        "Merge Request Hook",
        "m-2",
        &payload(999),
        HOOK,
    )
    .await;
    assert_eq!(status, 202);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!((env.event_count(), env.requests()), before);
    // A GitLab hook sent to a GitHub connection's id is not found.
    let github = env
        .store
        .insert_connection(
            &agentscm::hub::connection::NewConnection {
                id: agentscm::hub::store::new_connection_id().unwrap(),
                provider: "github".into(),
                host: "github.example.test".into(),
                kind: "app".into(),
                app_id: Some(1),
                app_slug: Some("fixture".into()),
                acts_as: None,
            },
            "fixture",
        )
        .unwrap()
        .id;
    let status = fake_gitlab::deliver(
        &env.url,
        &github,
        "Merge Request Hook",
        "m-3",
        &payload(REPO_ID),
        HOOK,
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_403_skips_that_project_or_write_and_leaves_the_connection_enabled() {
    let env = Env::start().await;
    env.fake.with(|s| {
        s.projects
            .insert(101, Project::new(101, "acme/gadgets", "private"));
        s.tokens.get_mut(TOKEN).unwrap().access.insert(101, 30);
    });
    assert_eq!(env.add_repository("acme/gadgets").await.0, 200);
    add_mr(&env, 1, &["src/a.rs"]);
    // Project 100 answers 403; 101 is still read, whichever comes first.
    fail_once(&env, "/projects/100/", 403, &[]);
    let reports = env.poll().await;
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].repository, "acme/gadgets");
    assert_eq!(env.connection_status().await.0, "enabled");
    assert!(env.observed("pull_request").is_empty());
    assert_eq!(env.poll().await.len(), 2);
    assert_eq!(env.observed("pull_request").len(), 1);

    // A 403 on a note write is logged and skipped.
    env.set_outbound(true).await;
    let agent = env.agent(1, "alice", "billing-agent");
    env.announce(&agent, 1, &["src/a.rs"], None).await;
    env.mr(1, |m| m.sha = "e".repeat(40));
    fail_once(&env, "/notes", 403, &[]);
    env.poll().await;
    assert_eq!(env.connection_status().await.0, "enabled");
    assert!(env.notes(1).is_empty());
    env.poll().await;
    assert_eq!(env.notes(1).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_request_files_renames_truncation_and_late_diffs() {
    let env = Env::start().await;
    add_mr(&env, 1, &["src/a.rs"]);
    env.mr(1, |m| {
        m.renamed = vec![("old/name.rs".into(), "new/name.rs".into())]
    });
    let many: Vec<String> = (0..350).map(|i| format!("src/f{i:03}.rs")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    add_mr(&env, 2, &many);
    // Diffs GitLab has not produced yet: empty for now.
    add_mr(&env, 3, &[]);
    env.poll().await;
    assert_eq!(
        env.pr_data(1)["files"],
        json!(["new/name.rs", "old/name.rs", "src/a.rs"])
    );
    let big = env.pr_data(2);
    assert_eq!(
        (
            big["files"].as_array().unwrap().len(),
            big["files_truncated"].clone()
        ),
        (300, json!(true))
    );
    assert_eq!(env.pr_data(3)["files"], json!([]));
    // The head did not move, but empty files are read again.
    env.mr(3, |m| m.files = vec!["src/late.rs".into()]);
    env.poll().await;
    assert_eq!(env.pr_data(3)["files"], json!(["src/late.rs"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branch_times_in_other_offsets_and_the_branch_cap() {
    let env = Env::start().await;
    let at = |age_days: f64, offset_hours: i64| {
        let secs = unix_now() - (age_days * 86_400.0) as i64;
        let local = rfc3339(secs + offset_hours * 3600);
        format!(
            "{}{}{:02}:00",
            local.trim_end_matches('Z'),
            if offset_hours < 0 { '-' } else { '+' },
            offset_hours.abs()
        )
    };
    let put = |name: &str, date: String| {
        env.fake.with(|s| {
            let branches = &mut s.projects.get_mut(&REPO_ID).unwrap().branches;
            branches.clear();
            branches.push(Branch::new("recent", &days_ago(1)));
            branches.push(Branch::new(name, &date));
        });
    };
    // 13.95 days old at -05:00 is inside the 14-day window; read as UTC it would look older.
    put("edge-in", at(13.95, -5));
    env.poll().await;
    assert!(env.observed("branch").contains("edge-in"));
    // 14.1 days old at +05:00 is outside; read as UTC it would look newer.
    put("edge-out", at(14.1, 5));
    env.poll().await;
    assert_eq!(
        env.observed("branch"),
        BTreeSet::from(["recent".to_owned()])
    );

    for i in 0..201 {
        add_branch(&env, &format!("many/{i}"), 1);
    }
    let reports = env.poll().await;
    assert_eq!(reports[0].truncated, json!({"branches": 200}));
    assert_eq!(env.observed("branch").len(), 200);
}

#[test]
fn gitlab_configuration_is_vetted() {
    let dir = TempDir::new().unwrap();
    let with = |extra: &'static [(&'static str, &'static str)]| {
        SyncConfig::from_lookup("http://127.0.0.1:7332", dir.path().to_owned(), 14, |name| {
            extra
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        })
        .err()
        .map(|e| e.to_string())
    };
    assert_eq!(with(&[("AGENTSCM_GITLAB_TOKEN", "t")]), None);
    assert_eq!(
        with(&[
            ("AGENTSCM_GITLAB_TOKEN", "t"),
            ("AGENTSCM_GITLAB_WEBHOOK_SECRET", "h")
        ]),
        None
    );
    // Blank credentials are unset (compose passes unset variables as empty strings).
    assert_eq!(with(&[("AGENTSCM_GITLAB_TOKEN", "")]), None);
    assert_eq!(
        with(&[
            ("AGENTSCM_GITLAB_TOKEN", "t"),
            ("AGENTSCM_GITLAB_WEBHOOK_SECRET", "  ")
        ]),
        None
    );
    assert!(
        with(&[("AGENTSCM_GITLAB_WEBHOOK_SECRET", "h")])
            .unwrap()
            .contains("needs AGENTSCM_GITLAB_TOKEN")
    );
    // GitHub and GitLab on one host would share a PAT connection and repositories.
    let error = with(&[
        ("AGENTSCM_GITLAB_TOKEN", "t"),
        ("AGENTSCM_GITLAB_URL", "https://github.com"),
    ])
    .unwrap();
    assert!(error.contains("different hosts"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catchup_discovers_ordinary_gitlab_merges_and_keeps_rename_paths() {
    use agentscm::hub::catchup::CatchupQuery;
    use agentscm::hub::store::mint_bound_agent_token;
    let env = Env::start().await;
    add_mr(&env, 71, &["src/current.rs"]);
    env.mr(71, |mr| {
        mr.state = "merged".into();
        mr.merge_commit_sha = Some("a".repeat(40));
        mr.renamed.push(("src/old.rs".into(), "src/new.rs".into()));
    });
    let owner = env
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "gitlab".into(),
                host: env.host.clone(),
                user_id: 71,
            },
            "reader",
        )
        .unwrap();
    let (token, _, _) =
        mint_bound_agent_token(&*env.store, &owner.id, "reader", REPO, REPO_ID, 3600, true)
            .unwrap();
    env.poll().await;
    let page = env
        .store
        .work_catchup(&token, &CatchupQuery::default(), unix_now())
        .unwrap();
    assert_eq!(page.merges.len(), 1);
    assert_eq!(page.merges[0].pull_number, 71);
    assert_eq!(
        page.merges[0].paths,
        ["src/current.rs", "src/new.rs", "src/old.rs"]
    );
    assert!(page.merges[0].paths_complete);
    env.poll().await;
    let again = env
        .store
        .work_catchup(&token, &CatchupQuery::default(), unix_now())
        .unwrap();
    assert_eq!(again.merges, page.merges);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catchup_reads_gitlab_listing_times_with_a_utc_offset() {
    use agentscm::hub::catchup::{CatchupCoverageState, CatchupMergeSource, CatchupQuery};
    use agentscm::hub::store::mint_bound_agent_token;
    let env = Env::start().await;
    add_mr(&env, 72, &["src/offset.rs"]);
    env.mr(72, |mr| {
        mr.state = "merged".into();
        mr.merge_commit_sha = Some("b".repeat(40));
        mr.updated_at = "2099-01-01T12:00:00+02:00".into();
    });
    let owner = env
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "gitlab".into(),
                host: env.host.clone(),
                user_id: 72,
            },
            "reader",
        )
        .unwrap();
    let (token, _, _) =
        mint_bound_agent_token(&*env.store, &owner.id, "reader", REPO, REPO_ID, 3600, true)
            .unwrap();
    env.poll().await;
    let page = env
        .store
        .work_catchup(&token, &CatchupQuery::default(), unix_now())
        .unwrap();
    assert_eq!(page.merges.len(), 1);
    let forge = page
        .source_coverage
        .iter()
        .find(|source| source.source == CatchupMergeSource::ForgePull)
        .unwrap();
    assert_eq!(forge.state, CatchupCoverageState::ObservedOnly);
}
