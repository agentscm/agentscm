//! Candidate branch and merge-request publication through the Hub for a GitLab connection:
//! an in-process Hub, a fake GitLab whose project is backed by a real bare Git repository,
//! and synthetic data only. The branch itself is pushed by the client over Git, so each test
//! moves refs in the bare repository directly.
#![cfg(unix)]
#[path = "support/fake_gitlab.rs"]
mod fake_gitlab;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::connection::NewConnection;
use agentscm::hub::publish::{derive_candidate_id, derive_id};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::{HubState, router};
use fake_gitlab::{Failure, FakeGitlab, Mr, Project, Token};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const REPO_ID: i64 = 100;
const TOKEN: &str = "synthetic-gitlab-token";
const GIT: &str = "/usr/bin/git";
const NESTED: &str = "group/sub/widgets";

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new(GIT)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Synthetic")
        .env("GIT_AUTHOR_EMAIL", "synthetic@example.invalid")
        .env("GIT_COMMITTER_NAME", "Synthetic")
        .env("GIT_COMMITTER_EMAIL", "synthetic@example.invalid")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "Git {args:?}: {:?}", output.stderr);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Env {
    fake: FakeGitlab,
    url: String,
    store: Arc<dyn Store>,
    dir: TempDir,
    bare: PathBuf,
    repo: String,
    /// The target tip, and a commit on top of it.
    seed: String,
    commit: String,
    host: String,
    connection: String,
    token: String,
    http: reqwest::Client,
}

fn bare_ref(bare: &Path, name: &str, sha: &str) {
    git(bare, &["update-ref", &format!("refs/heads/{name}"), sha]);
}

/// Serves a Hub over `store` on a fresh port; the URL it answers at.
async fn serve(dir: &Path, base: &str, store: Arc<dyn Store>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let secrets = dir.join("secrets");
    sync::prepare_secrets_dir(&secrets, true).unwrap();
    let base = base.to_owned();
    let config = SyncConfig::from_lookup(&url, secrets, 14, |name| match name {
        "AGENTSCM_GITLAB_URL" => Some(base.clone()),
        "AGENTSCM_GITLAB_TOKEN" => Some(TOKEN.into()),
        _ => None,
    })
    .unwrap();
    let state = HubState::new(
        store,
        Arc::new(LocalBackend::open(dir.join("objects")).unwrap()),
    )
    .with_sync(Sync::new(config).unwrap());
    sync::start(&state).await.unwrap();
    let app = router(state);
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

fn open_store(dir: &Path) -> Arc<dyn Store> {
    store::open(&format!("sqlite://{}", dir.join("hub.sqlite").display())).unwrap()
}

impl Env {
    async fn start() -> Self {
        Self::start_named(NESTED).await
    }

    async fn start_named(repo: &str) -> Self {
        let dir = TempDir::new().unwrap();
        // The forge's repository: `main`, and a candidate commit on top of it.
        let work = dir.path().join("work");
        fs::create_dir(&work).unwrap();
        git(&work, &["init", "--initial-branch=main"]);
        fs::write(work.join("old"), b"old bytes").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "base"]);
        let seed = git(&work, &["rev-parse", "HEAD"]);
        fs::write(work.join("old"), b"changed bytes").unwrap();
        git(&work, &["commit", "-am", "candidate"]);
        let commit = git(&work, &["rev-parse", "HEAD"]);
        let bare = dir.path().join("forge.git");
        fs::create_dir(&bare).unwrap();
        git(&bare, &["init", "--bare", "--initial-branch=main"]);
        git(
            &work,
            &[
                "push",
                bare.to_str().unwrap(),
                &format!("{seed}:refs/heads/main"),
                &format!("{commit}:refs/heads/agents/feat"),
            ],
        );

        let fake = FakeGitlab::start().await;
        fake.with(|s| {
            let mut project = Project::new(REPO_ID, repo, "private");
            project.git = Some(bare.clone());
            s.projects.insert(REPO_ID, project);
            s.tokens.insert(
                TOKEN.into(),
                Token {
                    username: "robot".into(),
                    user_id: 77,
                    access: [(REPO_ID, 30)].into(),
                },
            );
        });
        let store = open_store(dir.path());
        let url = serve(dir.path(), &fake.base, store.clone()).await;
        let host = fake
            .base
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .to_owned();
        let (setup, setup_host, name) = (store.clone(), host.clone(), repo.to_owned());
        let (connection, secret) = tokio::task::spawn_blocking(move || {
            let connection = setup.pat_connection(&setup_host).unwrap().unwrap().id;
            setup
                .upsert_repository(&connection, &setup_host, REPO_ID, &name, None, None)
                .unwrap();
            setup
                .set_connection_outbound(&connection, true, "test")
                .unwrap();
            let owner = setup
                .upsert_user(
                    &ForgeIdentity {
                        provider: "gitlab".into(),
                        host: setup_host,
                        user_id: 5,
                    },
                    "alice",
                )
                .unwrap();
            let (_, secret) = mint_agent_token(&*setup, &owner.id, "agent-a", &name, 3600).unwrap();
            (connection, secret.expose().to_owned())
        })
        .await
        .unwrap();
        Self {
            fake,
            url,
            store,
            dir,
            bare,
            repo: repo.to_owned(),
            seed,
            commit,
            host,
            connection,
            token: secret,
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
        }
    }

    /// A new Hub over the same database, as after a process restart.
    async fn restart(&mut self) {
        self.store = open_store(self.dir.path());
        self.url = serve(self.dir.path(), &self.fake.base, self.store.clone()).await;
    }

    fn base(&self) -> String {
        format!(
            "/v1/hub/repos/{}/publications",
            self.repo.replace('/', "%2F")
        )
    }

    async fn api(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(&self.token);
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

    /// A candidate manifest for `branch` (pointed at the candidate commit), created at the Hub.
    async fn create(&self, branch: &str) -> String {
        bare_ref(&self.bare, branch, &self.commit);
        let manifest = self.manifest(branch);
        let id = manifest["publication_id"].as_str().unwrap().to_owned();
        let (status, body) = self.api(Method::POST, &self.base(), Some(manifest)).await;
        assert_eq!(status, 200, "{body}");
        id
    }

    fn manifest(&self, branch: &str) -> Value {
        self.manifest_at(branch, &format!("https://{}/{}.git", self.host, self.repo))
    }

    fn manifest_at(&self, branch: &str, endpoint: &str) -> Value {
        let mut subject: agentscm::candidate::CandidateSubject = serde_json::from_value(json!({
            "id":"", "attachment":"1".repeat(64), "endpoint":endpoint, "output":branch,
            "change":{"repository":"2".repeat(64),"id":"change"}, "change_version":1,
            "context_digest":"3".repeat(64), "policy_epoch":0, "revision":"a".repeat(64),
            "target":"main", "baseline_commit":self.seed, "parent_commit":self.seed,
            "source_commit":self.seed, "source_fingerprint":"4".repeat(64),
            "comparison_fingerprint":"5".repeat(64)
        }))
        .unwrap();
        subject.id = subject.canonical_id().unwrap();
        json!({
            "publication_id": derive_candidate_id(&self.repo, &subject.id, &self.commit, branch),
            "revision": "a".repeat(64), "commit": self.commit, "tree": "c".repeat(40),
            "parent": self.seed, "branch": branch, "base": "main",
            "title": "Candidate", "body": "Synthetic body", "message": "msg\n",
            "author": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
            "committer": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
            "changes": [{"path":"old","mode":"000000","blob":null,"sha256":null,"size":0}],
            "candidate": {"subject":subject,"endpoint":endpoint,"previous_publication":null}
        })
    }

    async fn run(&self, id: &str) -> (u16, Value) {
        self.api(
            Method::POST,
            &format!("{}/{id}/run", self.base()),
            Some(json!({})),
        )
        .await
    }

    async fn recover(&self, id: &str) -> (u16, Value) {
        self.api(
            Method::POST,
            &format!("{}/{id}/recover", self.base()),
            Some(json!({})),
        )
        .await
    }

    async fn show(&self, id: &str) -> (u16, Value) {
        self.api(Method::GET, &format!("{}/{id}", self.base()), None)
            .await
    }

    fn stored(&self, id: &str) -> agentscm::hub::publish::Publication {
        self.store.publication(&self.repo, id).unwrap().unwrap()
    }

    fn creates(&self) -> usize {
        self.fake.with(|s| s.mr_creates())
    }

    fn mrs(&self) -> Vec<Mr> {
        self.fake
            .with(|s| s.projects[&REPO_ID].mrs.values().cloned().collect())
    }

    fn marker(&self, id: &str) -> String {
        format!("<!-- agentscm-candidate:{id}:{} -->", self.commit)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creates_one_merge_request_in_a_nested_group() {
    let env = Env::start().await;
    let id = env.create("agents/feat").await;
    let (status, out) = env.run(&id).await;
    assert_eq!(status, 200, "{out}");
    assert!(out.get("error").is_none(), "{out}");
    assert_eq!(out["states"]["pull_request"]["state"], "opened");
    assert_eq!(out["states"]["pull_request"]["number"], 1);
    assert!(
        out["states"]["pull_request"]["html_url"]
            .as_str()
            .unwrap()
            .ends_with("/group/sub/widgets/-/merge_requests/1")
    );
    let mrs = env.mrs();
    assert_eq!(mrs.len(), 1);
    assert_eq!(
        (mrs[0].source_branch.as_str(), mrs[0].target_branch.as_str()),
        ("agents/feat", "main")
    );
    assert_eq!(mrs[0].sha, env.commit);
    assert!(
        mrs[0].description.starts_with("Synthetic body")
            && mrs[0].description.lines().any(|l| l == env.marker(&id))
    );
    assert_eq!(env.creates(), 1);
    // Done is done: another run reads nothing and creates nothing.
    env.run(&id).await;
    assert_eq!(env.creates(), 1);
    assert_eq!(env.stored(&id).pr_number, Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adopts_an_exact_existing_merge_request_without_creating() {
    let env = Env::start().await;
    let id = env.create("agents/feat").await;
    env.fake.with(|s| {
        let mut mr = Mr::new(5, "agents/feat", &[]);
        mr.sha = env.commit.clone();
        mr.description = format!("older text\n\n{}", env.marker(&id));
        s.projects.get_mut(&REPO_ID).unwrap().mrs.insert(5, mr);
    });
    let (_, out) = env.run(&id).await;
    assert_eq!(out["states"]["pull_request"]["number"], 5, "{out}");
    assert_eq!(env.creates(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_or_malformed_reply_stays_uncertain_until_recovered_by_marker() {
    let env = Env::start().await;
    for mode in 0..2 {
        let branch = format!("agents/uncertain-{mode}");
        let id = env.create(&branch).await;
        let before = env.creates();
        env.fake.with(|s| {
            if mode == 0 {
                s.lose_next_mr_reply = true;
            } else {
                s.malformed_next_mr_reply = true;
            }
        });
        let (_, first) = env.run(&id).await;
        assert!(first["error"].is_string(), "{first}");
        assert_eq!(env.stored(&id).branch_state, "pr_attempting");
        assert_eq!(env.creates(), before + 1);
        // No automatic replay, and no blind recreation, even while the list does not show it.
        let hidden = env
            .fake
            .with(|s| std::mem::take(&mut s.projects.get_mut(&REPO_ID).unwrap().mrs));
        let (_, again) = env.run(&id).await;
        assert!(again["error"].is_string(), "{again}");
        assert_eq!(env.stored(&id).branch_state, "pr_attempting");
        assert_eq!(env.creates(), before + 1);
        env.fake
            .with(|s| s.projects.get_mut(&REPO_ID).unwrap().mrs = hidden);
        let (status, recovered) = env.recover(&id).await;
        assert_eq!(status, 200, "{recovered}");
        assert!(
            recovered["states"]["pull_request"]["number"].is_i64(),
            "{recovered}"
        );
        assert_eq!(env.creates(), before + 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_the_uncertainty_and_recovery_adopts() {
    let mut env = Env::start().await;
    let id = env.create("agents/feat").await;
    env.fake.with(|s| s.lose_next_mr_reply = true);
    env.run(&id).await;
    assert_eq!(env.stored(&id).branch_state, "pr_attempting");
    env.restart().await;
    assert_eq!(env.stored(&id).branch_state, "pr_attempting");
    let hidden = env
        .fake
        .with(|s| std::mem::take(&mut s.projects.get_mut(&REPO_ID).unwrap().mrs));
    let (_, out) = env.run(&id).await;
    assert!(out["error"].is_string(), "{out}");
    assert_eq!(env.creates(), 1, "a restart grants no retry");
    env.fake
        .with(|s| s.projects.get_mut(&REPO_ID).unwrap().mrs = hidden);
    let (_, recovered) = env.recover(&id).await;
    assert_eq!(
        recovered["states"]["pull_request"]["number"], 1,
        "{recovered}"
    );
    assert_eq!(env.creates(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusals_are_recorded_and_a_duplicate_is_only_uncertain() {
    let env = Env::start().await;
    for status in [400, 403] {
        let id = env.create(&format!("agents/refused-{status}")).await;
        env.fake.with(|s| s.refuse_next_mr = Some(status));
        let (_, out) = env.run(&id).await;
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains(&format!("refused ({status})")),
            "{out}"
        );
        assert_eq!(env.stored(&id).branch_state, "pr_refused");
        let creates = env.creates();
        env.run(&id).await;
        assert_eq!(env.creates(), creates, "a refusal is not retried");
        assert!(env.stored(&id).pr_number.is_none());
    }
    // An open MR for the same branches that is not this candidate's makes GitLab answer 409.
    let id = env.create("agents/dup").await;
    env.fake.with(|s| {
        let mut mr = Mr::new(50, "agents/dup", &[]);
        mr.sha = "f".repeat(40);
        s.projects.get_mut(&REPO_ID).unwrap().mrs.insert(50, mr);
    });
    let creates = env.creates();
    let (_, out) = env.run(&id).await;
    assert!(out["error"].is_string(), "{out}");
    assert_eq!(env.creates(), creates + 1);
    assert_eq!(env.stored(&id).branch_state, "pr_attempting");
    assert!(env.stored(&id).pr_number.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_merge_request_is_not_adopted() {
    let env = Env::start().await;
    let id = env.create("agents/feat").await;
    env.fake.with(|s| {
        let mut fork = Mr::new(9, "agents/feat", &[]);
        fork.sha = env.commit.clone();
        fork.description = env.marker(&id);
        fork.source_project_id = Some(999);
        s.projects.get_mut(&REPO_ID).unwrap().mrs.insert(9, fork);
    });
    let (_, out) = env.run(&id).await;
    assert_eq!(env.creates(), 1);
    assert_eq!(out["states"]["pull_request"]["number"], 10, "{out}");
    // Recovery of a publication with no MR of its own does not take the fork's either.
    let other = env.create("agents/other").await;
    env.fake.with(|s| {
        let mut fork = Mr::new(20, "agents/other", &[]);
        fork.sha = env.commit.clone();
        fork.source_project_id = Some(999);
        s.projects.get_mut(&REPO_ID).unwrap().mrs.insert(20, fork);
    });
    env.fake.with(|s| s.lose_next_mr_reply = true);
    env.run(&other).await;
    env.fake.with(|s| {
        s.projects
            .get_mut(&REPO_ID)
            .unwrap()
            .mrs
            .retain(|_, mr| mr.source_project_id.is_some() || mr.source_branch != "agents/other")
    });
    let (_, recovered) = env.recover(&other).await;
    assert!(
        recovered["states"]["pull_request"]["number"].is_null(),
        "{recovered}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_head_or_target_blocks_before_any_create() {
    let env = Env::start().await;
    let id = env.create("agents/feat").await;
    bare_ref(&env.bare, "agents/feat", &env.seed);
    let (_, out) = env.run(&id).await;
    assert!(out["error"].as_str().unwrap().contains("differs"), "{out}");
    assert_eq!(env.creates(), 0);
    bare_ref(&env.bare, "agents/feat", &env.commit);
    bare_ref(&env.bare, "main", &env.commit);
    let (_, out) = env.run(&id).await;
    assert!(out["error"].as_str().unwrap().contains("differs"), "{out}");
    assert_eq!(env.creates(), 0);
    assert!(env.stored(&id).pr_number.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limits_and_truncated_searches_fail_without_creating() {
    let env = Env::start().await;
    let id = env.create("agents/feat").await;
    for path in ["/repository/branches", "/merge_requests?"] {
        env.fake.with(|s| {
            s.fail.push_back(Failure {
                path_contains: path.into(),
                status: 429,
                headers: vec![("retry-after".into(), "1".into())],
            })
        });
        let (_, out) = env.run(&id).await;
        assert!(out["error"].is_string(), "{path}: {out}");
        assert_eq!(env.creates(), 0, "{path}");
    }
    env.fake.with(|s| {
        s.fail.clear();
        let project = s.projects.get_mut(&REPO_ID).unwrap();
        for iid in 1..=3000 {
            let mut mr = Mr::new(iid, "agents/feat", &[]);
            mr.sha = "e".repeat(40);
            project.mrs.insert(iid, mr);
        }
    });
    let (_, out) = env.run(&id).await;
    assert!(out["error"].as_str().unwrap().contains("too many"), "{out}");
    assert_eq!(env.creates(), 0);
    assert!(env.stored(&id).pr_number.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_outbound_or_lost_access_blocks_without_creating() {
    let env = Env::start().await;
    let id = env.create("agents/feat").await;
    let (store, connection) = (env.store.clone(), env.connection.clone());
    tokio::task::spawn_blocking(move || {
        store
            .set_connection_outbound(&connection, false, "test")
            .unwrap()
    })
    .await
    .unwrap();
    let (status, out) = env.run(&id).await;
    assert_eq!(
        (status, out["error"].as_str()),
        (409, Some("outbound_disabled"))
    );
    let (store, connection) = (env.store.clone(), env.connection.clone());
    tokio::task::spawn_blocking(move || {
        store
            .set_connection_outbound(&connection, true, "test")
            .unwrap()
    })
    .await
    .unwrap();
    env.fake
        .with(|s| s.tokens.get_mut(TOKEN).unwrap().access.clear());
    let (_, out) = env.run(&id).await;
    assert!(out["error"].is_string(), "{out}");
    assert_eq!(env.creates(), 0);
    assert!(env.stored(&id).pr_number.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_external_merge_ends_the_lineage_only_for_the_published_commit() {
    let env = Env::start().await;
    let cases = [
        // Fast-forward: no merge commit, the head is the published commit.
        ("agents/ff", None, None, true),
        // Fast-forward after a rebase on the forge: the head is not what was published.
        ("agents/rebased", None, Some("9".repeat(40)), false),
        ("agents/merge", Some("8".repeat(40)), None, true),
    ];
    for (branch, merge_commit, head, merged) in cases {
        let id = env.create(branch).await;
        let (_, out) = env.run(&id).await;
        let number = out["states"]["pull_request"]["number"].as_i64().unwrap();
        env.fake.with(|s| {
            let mr = s
                .projects
                .get_mut(&REPO_ID)
                .unwrap()
                .mrs
                .get_mut(&number)
                .unwrap();
            mr.state = "merged".into();
            mr.merge_commit_sha = merge_commit.clone();
            if let Some(head) = &head {
                mr.sha = head.clone();
            }
        });
        let (_, shown) = env.show(&id).await;
        let pull = &shown["states"]["pull_request"];
        if merged {
            assert_eq!(pull["state"], "merged", "{branch}: {shown}");
            let expected = merge_commit.unwrap_or_else(|| env.commit.clone());
            assert_eq!(pull["merge_commit"], expected.as_str(), "{branch}");
        } else {
            assert_eq!(pull["state"], "opened", "{branch}: {shown}");
            assert!(pull["merge_commit"].is_null());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_publication_never_reaches_a_gitlab_connection() {
    let env = Env::start().await;
    let commit = "b".repeat(40);
    let blob: String = Sha256::digest(b"hello")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let body = json!({
        "publication_id": derive_id(&env.repo, 1, &commit, "feat/event"),
        "revision": "a".repeat(64), "event_seq": 1, "commit": commit,
        "tree": "c".repeat(40), "parent": "d".repeat(40), "branch": "feat/event", "base": "main",
        "title": "t", "body": "", "message": "msg\n",
        "author": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "committer": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "changes": [{"path": "a.txt", "mode": "100644", "blob": "e".repeat(40),
            "sha256": blob, "size": 5}]
    });
    let before = env.fake.with(|s| s.requests.len());
    let (status, refused) = env.api(Method::POST, &env.base(), Some(body)).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (409, Some("unsupported_provider"))
    );
    assert_eq!(env.fake.with(|s| s.requests.len()), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_endpoint_under_a_url_root_or_a_name_shared_with_another_forge_fails_closed() {
    let env = Env::start_named("group/widgets").await;
    let endpoint = format!("https://{}/gitlab/group/widgets.git", env.host);
    let rooted = env.manifest_at("agents/rooted", &endpoint);
    let id = rooted["publication_id"].as_str().unwrap().to_owned();
    let before = env.fake.with(|s| s.requests.len());
    let (status, _) = env.api(Method::POST, &env.base(), Some(rooted)).await;
    // The prefix is never stripped: the manifest is refused and nothing is stored or sent.
    assert!(matches!(status, 400 | 409), "{status}");
    assert!(env.store.publication(&env.repo, &id).unwrap().is_none());
    assert_eq!(env.fake.with(|s| s.requests.len()), before);

    // The same name is connected on the GitHub host: publications are keyed by name alone.
    let store = env.store.clone();
    tokio::task::spawn_blocking(move || {
        let conn = store
            .insert_connection(
                &NewConnection {
                    id: format!("con_{}", "c".repeat(32)),
                    provider: "github".into(),
                    host: "github.com".into(),
                    kind: "pat".into(),
                    app_id: None,
                    app_slug: None,
                    acts_as: Some("bot".into()),
                },
                "test",
            )
            .unwrap();
        store
            .upsert_repository(&conn.id, "github.com", 7, "group/widgets", None, None)
            .unwrap();
    })
    .await
    .unwrap();
    bare_ref(&env.bare, "agents/shared", &env.commit);
    let before = env.fake.with(|s| s.requests.len());
    let (status, out) = env
        .api(
            Method::POST,
            &env.base(),
            Some(env.manifest("agents/shared")),
        )
        .await;
    assert_eq!(
        (status, out["error"].as_str()),
        (409, Some("ambiguous_repository_name"))
    );
    assert_eq!(env.fake.with(|s| s.requests.len()), before);
}
