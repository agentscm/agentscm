//! Synthetic provider HTTP -> authenticated Hub route. Every provider request is recorded,
//! including failures, to establish the observation path has no forge mutation calls.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscm::backend::LocalBackend;
use agentscm::hub::connection::NewConnection;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::{HubState, router};
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::{Json, Router};
use serde_json::{Value, json};
use tempfile::TempDir;

fn repository(provider: &str) -> &'static str {
    if provider == "github" {
        "group/widgets"
    } else {
        "group/sub/widgets"
    }
}

fn api_paths(provider: &str) -> Vec<String> {
    if provider == "github" {
        vec![
            "/repos/group/widgets".into(),
            "/repos/group/widgets/git/ref/heads/feature/topic".into(),
            "/repos/group/widgets/git/ref/heads/main".into(),
        ]
    } else {
        vec![
            "/projects/group%2Fsub%2Fwidgets".into(),
            "/projects/100/repository/branches/feature%2Ftopic".into(),
            "/projects/100/repository/branches/main".into(),
        ]
    }
}
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Fake {
    provider: String,
    id: i64,
    source: Option<String>,
    target: Option<String>,
    status: u16,
    delay: bool,
    wrong_branch: bool,
    requests: Vec<(Method, String)>,
}

async fn provider(
    State(fake): State<Arc<Mutex<Fake>>>,
    method: Method,
    uri: Uri,
) -> impl IntoResponse {
    let (status, body, delay) = {
        let mut fake = fake.lock().unwrap();
        fake.requests.push((method.clone(), uri.to_string()));
        assert_eq!(
            method,
            Method::GET,
            "continuity must never mutate the provider"
        );
        assert!(uri.query().is_none(), "unexpected provider query: {uri}");
        assert!(
            api_paths(&fake.provider)
                .iter()
                .any(|path| path == uri.path()),
            "unexpected provider path: {uri}"
        );
        let path = uri.path();
        let branch = path.contains("/git/ref/heads/") || path.contains("/repository/branches/");
        let source = path.ends_with("topic");
        let repository = repository(&fake.provider);
        if fake.status != 200 {
            (
                fake.status,
                json!({"message": "synthetic refusal"}),
                fake.delay,
            )
        } else if branch {
            let sha = if source { &fake.source } else { &fake.target };
            match sha {
                None => (404, json!({}), fake.delay),
                Some(sha) if fake.provider == "github" => (
                    200,
                    json!({
                        "ref": if source { "refs/heads/feature/topic" } else { "refs/heads/main" },
                        "object": {"sha": sha}
                    }),
                    fake.delay,
                ),
                Some(sha) => (
                    200,
                    json!({
                        "name": if fake.wrong_branch { "unrelated" } else if source { "feature/topic" } else { "main" },
                        "commit": {"id": sha, "committed_date": "2026-01-01T00:00:00Z"}
                    }),
                    fake.delay,
                ),
            }
        } else {
            (
                200,
                json!({"id": fake.id, "full_name": repository,
                "path_with_namespace": repository, "node_id": "R_100"}),
                fake.delay,
            )
        }
    };
    if delay {
        tokio::time::sleep(Duration::from_secs(11)).await;
    }
    (StatusCode::from_u16(status).unwrap(), Json(body))
}

struct Env {
    _dir: TempDir,
    repository: String,
    host: String,
    fake: Arc<Mutex<Fake>>,
    store: Arc<dyn Store>,
    url: String,
    token: String,
    token_id: String,
    connection: String,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Env {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
    }
}

impl Env {
    async fn start(provider_name: &str) -> Self {
        Self::start_with_host(provider_name, false).await
    }

    async fn start_with_host(provider_name: &str, different_host: bool) -> Self {
        let dir = TempDir::new().unwrap();
        let repository = repository(provider_name);
        let fake = Arc::new(Mutex::new(Fake {
            provider: provider_name.into(),
            id: 100,
            source: Some(HEAD.into()),
            target: Some(BASE.into()),
            status: 200,
            delay: false,
            wrong_branch: false,
            requests: Vec::new(),
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(provider).with_state(fake.clone());
        let provider_server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let host = if provider_name == "github" {
            "github.example.invalid"
        } else {
            "gitlab.example.invalid"
        };
        let configured_host = if different_host {
            "another.example.invalid"
        } else {
            host
        };
        let secrets = dir.path().join("secrets");
        sync::prepare_secrets_dir(&secrets, true).unwrap();
        let config = SyncConfig::from_lookup("http://127.0.0.1:1234", secrets, 14, |name| {
            match (provider_name, name) {
                ("github", "AGENTSCM_GITHUB_URL") => Some(format!("https://{configured_host}")),
                ("github", "AGENTSCM_GITHUB_API_URL") => Some(api.clone()),
                ("github", "AGENTSCM_GITHUB_PAT") => Some("synthetic-provider-token".into()),
                ("gitlab", "AGENTSCM_GITLAB_URL") => Some(format!("https://{configured_host}")),
                ("gitlab", "AGENTSCM_GITLAB_API_URL") => Some(api.clone()),
                ("gitlab", "AGENTSCM_GITLAB_TOKEN") => Some("synthetic-provider-token".into()),
                _ => None,
            }
        })
        .unwrap();
        let store: Arc<dyn Store> = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let connection = store
            .insert_connection(
                &NewConnection {
                    id: "con_11111111111111111111111111111111".into(),
                    provider: provider_name.into(),
                    host: host.into(),
                    kind: "pat".into(),
                    app_id: None,
                    app_slug: None,
                    acts_as: Some("synthetic".into()),
                },
                "test",
            )
            .unwrap()
            .id;
        store
            .upsert_repository(&connection, host, 100, repository, None, None)
            .unwrap();
        let owner = store
            .upsert_user(
                &ForgeIdentity {
                    provider: provider_name.into(),
                    host: host.into(),
                    user_id: 5,
                },
                "synthetic",
            )
            .unwrap();
        let (token_id, token, _) =
            mint_bound_agent_token(&*store, &owner.id, "synthetic", repository, 100, 3600, true)
                .unwrap();
        let state = HubState::new(
            store.clone(),
            Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
        )
        .with_sync(Sync::new(config).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = router(state);
        let hub_server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _dir: dir,
            repository: repository.into(),
            host: host.into(),
            fake,
            store,
            url,
            token: token.expose().to_owned(),
            token_id,
            connection,
            servers: vec![provider_server, hub_server],
        }
    }

    async fn get(&self, query: &str) -> (u16, Value) {
        let response = reqwest::Client::new()
            .get(format!(
                "{}/v1/hub/repos/{}/continuity?{query}",
                self.url,
                self.repository.replace('/', "%2F")
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        (
            response.status().as_u16(),
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap(),
        )
    }

    async fn read(&self) -> Value {
        let (status, body) = self
            .get("source_branch=feature%2Ftopic&target_branch=main")
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    fn only_reads(&self) {
        assert!(
            self.fake
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|(method, _)| *method == Method::GET)
        );
    }
}

#[tokio::test]
async fn github_and_gitlab_observe_fresh_refs_and_preserve_missing_or_changed_facts() {
    for provider in ["github", "gitlab"] {
        let env = Env::start(provider).await;
        let body = env.read().await;
        assert_eq!(body["availability"], "fresh");
        assert_eq!(body["repository_id"], 100);
        assert_eq!(body["provider"], provider);
        assert_eq!(body["host"], env.host);
        assert_eq!(body["repository"], repository(provider));
        assert_eq!(body["source_sha"], HEAD);
        assert_eq!(body["target_sha"], BASE);
        assert_eq!(body["authority"], "observation_only");
        let paths = api_paths(provider);
        assert_eq!(
            env.fake
                .lock()
                .unwrap()
                .requests
                .iter()
                .map(|(_, path)| path.clone())
                .collect::<Vec<_>>(),
            vec![
                paths[0].clone(),
                paths[1].clone(),
                paths[2].clone(),
                paths[0].clone()
            ]
        );
        {
            let mut fake = env.fake.lock().unwrap();
            fake.source = None;
            fake.target = Some("c".repeat(40));
        }
        let body = env.read().await;
        assert_eq!(body["availability"], "fresh");
        assert!(body["source_sha"].is_null());
        assert_eq!(body["target_sha"], "c".repeat(40));
        env.only_reads();
    }
}

#[tokio::test]
async fn provider_failures_and_numeric_substitution_never_become_fresh() {
    for provider in ["github", "gitlab"] {
        let env = Env::start(provider).await;
        for status in [401, 403, 429, 503] {
            env.fake.lock().unwrap().status = status;
            let body = env.read().await;
            assert_eq!(
                body["availability"], "unverified",
                "{provider} {status}: {body}"
            );
            assert!(body["observed_at"].is_null());
            assert!(body["source_sha"].is_null());
        }
        {
            let mut fake = env.fake.lock().unwrap();
            fake.status = 200;
            fake.id = 999;
            fake.requests.clear();
        }
        assert_eq!(env.read().await["availability"], "unverified");
        assert_eq!(env.fake.lock().unwrap().requests.len(), 1);
        env.only_reads();
    }
}

#[tokio::test]
async fn revoked_tokens_disabled_connections_and_bad_queries_do_not_contact_provider() {
    let env = Env::start("github").await;
    for query in [
        "source_branch=HEAD&target_branch=main",
        "source_branch=feature%2Ftopic&target_branch=main&extra=true",
        "source_branch=feature%2Ftopic&target_branch=refs%2Fheads%2Fmain",
    ] {
        assert_eq!(env.get(query).await.0, 400);
    }
    env.store
        .set_connection_status(&env.connection, "auth_failed", None)
        .unwrap();
    assert_eq!(env.read().await["availability"], "unverified");
    env.store.revoke_agent_token(&env.token_id).unwrap();
    assert_eq!(
        env.get("source_branch=feature%2Ftopic&target_branch=main")
            .await
            .0,
        401
    );
    assert!(env.fake.lock().unwrap().requests.is_empty());
}

#[tokio::test]
async fn malformed_gitlab_branch_and_invalid_sha_are_unverified() {
    let env = Env::start("gitlab").await;
    env.fake.lock().unwrap().wrong_branch = true;
    assert_eq!(env.read().await["availability"], "unverified");
    {
        let mut fake = env.fake.lock().unwrap();
        fake.wrong_branch = false;
        fake.source = Some("invalid".into());
    }
    assert_eq!(env.read().await["availability"], "unverified");
    env.only_reads();
}

#[tokio::test]
async fn provider_timeout_is_unverified() {
    let env = Env::start("github").await;
    env.fake.lock().unwrap().delay = true;
    assert_eq!(env.read().await["availability"], "unverified");
    env.only_reads();
}

#[tokio::test]
async fn reconfigured_provider_host_never_labels_new_endpoint_with_old_identity() {
    for provider in ["github", "gitlab"] {
        let env = Env::start_with_host(provider, true).await;
        assert_eq!(env.read().await["availability"], "unverified");
        assert!(env.fake.lock().unwrap().requests.is_empty());
    }
}

#[tokio::test]
async fn expired_hub_tokens_do_not_contact_either_provider() {
    let mut environments = Vec::new();
    for provider in ["github", "gitlab"] {
        let mut env = Env::start(provider).await;
        let principal = env
            .store
            .authenticate(
                &store::agent_token_digest(&env.token).unwrap(),
                store::unix_now(),
            )
            .unwrap()
            .unwrap();
        let (_, secret, _) = mint_bound_agent_token(
            &*env.store,
            &principal.owner.id,
            "short-lived",
            &env.repository,
            100,
            1,
            true,
        )
        .unwrap();
        env.token = secret.expose().to_owned();
        environments.push(env);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    for env in environments {
        assert_eq!(
            env.get("source_branch=feature%2Ftopic&target_branch=main")
                .await
                .0,
            401
        );
        assert!(env.fake.lock().unwrap().requests.is_empty());
    }
}

#[cfg(unix)]
fn fixture_git(root: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Synthetic")
        .env("GIT_AUTHOR_EMAIL", "synthetic@example.invalid")
        .env("GIT_COMMITTER_NAME", "Synthetic")
        .env("GIT_COMMITTER_EMAIL", "synthetic@example.invalid")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Git fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[cfg(unix)]
impl Env {
    async fn cli(&self, checkout: &std::path::Path, args: &[&str]) -> Value {
        let home = self._dir.path().canonicalize().unwrap();
        let checkout = checkout.to_path_buf();
        let arguments: Vec<String> = args.iter().map(|value| (*value).to_owned()).collect();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_agentscm"))
                .env_clear()
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join("config"))
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                // Ambient singleton values must not override the saved checkout binding.
                .env("AGENTSCM_HUB_URL", "http://127.0.0.1:1")
                .env(
                    "AGENTSCM_HUB_TOKEN_FILE",
                    home.join("nonexistent-ambient-token"),
                )
                .arg("continuity")
                .arg("--repo")
                .arg(checkout)
                .arg("--state-dir")
                .arg(home.join("continuity-state"))
                .args(arguments)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "continuity CLI failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cli_live_uses_saved_connection_for_each_provider_and_keeps_failures_unknown() {
    use std::os::unix::fs::PermissionsExt;
    for provider_name in ["github", "gitlab"] {
        let env = Env::start(provider_name).await;
        let home = env._dir.path().canonicalize().unwrap();
        let checkout = home.join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        fixture_git(&checkout, &["init", "-b", "main"]);
        fixture_git(&checkout, &["config", "commit.gpgsign", "false"]);
        let origin = format!("https://{}/{}.git", env.host, env.repository);
        fixture_git(&checkout, &["remote", "add", "origin", &origin]);
        std::fs::write(checkout.join("source.txt"), "synthetic base\n").unwrap();
        fixture_git(&checkout, &["add", "."]);
        fixture_git(&checkout, &["commit", "-m", "Synthetic base"]);
        let base = fixture_git(&checkout, &["rev-parse", "HEAD"]);
        fixture_git(&checkout, &["checkout", "-b", "feature/topic"]);
        std::fs::write(checkout.join("source.txt"), "synthetic change\n").unwrap();
        fixture_git(&checkout, &["commit", "-am", "Synthetic change"]);
        let head = fixture_git(&checkout, &["rev-parse", "HEAD"]);
        let token_path = home.join("saved-token");
        std::fs::write(&token_path, &env.token).unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let configuration = home.join("config/agentscm");
        std::fs::create_dir_all(&configuration).unwrap();
        std::fs::write(
            configuration.join("connections.json"),
            serde_json::to_vec(&json!({
                "version": 1, "connections": [{"root": checkout, "origin_host": env.host,
                    "hub": env.url, "repo": env.repository, "token_file": token_path, "agents": []}]
            }))
            .unwrap(),
        )
        .unwrap();
        {
            let mut fake = env.fake.lock().unwrap();
            fake.source = Some(head.clone());
            fake.target = Some(base);
        }
        env.cli(
            &checkout,
            &[
                "start",
                "--id",
                "task",
                "--base",
                "main",
                "--context",
                "Private context stays local",
                "--provider",
                provider_name,
                "--host",
                &env.host,
                "--repository",
                &env.repository,
            ],
        )
        .await;
        env.cli(
            &checkout,
            &[
                "draft",
                "--id",
                "task",
                "--draft",
                "draft",
                "--source-branch",
                "feature/topic",
                "--target-branch",
                "main",
                "--title",
                "Synthetic proposal",
                "--body",
                "Private proposal stays local",
            ],
        )
        .await;
        assert!(
            env.fake.lock().unwrap().requests.is_empty(),
            "local preparation contacted provider"
        );
        let live = ["revalidate", "--id", "task", "--live", "--draft", "draft"];
        let body = env.cli(&checkout, &live).await;
        assert_eq!(body["remote"]["availability"], "fresh");
        assert_eq!(body["remote"]["blockers"], json!([]));
        assert_eq!(body["remote"]["source_sha"], head);
        assert_eq!(body["publication_authorized"], false);
        {
            let mut fake = env.fake.lock().unwrap();
            fake.id = 999;
            fake.requests.clear();
        }
        let body = env.cli(&checkout, &live).await;
        assert_eq!(body["remote"]["freshness"], "unknown");
        assert_eq!(body["remote"]["reason"], "remote_identity_unverified");
        assert_eq!(body["publication_authorized"], false);
        assert_eq!(env.fake.lock().unwrap().requests.len(), 1);
        {
            let mut fake = env.fake.lock().unwrap();
            fake.id = 100;
            fake.status = 503;
        }
        let body = env.cli(&checkout, &live).await;
        assert_eq!(body["remote"]["freshness"], "unknown");
        assert_eq!(body["publication_authorized"], false);
        for (status, reason) in [(401, "forge_authentication_failed"), (429, "rate_limited")] {
            env.fake.lock().unwrap().status = status;
            let body = env.cli(&checkout, &live).await;
            assert_eq!(body["remote"]["freshness"], "unknown");
            assert_eq!(body["remote"]["reason"], reason);
            assert_eq!(body["publication_authorized"], false);
        }
        env.only_reads();
        assert!(
            !env.fake
                .lock()
                .unwrap()
                .requests
                .iter()
                .any(|(_, path)| path.contains("Private"))
        );
    }
}
