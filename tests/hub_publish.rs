//! Publishing a native revision as a GitHub branch and pull request, end to end: a real
//! git-export mirror, the `agentscm hub publish` binary, an in-process hub, and a fake
//! GitHub backed by a real bare Git repository. Synthetic data only.
#![cfg(unix)]
#[path = "support/fake_github.rs"]
mod fake_github;
#[path = "support/git_https.rs"]
mod https_fixture;
#[path = "common/hub_pg.rs"]
mod hub_pg;
use https_fixture::HttpsFixture;

fn git() -> PathBuf {
    PathBuf::from(GIT)
}

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agentscm::authority::Authority;
use agentscm::backend::LocalBackend;
use agentscm::context::ReviewContext;
use agentscm::git_export::{CommitMetadata, ExportRequest, Mirror};
use agentscm::hub::publish::derive_id;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::{HubState, router};
use agentscm::model::{Approval, FileChange, IntegrateRequest, IntegrationEvent};
use agentscm::store::Engine;
use fake_github::{Failure, FakeGithub, Pull, Repo, bare_clone, git as bare_git};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 100;
const PAT: &str = "ghp_synthetic_pat";
const GIT: &str = "/usr/bin/git";

fn run(path: &Path, args: &[&str]) -> String {
    let output = Command::new(GIT)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "Git {args:?}: {:?}", output.stderr);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn change(path: &str, bytes: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(bytes.to_vec()),
    }
}

/// Lands `changes` on a fresh `main` and returns the integration event.
fn integrate(
    engine: &Engine,
    authority: &mut Authority,
    changes: Vec<FileChange>,
) -> IntegrationEvent {
    let revision = engine.checkpoint(None, &changes).unwrap();
    let repository = authority.repository_id().unwrap();
    let context: ReviewContext = serde_json::from_value(json!({
        "version":1,"change":{"repository":repository,"id":"change-1"},
        "change_version":1,"revision":revision,"base":null,
        "target":"main","policy_epoch":0,"intent":"synthetic publication",
        "acceptance_criteria":["exact byte fidelity"],"scope":["source"],"exclusions":[],"omissions":[],"dependencies":[],
        "recipe":{"state":"digest","sha256":"b".repeat(64)},
        "toolchain":{"state":"digest","sha256":"c".repeat(64)},
        "environment":{"state":"digest","sha256":"d".repeat(64)},
        "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
    }))
    .unwrap();
    let state = authority.start_change(&context).unwrap();
    let approval = Approval {
        approval_id: "approval-1".into(),
        target: "main".into(),
        revision: revision.clone(),
        context_digest: state.context_digest.clone(),
        change: Some(state.id.clone()),
        change_version: Some(1),
        policy_epoch: 0,
    };
    authority.approve(&approval).unwrap();
    authority
        .integrate(
            engine,
            &IntegrateRequest {
                operation_id: "integrate-1".into(),
                target: "main".into(),
                expected_target: None,
                revision,
                context_digest: state.context_digest,
                approval_id: approval.approval_id,
                change: Some(state.id),
                change_version: Some(1),
                policy_epoch: 0,
            },
        )
        .unwrap()
}

/// The native files of the published revision; `second` of the base commit is gone.
const FILES: [(&str, &[u8]); 4] = [
    ("old", b"old bytes, now changed"),
    ("binary", &[0, 255, 1]),
    ("empty", b""),
    ("new/dir/file.txt", b"brand new file\n"),
];

struct Env {
    fake: FakeGithub,
    url: String,
    store: Arc<dyn Store>,
    root: TempDir,
    bare: PathBuf,
    revision: String,
    commit: String,
    seed: String,
    connection: String,
    token: String,
    token_file: PathBuf,
    owner: String,
    http: reqwest::Client,
}

impl Env {
    async fn start() -> Self {
        Self::start_with_transport(None).await
    }

    async fn start_with_transport(transport: Option<&HttpsFixture>) -> Self {
        Self::start_with_database(transport, None).await
    }

    async fn start_with_database(transport: Option<&HttpsFixture>, database: Option<&str>) -> Self {
        let root = TempDir::new().unwrap();
        // The forge's base: one commit with a file the revision deletes.
        let seed_repo = root.path().join("seed");
        fs::create_dir(&seed_repo).unwrap();
        run(
            &seed_repo,
            &["init", "--object-format=sha1", "--initial-branch=main"],
        );
        run(&seed_repo, &["config", "user.name", "Seed Test"]);
        run(
            &seed_repo,
            &["config", "user.email", "seed@example.invalid"],
        );
        fs::write(seed_repo.join("old"), b"old bytes").unwrap();
        fs::write(seed_repo.join("binary"), [0, 255, 1]).unwrap();
        fs::write(seed_repo.join("empty"), b"").unwrap();
        fs::write(seed_repo.join("second"), b"deleted by the revision").unwrap();
        run(&seed_repo, &["add", "."]);
        run(&seed_repo, &["commit", "-m", "base"]);
        let seed = run(&seed_repo, &["rev-parse", "HEAD"]);

        // Native data, one integrated revision, and its seeded git-export mirror.
        let data = root.path().join("data");
        let engine = Engine::open(&data).unwrap();
        let mut authority = Authority::open(&data).unwrap();
        let event = integrate(
            &engine,
            &mut authority,
            FILES.iter().map(|(p, b)| change(p, b)).collect(),
        );
        let mirror_path = root.path().join("mirror.git");
        let mut mirror = Mirror::initialize_with_seed(
            Path::new(GIT),
            &mirror_path,
            &authority.repository_id().unwrap(),
            "main",
            &seed_repo,
            &seed,
        )
        .unwrap();
        let exported = mirror
            .export(
                &engine,
                &authority,
                &ExportRequest {
                    event_sequence: event.sequence,
                    expected_oid: Some(seed.clone()),
                    metadata: CommitMetadata {
                        name: "Snapshot test".into(),
                        email: "test@example.invalid".into(),
                        timestamp: 1_780_000_000,
                        message: "Export native snapshot".into(),
                    },
                },
            )
            .unwrap();
        drop((mirror, authority, engine));

        // The fake forge holds the same base commit.
        let fake = FakeGithub::start().await;
        let bare = if let Some(transport) = transport {
            run(
                &transport.remote,
                &[
                    "fetch",
                    seed_repo.to_str().unwrap(),
                    &format!("{seed}:refs/heads/main"),
                ],
            );
            transport.remote.clone()
        } else {
            bare_clone(root.path(), &seed_repo)
        };
        fake.with(|s| {
            let mut repo = Repo::new(REPO_ID, REPO);
            repo.git = Some(bare.clone());
            s.repos.insert(REPO_ID, repo);
            s.pats
                .insert(PAT.into(), ("robot".into(), 77, [REPO_ID].into()));
        });

        let dir = root.path().join("hub");
        fs::create_dir(&dir).unwrap();
        let local_url = format!("sqlite://{}", dir.join("hub.sqlite").display());
        let database = database.unwrap_or(&local_url).to_owned();
        let store = tokio::task::spawn_blocking(move || store::open(&database).unwrap())
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let secrets = dir.join("secrets");
        sync::prepare_secrets_dir(&secrets, true).unwrap();
        let base = fake.base.clone();
        let web = transport
            .map(|t| t.url.trim_end_matches("/acme/widgets.git").to_owned())
            .unwrap_or_else(|| base.clone());
        let config = SyncConfig::from_lookup(&url, secrets, 14, |name| match name {
            "AGENTSCM_GITHUB_URL" => Some(web.clone()),
            "AGENTSCM_GITHUB_API_URL" => Some(base.clone()),
            "AGENTSCM_GITHUB_PAT" => Some(PAT.into()),
            _ => None,
        })
        .unwrap();
        let state = HubState::new(
            store.clone(),
            Arc::new(LocalBackend::open(dir.join("objects")).unwrap()),
        )
        .with_sync(Sync::new(config).unwrap());
        sync::start(&state).await.unwrap();
        let app = router(state);
        tokio::spawn(async move { axum::serve(listener, app).await });

        // The repository is connected, outbound writes are on, and an agent holds a token.
        let host = web
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_owned();
        let setup_store = store.clone();
        let (connection, owner, secret) = tokio::task::spawn_blocking(move || {
            let store = setup_store;
            let connection = store.pat_connection(&host).unwrap().unwrap().id;
            store
                .upsert_repository(&connection, &host, REPO_ID, REPO, None, None)
                .unwrap();
            store
                .set_connection_outbound(&connection, true, "test")
                .unwrap();
            let owner = store
                .upsert_user(
                    &ForgeIdentity {
                        provider: "github".into(),
                        host,
                        user_id: 5,
                    },
                    "alice",
                )
                .unwrap();
            let (_, secret) = mint_agent_token(&*store, &owner.id, "agent-a", REPO, 3600).unwrap();
            (connection, owner.id, secret.expose().to_owned())
        })
        .await
        .unwrap();
        let token_file = root.path().join("token");
        fs::write(&token_file, &secret).unwrap();
        Self {
            fake,
            url,
            store,
            bare,
            revision: event.after.revision,
            commit: exported.record.commit.unwrap(),
            seed,
            connection,
            token: secret,
            token_file,
            owner,
            root,
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
        }
    }

    /// `agentscm hub publish` for event 1; the exit status and parsed stdout.
    async fn publish(&self, branch: &str) -> (bool, Value, String) {
        let root = self.root.path().to_owned();
        let (url, token_file, branch) =
            (self.url.clone(), self.token_file.clone(), branch.to_owned());
        let output = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_agentscm"))
                .env("AGENTSCM_HUB_URL", url)
                .env("AGENTSCM_HUB_TOKEN_FILE", token_file)
                .args(["--data-dir"])
                .arg(root.join("data"))
                .args(["hub", "publish", "--mirror"])
                .arg(root.join("mirror.git"))
                .args(["--event", "1", "--branch", &branch, "--base", "main"])
                .args(["--title", "Publish the revision", "--git", GIT])
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        let stdout = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        (
            output.status.success(),
            stdout,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    async fn api(
        &self,
        method: Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(token);
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

    fn events(&self) -> Vec<(String, Value)> {
        self.store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind.starts_with("publication_"))
            .map(|e| (e.kind, serde_json::from_str(&e.data).unwrap()))
            .collect()
    }

    fn branch_tip(&self, branch: &str) -> Option<String> {
        let out = bare_git(
            &self.bare,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{branch}"),
            ],
            &[],
            None,
        )
        .ok()?;
        Some(String::from_utf8(out).unwrap().trim().to_owned())
    }

    fn requests(&self, method: &str, containing: &str) -> usize {
        self.fake.with(|s| {
            s.requests
                .iter()
                .filter(|r| r.method == method && r.path_and_query.contains(containing))
                .count()
        })
    }

    fn writes(&self) -> usize {
        self.fake
            .with(|s| s.requests.iter().filter(|r| r.method != "GET").count())
    }

    fn pulls(&self) -> Vec<fake_github::Pull> {
        self.fake
            .with(|s| s.repos[&REPO_ID].pulls.values().cloned().collect())
    }

    fn connection_status(&self) -> String {
        self.store
            .connection(&self.connection)
            .unwrap()
            .unwrap()
            .status
    }
}

fn kinds(events: &[(String, Value)]) -> Vec<&str> {
    events.iter().map(|(k, _)| k.as_str()).collect()
}

#[tokio::test]
async fn publishes_a_native_revision_as_a_branch_and_pull_request() {
    let env = Env::start().await;
    let (ok, out, err) = env.publish("feat/publish").await;
    assert!(ok, "{err} {out}");
    assert_eq!(out["states"]["native_landed"], true);
    assert_eq!(out["states"]["branch"]["state"], "created");
    assert_eq!(out["states"]["branch"]["commit"], env.commit.as_str());
    assert_eq!(out["states"]["branch"]["ref"], "refs/heads/feat/publish");
    assert_eq!(out["states"]["pull_request"]["state"], "opened");
    assert_eq!(out["states"]["pull_request"]["number"], 1);
    assert!(out.get("error").is_none());

    // The branch is exactly the mirror's commit: the same SHA is the byte-equality proof.
    assert_eq!(env.branch_tip("feat/publish"), Some(env.commit.clone()));
    let engine = Engine::open(env.root.path().join("data")).unwrap();
    for (path, bytes) in FILES {
        let in_forge = bare_git(
            &env.bare,
            &["cat-file", "blob", &format!("{}:{path}", env.commit)],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(in_forge, bytes, "{path}");
        assert_eq!(engine.read_file(&env.revision, path).unwrap(), bytes);
    }
    // The base commit's other file is deleted, and the parent is the forge's base.
    assert!(
        bare_git(
            &env.bare,
            &["cat-file", "-e", &format!("{}:second", env.commit)],
            &[],
            None
        )
        .is_err()
    );
    let parent = bare_git(
        &env.bare,
        &["rev-parse", &format!("{}^", env.commit)],
        &[],
        None,
    )
    .unwrap();
    assert_eq!(String::from_utf8(parent).unwrap().trim(), env.seed);

    // One pull request from that branch, attributed.
    let pulls = env.pulls();
    assert_eq!(pulls.len(), 1);
    let pull = &pulls[0];
    assert_eq!(
        (pull.head_ref.as_str(), pull.base_ref.as_str()),
        ("feat/publish", "main")
    );
    assert_eq!(pull.head_sha, env.commit);
    assert_eq!(pull.title, "Publish the revision");
    let footer = format!(
        "Published by AgentSCM for alice's agent (agent-a) from revision {}.",
        env.revision
    );
    // An empty description is just the footer.
    assert_eq!(pull.body, footer);

    // Every state change is in the event log with the exact mappings.
    let events = env.events();
    assert_eq!(
        kinds(&events),
        [
            "publication_requested",
            "publication_branch_created",
            "publication_pr_opened"
        ]
    );
    let created = &events[1].1;
    assert_eq!(created["ref"], "refs/heads/feat/publish");
    assert_eq!(
        (created["commit"].as_str(), created["revision"].as_str()),
        (Some(env.commit.as_str()), Some(env.revision.as_str()))
    );
    let opened = &events[2].1;
    assert_eq!(opened["number"], 1);
    assert_eq!(opened["node_id"], pull.node_id.as_str());
    assert_eq!(opened["commit"], env.commit.as_str());
    assert_eq!(opened["revision"], env.revision.as_str());
    assert_eq!(env.connection_status(), "enabled");

    // Rerunning after success changes nothing on the forge or in the log.
    let (writes, logged) = (env.writes(), env.events().len());
    let (ok, again, err) = env.publish("feat/publish").await;
    assert!(ok, "{err}");
    assert_eq!(again["states"], out["states"]);
    assert_eq!((env.writes(), env.events().len()), (writes, logged));
    assert_eq!(env.pulls().len(), 1);

    // Done is done: the branch moved, then deleted (as after a merge), is not this run's business,
    // and the run does not even look at it.
    let lookups = env.requests("GET", "/git/ref/");
    let update = |args: &[&str]| bare_git(&env.bare, args, &[], None).unwrap();
    update(&["update-ref", "refs/heads/feat/publish", &env.seed]);
    let (ok, moved, err) = env.publish("feat/publish").await;
    assert!(ok, "{err}");
    update(&["update-ref", "-d", "refs/heads/feat/publish"]);
    let (ok, deleted, err) = env.publish("feat/publish").await;
    assert!(ok, "{err}");
    assert_eq!(
        (&moved["states"], &deleted["states"]),
        (&out["states"], &out["states"])
    );
    assert!(moved.get("error").is_none() && deleted.get("error").is_none());
    assert_eq!((env.writes(), env.events().len()), (writes, logged));
    assert_eq!(env.requests("GET", "/git/ref/"), lookups);

    // A merge on the forge is recorded once, with its merge commit.
    let merge = "7".repeat(40);
    env.fake.with(|s| {
        let pull = s
            .repos
            .get_mut(&REPO_ID)
            .unwrap()
            .pulls
            .get_mut(&1)
            .unwrap();
        (pull.state, pull.merged, pull.merge_commit_sha) =
            ("closed".into(), true, Some(merge.clone()));
    });
    let id = out["id"].as_str().unwrap();
    let path = format!("/v1/hub/repos/acme%2Fwidgets/publications/{id}");
    for _ in 0..2 {
        let (status, shown) = env.api(Method::GET, &path, &env.token, None).await;
        assert_eq!(status, 200);
        assert_eq!(shown["states"]["pull_request"]["state"], "merged");
        assert_eq!(
            shown["states"]["pull_request"]["merge_commit"],
            merge.as_str()
        );
    }
    let merged: Vec<_> = env
        .events()
        .into_iter()
        .filter(|(k, _)| k == "publication_merged")
        .collect();
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].1["merge_commit"], merge.as_str());
    assert_eq!(merged[0].1["number"], 1);
}

#[tokio::test]
async fn a_lost_pull_request_reply_is_adopted_not_duplicated() {
    let env = Env::start().await;
    env.fake.with(|s| s.lose_next_pull_reply = true);
    let (ok, out, err) = env.publish("feat/lost").await;
    assert!(!ok);
    assert!(err.contains("publication incomplete"), "{err}");
    assert_eq!(out["states"]["branch"]["state"], "created");
    assert_eq!(out["states"]["pull_request"]["state"], "pending");
    assert!(out["error"].as_str().unwrap().contains("502"), "{out}");
    assert_eq!(env.pulls().len(), 1, "the forge stored the pull request");

    let (ok, out, err) = env.publish("feat/lost").await;
    assert!(ok, "{err} {out}");
    assert_eq!(out["states"]["pull_request"]["state"], "opened");
    assert_eq!(out["states"]["pull_request"]["number"], 1);
    assert!(out.get("error").is_none());
    assert_eq!(env.pulls().len(), 1);
    assert_eq!(env.requests("POST", "/pulls"), 1);
    assert_eq!(env.requests("POST", "/git/refs"), 1);
    assert_eq!(
        kinds(&env.events()),
        [
            "publication_requested",
            "publication_branch_created",
            "publication_pr_opened"
        ]
    );
}

#[tokio::test]
async fn refused_and_taken_branches_are_reported_without_touching_credentials() {
    let env = Env::start().await;
    env.fake
        .with(|s| s.protected_branches.insert("feat/protected".into(), 403));
    let (ok, out, err) = env.publish("feat/protected").await;
    assert!(!ok);
    assert!(err.contains("rejected the branch"), "{err}");
    assert_eq!(out["states"]["branch"]["state"], "rejected");
    assert!(
        out["states"]["branch"]["message"]
            .as_str()
            .unwrap()
            .contains("restricted"),
        "{out}"
    );
    assert_eq!(out["states"]["pull_request"]["state"], "pending");
    assert!(env.branch_tip("feat/protected").is_none());
    assert!(env.pulls().is_empty());
    assert_eq!(env.connection_status(), "enabled");
    let id = out["id"].as_str().unwrap();
    let (status, shown) = env
        .api(
            Method::GET,
            &format!("/v1/hub/repos/acme%2Fwidgets/publications/{id}"),
            &env.token,
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(shown["states"]["branch"]["state"], "rejected");
    let rejected: Vec<_> = env
        .events()
        .into_iter()
        .filter(|(k, _)| k == "publication_branch_rejected")
        .collect();
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].1["status"], 403);

    // Trying again against the same refusal is not news: still one event.
    let (ok, again, _) = env.publish("feat/protected").await;
    assert!(!ok);
    assert_eq!(again["states"]["branch"]["state"], "rejected");
    let rejections = env
        .events()
        .into_iter()
        .filter(|(k, _)| k == "publication_branch_rejected")
        .count();
    assert_eq!(rejections, 1);

    // A retry (the rule is lifted) goes through: rejected is not final.
    env.fake.with(|s| s.protected_branches.clear());
    let (ok, out, err) = env.publish("feat/protected").await;
    assert!(ok, "{err} {out}");
    assert_eq!(out["states"]["branch"]["state"], "created");

    // A branch that already exists at another commit is never moved.
    bare_git(
        &env.bare,
        &["update-ref", "refs/heads/feat/taken", &env.seed],
        &[],
        None,
    )
    .unwrap();
    let (ok, out, _) = env.publish("feat/taken").await;
    assert!(!ok);
    assert_eq!(out["states"]["branch"]["state"], "rejected");
    assert!(
        out["states"]["branch"]["message"]
            .as_str()
            .unwrap()
            .contains("another commit")
    );
    assert_eq!(env.branch_tip("feat/taken"), Some(env.seed.clone()));
    assert_eq!(
        env.requests("POST", "/git/refs"),
        3,
        "no create attempt on the taken branch"
    );
    assert_eq!(env.connection_status(), "enabled");
    assert_eq!(
        env.pulls().len(),
        1,
        "only the retried publication opened one"
    );
}

#[tokio::test]
async fn a_commit_the_forge_builds_differently_is_never_referenced() {
    let env = Env::start().await;
    env.fake.with(|s| s.alter_commit_messages = true);
    let (ok, out, err) = env.publish("feat/mismatch").await;
    assert!(!ok);
    assert!(err.contains("publication incomplete"), "{err}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("projection_mismatch"),
        "{out}"
    );
    assert_eq!(out["states"]["branch"]["state"], "pending");
    assert!(env.branch_tip("feat/mismatch").is_none());
    assert_eq!(env.requests("POST", "/git/refs"), 0);
    assert!(env.pulls().is_empty());

    // Once the forge builds the expected commit, the same publication completes.
    env.fake.with(|s| s.alter_commit_messages = false);
    let (ok, out, err) = env.publish("feat/mismatch").await;
    assert!(ok, "{err} {out}");
    assert_eq!(env.branch_tip("feat/mismatch"), Some(env.commit.clone()));
}

fn manifest(blob_sha256: &str, branch: &str) -> Value {
    let commit = "b".repeat(40);
    json!({
        "publication_id": derive_id(REPO, 1, &commit, branch),
        "revision": "a".repeat(64), "event_seq": 1, "commit": commit,
        "tree": "c".repeat(40), "parent": "d".repeat(40), "branch": branch, "base": "main",
        "title": "t", "body": "", "message": "msg\n",
        "author": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "committer": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "changes": [{"path": "a.txt", "mode": "100644", "blob": "e".repeat(40),
            "sha256": blob_sha256, "size": 5}]
    })
}

const BASE: &str = "/v1/hub/repos/acme%2Fwidgets/publications";

fn hello() -> String {
    Sha256::digest(b"hello")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn the_api_refuses_incomplete_oversized_and_foreign_requests() {
    let env = Env::start().await;
    let sha256 = hello();
    let id = derive_id(REPO, 1, &"b".repeat(40), "feat/api");
    let base = BASE;
    let (status, created) = env
        .api(
            Method::POST,
            base,
            &env.token,
            Some(manifest(&sha256, "feat/api")),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["id"], id.as_str());
    assert_eq!(created["missing_blobs"], json!([sha256]));
    // Idempotent for the same manifest, a conflict for a different one under the same id.
    let (status, again) = env
        .api(
            Method::POST,
            base,
            &env.token,
            Some(manifest(&sha256, "feat/api")),
        )
        .await;
    assert_eq!((status, &again["missing_blobs"]), (200, &json!([sha256])));
    let mut other = manifest(&sha256, "feat/api");
    other["title"] = json!("another title");
    assert_eq!(
        env.api(Method::POST, base, &env.token, Some(other)).await.0,
        409
    );

    // Bad shapes are 400, nothing stored: a path outside the repository, an oversize file, an id
    // that is not the derived one, one file hash for two git blobs, one git blob for two hashes.
    let mut bad = manifest(&sha256, "feat/api");
    bad["changes"][0]["path"] = json!("../escape");
    let mut huge = manifest(&sha256, "feat/api");
    huge["changes"][0]["size"] = json!(8 * 1024 * 1024 + 1);
    let mut foreign_id = manifest(&sha256, "feat/api");
    foreign_id["publication_id"] = json!("pub_00000000000000000000000000000001");
    let extra = |sha: &str, blob: &str| {
        let mut m = manifest(&sha256, "feat/dup");
        m["changes"].as_array_mut().unwrap().push(
            json!({"path": "b.txt", "mode": "100644", "blob": blob, "sha256": sha, "size": 5}),
        );
        m
    };
    for refused in [
        bad,
        huge,
        foreign_id,
        extra(&sha256, &"f".repeat(40)),
        extra(&"9".repeat(64), &"e".repeat(40)),
    ] {
        assert_eq!(
            env.api(Method::POST, base, &env.token, Some(refused))
                .await
                .0,
            400
        );
    }

    // Running without the blobs is refused.
    let run = format!("{base}/{id}/run");
    let (status, refused) = env
        .api(Method::POST, &run, &env.token, Some(json!({})))
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (409, Some("blobs_missing"))
    );

    // Only the creating token's owner may upload, run or create again; others see nothing (404),
    // though anyone with a token for the repository may read.
    let bob = env
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: env.fake.base.trim_start_matches("http://").to_owned(),
                user_id: 6,
            },
            "bob",
        )
        .unwrap();
    let (_, bob_secret) = mint_agent_token(&*env.store, &bob.id, "agent-b", REPO, 3600).unwrap();
    let bob_token = bob_secret.expose().to_owned();
    let bob_put = env
        .http
        .put(format!("{}{base}/{id}/blobs/{sha256}", env.url))
        .bearer_auth(&bob_token)
        .body("hello");
    assert_eq!(bob_put.send().await.unwrap().status().as_u16(), 404);
    assert_eq!(
        env.api(Method::POST, &run, &bob_token, Some(json!({})))
            .await
            .0,
        404
    );
    let again = Some(manifest(&sha256, "feat/api"));
    assert_eq!(env.api(Method::POST, base, &bob_token, again).await.0, 404);
    assert_eq!(
        env.api(Method::GET, &format!("{base}/{id}"), &bob_token, None)
            .await
            .0,
        200
    );

    // Uploads are exact: wrong bytes are 400, an unlisted blob 404, anything over the cap 413,
    // and none of them store anything.
    // (A new token of the same owner is enough, so a lost or expired token does not strand it.)
    let (_, renewed) = mint_agent_token(&*env.store, &env.owner, "agent-a2", REPO, 3600).unwrap();
    let renewed = renewed.expose().to_owned();
    let put = |sha: String, bytes: Vec<u8>| {
        let url = format!("{}{base}/{id}/blobs/{sha}", env.url);
        let request = env.http.put(url).bearer_auth(&renewed).body(bytes);
        async move { request.send().await.unwrap().status().as_u16() }
    };
    assert_eq!(put(sha256.clone(), b"hellO".to_vec()).await, 400);
    assert_eq!(put(sha256.clone(), b"hello world".to_vec()).await, 400);
    assert_eq!(put("0".repeat(64), b"hello".to_vec()).await, 404);
    assert_eq!(put(sha256.clone(), vec![7; 8 * 1024 * 1024 + 1]).await, 413);
    let (_, still) = env
        .api(
            Method::POST,
            base,
            &env.token,
            Some(manifest(&sha256, "feat/api")),
        )
        .await;
    assert_eq!(still["missing_blobs"], json!([sha256]));
    assert_eq!(put(sha256.clone(), b"hello".to_vec()).await, 204);
    let (_, done) = env
        .api(
            Method::POST,
            base,
            &env.token,
            Some(manifest(&sha256, "feat/api")),
        )
        .await;
    assert_eq!(done["missing_blobs"], json!([]));

    // With everything uploaded the forge steps run; this base commit is not on the fake.
    let (status, ran) = env.api(Method::POST, &run, &renewed, Some(json!({}))).await;
    assert_eq!(status, 200, "{ran}");
    assert!(
        ran["error"].as_str().unwrap().contains("base commit"),
        "{ran}"
    );
    assert_eq!(env.requests("POST", "/git/refs"), 0);

    // Another repository's token sees nothing (404), and an unconnected repository cannot publish.
    let (_, other_secret) =
        mint_agent_token(&*env.store, &env.owner, "agent-c", "acme/other", 3600).unwrap();
    let path = format!("{base}/{id}");
    assert_eq!(
        env.api(Method::GET, &path, other_secret.expose(), None)
            .await
            .0,
        404
    );
    assert_eq!(
        env.api(Method::GET, &path, "not-a-token", None).await.0,
        401
    );
    let mut elsewhere = manifest(&sha256, "feat/api");
    elsewhere["publication_id"] = json!(derive_id("acme/other", 1, &"b".repeat(40), "feat/api"));
    let (status, refused) = env
        .api(
            Method::POST,
            "/v1/hub/repos/acme%2Fother/publications",
            other_secret.expose(),
            Some(elsewhere),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (409, Some("not_connected"))
    );

    // Without the outbound opt-in nothing is accepted.
    env.store
        .set_connection_outbound(&env.connection, false, "test")
        .unwrap();
    let second = manifest(&sha256, "feat/api2");
    let (status, refused) = env.api(Method::POST, base, &env.token, Some(second)).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (409, Some("outbound_disabled"))
    );
    let (status, refused) = env
        .api(Method::POST, &run, &env.token, Some(json!({})))
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (409, Some("outbound_disabled"))
    );
    assert_eq!(env.connection_status(), "enabled");
}

#[tokio::test]
async fn a_token_has_a_limited_number_of_open_publications() {
    let env = Env::start().await;
    let sha256 = hello();
    for n in 0..20 {
        let manifest = manifest(&sha256, &format!("feat/c{n}"));
        assert_eq!(
            env.api(Method::POST, BASE, &env.token, Some(manifest))
                .await
                .0,
            200
        );
    }
    let over = manifest(&sha256, "feat/over");
    let (status, refused) = env.api(Method::POST, BASE, &env.token, Some(over)).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (429, Some("too_many_publications"))
    );
    // A repeat of an existing one is not a new publication.
    let first = manifest(&sha256, "feat/c0");
    assert_eq!(
        env.api(Method::POST, BASE, &env.token, Some(first)).await.0,
        200
    );
}

#[tokio::test]
async fn only_an_open_matching_pull_request_is_adopted_and_a_crowd_is_not_guessed_at() {
    let env = Env::start().await;
    // Pull requests from the branch that must not be adopted: closed, other base, other commit.
    env.fake.with(|s| {
        let repo = s.repos.get_mut(&REPO_ID).unwrap();
        let mut closed = Pull::new(1, "feat/adopt", &[]);
        closed.state = "closed".into();
        closed.head_sha = env.commit.clone();
        let mut elsewhere = Pull::new(2, "feat/adopt", &[]);
        elsewhere.base_ref = "release".into();
        elsewhere.head_sha = env.commit.clone();
        let stale = Pull::new(3, "feat/adopt", &[]);
        for pull in [closed, elsewhere, stale] {
            repo.pulls.insert(pull.number, pull);
        }
    });
    let (ok, out, err) = env.publish("feat/adopt").await;
    assert!(ok, "{err} {out}");
    assert_eq!(out["states"]["pull_request"]["number"], 4);
    assert_eq!(env.requests("POST", "/pulls"), 1);
    assert_eq!(env.pulls()[3].base_ref, "main");

    // More open pull requests from the branch than can be searched: no guess, no duplicate.
    env.fake.with(|s| {
        let repo = s.repos.get_mut(&REPO_ID).unwrap();
        for number in 10..=310 {
            repo.pulls
                .insert(number, Pull::new(number, "feat/crowd", &[]));
        }
    });
    let (ok, out, _) = env.publish("feat/crowd").await;
    assert!(!ok);
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("too many open pull requests"),
        "{out}"
    );
    assert_eq!(out["states"]["pull_request"]["state"], "pending");
    assert_eq!(env.requests("POST", "/pulls"), 1);
}

#[tokio::test]
async fn a_branch_is_adopted_only_after_this_publication_tried_to_create_it() {
    let env = Env::start().await;
    let (ok, _, err) = env.publish("feat/first").await;
    assert!(ok, "{err}");
    // Someone else puts the same commit on another branch before this publication creates it.
    let tip = &env.commit;
    bare_git(
        &env.bare,
        &["update-ref", "refs/heads/feat/second", tip],
        &[],
        None,
    )
    .unwrap();
    let (ok, out, _) = env.publish("feat/second").await;
    assert!(!ok);
    assert_eq!(out["states"]["branch"]["state"], "rejected");
    assert_eq!(out["states"]["branch"]["message"], "branch already exists");
    assert_eq!(
        env.requests("POST", "/git/refs"),
        1,
        "no create for the second branch"
    );
    assert_eq!(env.pulls().len(), 1);

    // A create whose reply was lost is recognised: the attempt was recorded first.
    env.fake.with(|s| s.lose_next_ref_reply = true);
    let (ok, out, err) = env.publish("feat/lost-ref").await;
    assert!(!ok);
    assert!(err.contains("publication incomplete"), "{err}");
    assert_eq!(out["states"]["branch"]["state"], "pending");
    assert_eq!(env.branch_tip("feat/lost-ref"), Some(env.commit.clone()));
    let (ok, out, err) = env.publish("feat/lost-ref").await;
    assert!(ok, "{err} {out}");
    assert_eq!(out["states"]["branch"]["state"], "created");
    assert_eq!(out["states"]["pull_request"]["state"], "opened");
    assert_eq!(env.requests("POST", "/git/refs"), 2);
    assert_eq!(env.pulls().len(), 2);
}

#[tokio::test]
async fn a_connection_switched_off_or_failed_gets_no_forge_writes() {
    let env = Env::start().await;
    let status = |value: &str, detail: Option<&str>| {
        env.store
            .set_connection_status(&env.connection, value, detail)
            .unwrap()
    };
    status("auth_failed", Some("401 from app"));
    let (ok, out, _) = env.publish("feat/blocked").await;
    assert!(!ok);
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("credentials failed"),
        "{out}"
    );
    assert_eq!(env.writes(), 0);
    status("enabled", None);
    env.store
        .set_connection_outbound(&env.connection, false, "test")
        .unwrap();
    let (ok, _, err) = env.publish("feat/blocked").await;
    assert!(!ok);
    assert!(err.contains("outbound_disabled"), "{err}");
    assert_eq!(env.writes(), 0);
    env.store
        .set_connection_outbound(&env.connection, true, "test")
        .unwrap();
    let (ok, out, err) = env.publish("feat/blocked").await;
    assert!(ok, "{err} {out}");
}

#[tokio::test]
async fn showing_a_publication_reads_the_forge_rarely_and_a_403_does_not_fail_the_connection() {
    let env = Env::start().await;
    let (ok, out, err) = env.publish("feat/show").await;
    assert!(ok, "{err}");
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/pulls/1".into(),
            status: 403,
            headers: vec![],
        });
    });
    let path = format!("{BASE}/{}", out["id"].as_str().unwrap());
    for _ in 0..3 {
        let (status, shown) = env.api(Method::GET, &path, &env.token, None).await;
        assert_eq!(status, 200);
        assert_eq!(shown["states"]["pull_request"]["state"], "opened");
    }
    assert_eq!(env.requests("GET", "/pulls/1"), 1, "one read per minute");
    assert_eq!(env.connection_status(), "enabled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repository_on_another_forge_is_not_published() {
    let env = Env::start().await;
    let host = "gitlab.example";
    let conn = env
        .store
        .insert_connection(
            &agentscm::hub::connection::NewConnection {
                id: format!("con_{}", "c".repeat(32)),
                provider: "gitlab".into(),
                host: host.into(),
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: Some("gitlab-bot".into()),
            },
            "test",
        )
        .unwrap();
    env.store
        .upsert_repository(&conn.id, host, 7, "group/widgets", None, None)
        .unwrap();
    env.store
        .set_connection_outbound(&conn.id, true, "test")
        .unwrap();
    let carol = env
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "gitlab".into(),
                host: host.into(),
                user_id: 9,
            },
            "carol",
        )
        .unwrap();
    let (_, secret, _) = store::mint_bound_agent_token(
        &*env.store,
        &carol.id,
        "agent-c",
        "group/widgets",
        7,
        3600,
        false,
    )
    .unwrap();
    let before = env.fake.with(|state| state.requests.len());
    let mut body = manifest(&hello(), "feat/gl");
    body["publication_id"] = json!(derive_id("group/widgets", 1, &"b".repeat(40), "feat/gl"));
    let (status, refused) = env
        .api(
            Method::POST,
            "/v1/hub/repos/group%2Fwidgets/publications",
            secret.expose(),
            Some(body),
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (409, Some("unsupported_provider"))
    );
    assert_eq!(env.fake.with(|state| state.requests.len()), before);
}

impl Env {
    async fn attached_cli(&self, args: &[&str]) -> (bool, Value, String) {
        let args: Vec<String> = args.iter().map(|s| (*s).into()).collect();
        let root = self.root.path().to_owned();
        let url = self.url.clone();
        let token = self.token_file.clone();
        let output = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_agentscm"))
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &root)
                .env("AGENTSCM_HUB_URL", url)
                .env("AGENTSCM_HUB_TOKEN_FILE", token)
                .arg("--data-dir")
                .arg(root.join("attached-data"))
                .args(args)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        (
            output.status.success(),
            value,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
    async fn attached_ok(&self, args: &[&str]) -> Value {
        let (ok, value, error) = self.attached_cli(args).await;
        assert!(ok, "{args:?}: {error} {value}");
        value
    }
    async fn attached_context(&self, state: &Value, version: u64, expected: &[u8]) -> Value {
        use agentscm::checks::{CheckLimits, SourcePathExpectation, SourcePathsRecipe};
        let recipe = SourcePathsRecipe {
            version: 1,
            expectations: vec![SourcePathExpectation {
                path: "old".into(),
                sha256: agentscm::store::digest(expected),
            }],
            limits: CheckLimits {
                max_paths: 8,
                max_total_bytes: 4096,
                max_output_bytes: 4096,
                timeout_ms: 30000,
            },
        };
        let identity = json!({"state":"digest","sha256":recipe.digest().unwrap()});
        let repository = Authority::open_read_only(self.root.path().join("attached-data"))
            .unwrap()
            .repository_id()
            .unwrap();
        let context: ReviewContext = serde_json::from_value(json!({"version":1,"change":{"repository":repository,"id":"attached-change"},"change_version":version,"revision":state["candidate_revision"],"base":null,"target":"main","policy_epoch":0,"intent":"synthetic source-path qualification","acceptance_criteria":["exact expected bytes"],"scope":["old"],"exclusions":[],"omissions":[],"dependencies":[],"recipe":identity,"toolchain":{"state":"digest","sha256":"a".repeat(64)},"environment":{"state":"digest","sha256":"b".repeat(64)},"required_evidence":[{"identity":agentscm::checks::SOURCE_PATHS_EVIDENCE,"digest":identity}],"required_capabilities":["agentscm.review-context/v1",agentscm::checks::SOURCE_PATHS_CAPABILITY]})).unwrap();
        let path = self.root.path().join("context.json");
        fs::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
        if version == 1 {
            self.attached_ok(&["start", path.to_str().unwrap()]).await;
        } else {
            self.attached_ok(&[
                "context",
                &(version - 1).to_string(),
                path.to_str().unwrap(),
            ])
            .await;
        }
        let bound = self
            .attached_ok(&[
                "attached-worktree",
                "bind-context",
                "--id",
                state["id"].as_str().unwrap(),
                "--expected-generation",
                &state["generation"].to_string(),
                "--change",
                "attached-change",
                "--operation-id",
                &format!("bind-{version}"),
            ])
            .await;
        let request = json!({"attempt_id":format!("check-{version}"),"change":context.change,"change_version":version,"context_digest":context.digest().unwrap(),"revision":context.revision,"recipe":recipe});
        fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
        self.attached_ok(&["check-source-paths", path.to_str().unwrap()])
            .await;
        let checked = self
            .attached_ok(&["check-run", &format!("check-{version}"), "0"])
            .await;
        json!({"attachment":bound["attachment"],"attempt":checked["attempt"]})
    }
}

#[tokio::test]
async fn attached_joined_https_candidate_uses_existing_hub_and_continues_same_pr() {
    let transport = HttpsFixture::new(false).repository(REPO);
    let env = Env::start_with_transport(Some(&transport)).await;
    let data = env.root.path().join("attached-data");
    drop(Engine::open(&data).unwrap());
    drop(Authority::open(&data).unwrap());
    let worktree = env.root.path().join("seed");
    run(&worktree, &["remote", "add", "origin", &transport.url]);
    run(
        &worktree,
        &["update-ref", "refs/remotes/origin/main", &env.seed],
    );
    run(&worktree, &["config", "core.excludesFile", ""]);
    fs::write(worktree.join(".git/info/exclude"), "secret\n").unwrap();
    fs::write(worktree.join("secret"), "ignored synthetic bytes").unwrap();
    let index = fs::read(worktree.join(".git/index")).unwrap();
    let attached = env
        .attached_ok(&[
            "attached-worktree",
            "attach",
            "--repository",
            worktree.to_str().unwrap(),
            "--actor",
            "alice",
            "--target",
            "main",
            "--output",
            "agents/attached",
            "--source",
            "worktree",
            "--path",
            ".",
            "--operation-id",
            "attach",
        ])
        .await;
    let mut state = attached["attachment"].clone();
    let id = state["id"].as_str().unwrap().to_owned();
    let failed = env.attached_context(&state, 1, b"fixed source\n").await;
    assert_ne!(failed["attempt"]["state"], "success");
    state = failed["attachment"].clone();
    let mirror = env.root.path().join("attached.git");
    let prepare = |generation: u64, operation: &str| {
        vec![
            "attached-worktree".to_owned(),
            "prepare".into(),
            "--id".into(),
            id.clone(),
            "--expected-generation".into(),
            generation.to_string(),
            "--mirror".into(),
            mirror.to_string_lossy().into_owned(),
            "--operation-id".into(),
            operation.into(),
            "--name".into(),
            "Synthetic".into(),
            "--email".into(),
            "synthetic@example.invalid".into(),
            "--timestamp".into(),
            "1780000000".into(),
            "--message".into(),
            "Reviewed candidate".into(),
        ]
    };
    let args = prepare(state["generation"].as_u64().unwrap(), "prepare-failed");
    assert!(
        !env.attached_cli(&args.iter().map(String::as_str).collect::<Vec<_>>())
            .await
            .0
    );
    fs::write(worktree.join("old"), b"fixed source\n").unwrap();
    fs::write(worktree.join("run"), b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(worktree.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("old", worktree.join("link")).unwrap();
    fs::remove_file(worktree.join("second")).unwrap();
    let preview = env
        .attached_ok(&[
            "attached-worktree",
            "preview",
            "--id",
            &id,
            "--expected-generation",
            &state["generation"].to_string(),
        ])
        .await;
    assert!(
        preview["preview"]["changed_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "old")
    );
    let capture = env
        .attached_ok(&[
            "attached-worktree",
            "capture",
            "--id",
            &id,
            "--expected-generation",
            &state["generation"].to_string(),
            "--operation-id",
            "capture-fixed",
        ])
        .await;
    state = capture["capture"]["attachment"].clone();
    let checked = env.attached_context(&state, 2, b"fixed source\n").await;
    assert_eq!(checked["attempt"]["state"], "success");
    state = checked["attachment"].clone();
    let args = prepare(state["generation"].as_u64().unwrap(), "prepare-fixed");
    let refs: Vec<_> = args.iter().map(String::as_str).collect();
    let prepared = env.attached_ok(&refs).await;
    assert_eq!(prepared["ok"], true, "{prepared}");
    let candidate = prepared["candidate"]["request"]["subject"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let commit = prepared["candidate"]["commit"].as_str().unwrap().to_owned();
    let duplicate = env.attached_ok(&refs).await;
    assert_eq!(duplicate["candidate"], prepared["candidate"]);
    assert_eq!(duplicate["qualification"], "not_checked");
    let noop_args = prepare(state["generation"].as_u64().unwrap(), "prepare-noop");
    let noop = env
        .attached_ok(&noop_args.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
    assert_eq!(noop["candidate"], prepared["candidate"]);
    let db = rusqlite::Connection::open(mirror.join("agentscm-export.sqlite")).unwrap();
    let operations: i64 = db
        .query_row("SELECT count(*) FROM candidate_operations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(operations, 1, "no-op preparation must not consume IDs");
    drop(db);
    let publish = |candidate: &str, generation: u64, operation: &str, expected: Option<&str>| {
        let mut args = vec![
            "attached-worktree".to_owned(),
            "publish-branch".into(),
            "--id".into(),
            id.clone(),
            "--expected-generation".into(),
            generation.to_string(),
            "--mirror".into(),
            mirror.to_string_lossy().into_owned(),
            "--candidate".into(),
            candidate.into(),
            "--operation-id".into(),
            operation.into(),
            "--credential-file".into(),
            transport.credential.to_string_lossy().into_owned(),
            "--ca-file".into(),
            transport.ca.to_string_lossy().into_owned(),
        ];
        if let Some(expected) = expected {
            args.extend(["--expected-oid".into(), expected.into()]);
        }
        args
    };
    let args = publish(
        &candidate,
        state["generation"].as_u64().unwrap(),
        "push-1",
        None,
    );
    let refs: Vec<_> = args.iter().map(String::as_str).collect();
    fs::write(worktree.join("old"), b"stale before dispatch").unwrap();
    let before_requests = transport.requests().len();
    assert!(!env.attached_cli(&refs).await.0);
    assert_eq!(transport.requests().len(), before_requests);
    fs::write(worktree.join("old"), b"fixed source\n").unwrap();
    transport.option(
        "before_push_write",
        json!({"path":worktree.join("old"),"text":"concurrent edit"}),
    );
    transport.option("lose_push_reply", json!(true));
    let unknown = env.attached_ok(&refs).await;
    assert_eq!(unknown["record"]["state"], "unknown", "{unknown}");
    assert_eq!(unknown["qualification"]["state"], "stale_or_unknown");
    transport.option("before_push_write", Value::Null);
    fs::write(worktree.join("old"), b"fixed source\n").unwrap();
    let count = transport
        .requests()
        .iter()
        .filter(|r| {
            r["method"] == "POST" && r["path"].as_str().unwrap().ends_with("git-receive-pack")
        })
        .count();
    env.attached_ok(&refs).await;
    assert_eq!(
        count,
        transport
            .requests()
            .iter()
            .filter(|r| r["method"] == "POST"
                && r["path"].as_str().unwrap().ends_with("git-receive-pack"))
            .count()
    );
    transport.option("lose_push_reply", json!(false));
    let recovered = env
        .attached_ok(&[
            "attached-worktree",
            "recover-branch",
            "--id",
            &id,
            "--mirror",
            mirror.to_str().unwrap(),
            "--operation-id",
            "push-1",
            "--credential-file",
            transport.credential.to_str().unwrap(),
            "--ca-file",
            transport.ca.to_str().unwrap(),
        ])
        .await;
    assert_eq!(recovered["record"]["state"], "observed_desired");
    let pr_args = |candidate: &str, generation: u64, operation: &str, previous: Option<&str>| {
        let mut args = publish(candidate, generation, operation, None);
        args[1] = "publish-pr".into();
        args.extend(["--title".into(), "Synthetic attached change".into()]);
        if let Some(previous) = previous {
            args.extend(["--previous-publication".into(), previous.into()]);
        }
        args
    };
    let args = pr_args(
        &candidate,
        state["generation"].as_u64().unwrap(),
        "push-1",
        None,
    );
    let refs: Vec<_> = args.iter().map(String::as_str).collect();
    env.fake.with(|s| s.lose_next_pull_reply = true);
    let lost = env.attached_ok(&refs).await;
    assert_eq!(lost["ok"], false, "{lost}");
    let recovered_pr = env
        .attached_ok(&[
            "attached-worktree",
            "recover-pr",
            "--id",
            &id,
            "--publication",
            lost["publication"]["id"].as_str().unwrap(),
        ])
        .await;
    assert_eq!(
        recovered_pr["publication"]["states"]["pull_request"]["number"],
        1
    );
    let opened = env.attached_ok(&refs).await;
    assert_eq!(
        opened["ok"], false,
        "historical replay must not become a current pass: {opened}"
    );
    assert_eq!(opened["publication"]["states"]["native_landed"], false);
    assert_eq!(opened["publication"]["states"]["pull_request"]["number"], 1);
    env.attached_ok(&refs).await;
    assert_eq!(env.requests("POST", "/pulls"), 1);
    assert_eq!(
        env.requests("POST", "/git/refs"),
        0,
        "Mirror owns the branch push"
    );
    let tree = run(
        &transport.remote,
        &["ls-tree", "-r", "refs/heads/agents/attached"],
    );
    assert!(
        tree.lines()
            .any(|l| l.starts_with("100755 ") && l.ends_with("\trun"))
    );
    assert!(
        tree.lines()
            .any(|l| l.starts_with("120000 ") && l.ends_with("\tlink"))
    );
    assert!(
        !tree
            .lines()
            .any(|l| l.ends_with("\tsecond") || l.ends_with("\tsecret"))
    );
    assert_eq!(fs::read(worktree.join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read(worktree.join("secret")).unwrap(),
        b"ignored synthetic bytes"
    );
    // An explicit external commit and target advance retain the Change and output ancestry.
    run(&worktree, &["add", "old"]);
    run(&worktree, &["commit", "-m", "external source commit"]);
    let captured = env
        .attached_ok(&[
            "attached-worktree",
            "capture",
            "--id",
            &id,
            "--expected-generation",
            &state["generation"].to_string(),
            "--operation-id",
            "external",
        ])
        .await;
    state = captured["capture"]["attachment"].clone();
    let target = env.root.path().join("target");
    fs::create_dir(&target).unwrap();
    run(&target, &["init"]);
    run(
        &target,
        &[
            "fetch",
            transport.remote.to_str().unwrap(),
            "refs/heads/main",
        ],
    );
    run(&target, &["checkout", "-b", "main", "FETCH_HEAD"]);
    fs::write(target.join("incoming"), "target advance").unwrap();
    run(&target, &["add", "incoming"]);
    run(
        &target,
        &[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=s@example.invalid",
            "commit",
            "-m",
            "target",
        ],
    );
    let advanced = run(&target, &["rev-parse", "HEAD"]);
    run(
        &transport.remote,
        &["fetch", target.to_str().unwrap(), "main:refs/heads/main"],
    );
    run(
        &worktree,
        &[
            "fetch",
            target.to_str().unwrap(),
            "main:refs/remotes/origin/main",
        ],
    );
    let refreshed = env
        .attached_ok(&[
            "attached-worktree",
            "refresh",
            "--id",
            &id,
            "--expected-generation",
            &state["generation"].to_string(),
            "--operation-id",
            "refresh",
        ])
        .await;
    state = refreshed["attachment"].clone();
    let checked = env.attached_context(&state, 3, b"fixed source\n").await;
    state = checked["attachment"].clone();
    let args = prepare(state["generation"].as_u64().unwrap(), "prepare-next");
    let next = env
        .attached_ok(&args.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
    assert_eq!(next["ok"], true, "{next}");
    let next_id = next["candidate"]["request"]["subject"]["id"]
        .as_str()
        .unwrap();
    let next_commit = next["candidate"]["commit"].as_str().unwrap();
    run(
        &mirror,
        &["merge-base", "--is-ancestor", &advanced, next_commit],
    );
    run(
        &mirror,
        &["merge-base", "--is-ancestor", &commit, next_commit],
    );
    let args = publish(
        next_id,
        state["generation"].as_u64().unwrap(),
        "push-2",
        Some(&commit),
    );
    let pushed = env
        .attached_ok(&args.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
    assert_eq!(pushed["ok"], true, "{pushed}");
    // The fixture's PR observations are scripted; update its observed head to
    // the commit actually received by the real Git HTTPS transport.
    env.fake.with(|s| {
        s.repos
            .get_mut(&REPO_ID)
            .unwrap()
            .pulls
            .get_mut(&1)
            .unwrap()
            .head_sha = next_commit.to_owned();
    });
    let args = pr_args(
        next_id,
        state["generation"].as_u64().unwrap(),
        "push-2",
        opened["publication"]["id"].as_str(),
    );
    let continued = env
        .attached_ok(&args.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
    assert_eq!(continued["ok"], true, "{continued}");
    assert_eq!(
        continued["publication"]["states"]["pull_request"]["number"],
        1
    );
    assert_eq!(env.requests("POST", "/pulls"), 1);
    let revoked = env.root.path().join("revoked.json");
    fs::write(
        &revoked,
        br#"{"username":"synthetic","password":"revoked"}"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&revoked, fs::Permissions::from_mode(0o600)).unwrap();
    let mut denied_args = publish(
        next_id,
        state["generation"].as_u64().unwrap(),
        "revoked",
        Some(next_commit),
    );
    let credential_index = denied_args
        .iter()
        .position(|v| v == "--credential-file")
        .unwrap()
        + 1;
    denied_args[credential_index] = revoked.to_string_lossy().into_owned();
    let denied = env
        .attached_ok(&denied_args.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
    assert_eq!(denied["record"]["state"], "unknown");
    let observed_requests = transport.requests().len();
    denied_args[credential_index] = transport.credential.to_string_lossy().into_owned();
    env.attached_ok(&denied_args.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
    assert_eq!(
        transport.requests().len(),
        observed_requests,
        "historical retry cannot replay authentication"
    );
    Authority::open(&data).unwrap().advance_policy(0).unwrap();
    assert!(
        !env.attached_cli(&[
            "attached-worktree",
            "compare",
            "--id",
            &id,
            "--expected-generation",
            &state["generation"].to_string()
        ])
        .await
        .0
    );
    assert!(
        Authority::open_read_only(&data)
            .unwrap()
            .events(0, 100)
            .unwrap()
            .is_empty()
    );
    // A corrupted candidate must not be reinterpreted as an integrated event.
    let db = rusqlite::Connection::open(env.root.path().join("hub/hub.sqlite")).unwrap();
    let publication = continued["publication"]["id"].as_str().unwrap();
    let raw: String = db
        .query_row(
            "SELECT manifest FROM hub_publications WHERE id=?1",
            [publication],
            |r| r.get(0),
        )
        .unwrap();
    let mut corrupt: Value = serde_json::from_str(&raw).unwrap();
    corrupt.as_object_mut().unwrap().remove("candidate");
    corrupt["event_seq"] = json!(17);
    db.execute(
        "UPDATE hub_publications SET event_seq=17,manifest=?1 WHERE id=?2",
        rusqlite::params![corrupt.to_string(), publication],
    )
    .unwrap();
    let before = env.requests("POST", "/pulls");
    let path = format!("/v1/hub/repos/acme%2Fwidgets/publications/{publication}/run");
    let (status, _) = env
        .api(Method::POST, &path, &env.token, Some(json!({})))
        .await;
    assert_ne!(status, 200, "corrupt subject substitution must fail closed");
    assert_eq!(env.requests("POST", "/pulls"), before);
}

fn candidate_manifest(env: &Env, branch: &str, version: u64, previous: Option<&str>) -> Value {
    let mut m = manifest(&hello(), branch);
    m.as_object_mut().unwrap().remove("event_seq");
    m["revision"] = json!(env.revision);
    m["commit"] = json!(env.commit);
    m["parent"] = json!(env.seed);
    m["changes"] = json!([{"path":"old","mode":"000000","blob":null,"sha256":null,"size":0}]);
    let endpoint = format!(
        "{}/{}.git",
        env.fake.base.replace("http://", "https://"),
        REPO
    );
    let mut subject: agentscm::candidate::CandidateSubject = serde_json::from_value(json!({
        "id":"", "attachment":"1".repeat(64), "endpoint":endpoint, "output":branch,
        "change":{"repository":"2".repeat(64),"id":"change"}, "change_version":version,
        "context_digest":"3".repeat(64), "policy_epoch":0, "revision":env.revision,
        "target":"main", "baseline_commit":env.seed, "parent_commit":env.seed,
        "source_commit":env.seed, "source_fingerprint":"4".repeat(64), "comparison_fingerprint":"5".repeat(64)
    })).unwrap();
    subject.id = subject.canonical_id().unwrap();
    m["publication_id"] = json!(agentscm::hub::publish::derive_candidate_id(
        REPO,
        &subject.id,
        &env.commit,
        branch
    ));
    m["candidate"] = json!({"subject":subject,"endpoint":endpoint,"previous_publication":previous});
    m
}

#[tokio::test]
async fn candidate_attempt_survives_unreadable_success_store_failure_and_list_lag() {
    let env = Env::start().await;
    let db = rusqlite::Connection::open(env.root.path().join("hub/hub.sqlite")).unwrap();
    for mode in 0..3 {
        let branch = format!("agents/uncertain-{mode}");
        run(
            &env.bare,
            &[
                "fetch",
                env.root.path().join("mirror.git").to_str().unwrap(),
                &format!("{}:refs/heads/{branch}", env.commit),
            ],
        );
        let m = candidate_manifest(&env, &branch, 1, None);
        let id = m["publication_id"].as_str().unwrap().to_owned();
        let (status, body) = env.api(Method::POST, BASE, &env.token, Some(m)).await;
        assert_eq!(status, 200, "{body}");
        if mode == 0 {
            env.fake.with(|s| s.malformed_next_pull_reply = true);
        }
        if mode == 1 {
            env.fake.with(|s| s.lose_next_pull_reply = true);
        }
        if mode == 2 {
            db.execute_batch("CREATE TRIGGER fail_pr_record BEFORE UPDATE OF pr_number ON hub_publications WHEN NEW.pr_number IS NOT NULL BEGIN SELECT RAISE(ABORT,'synthetic storage failure'); END;").unwrap();
        }
        let path = format!("{BASE}/{id}/run");
        env.api(Method::POST, &path, &env.token, Some(json!({})))
            .await;
        if mode == 2 {
            db.execute_batch("DROP TRIGGER fail_pr_record").unwrap();
        }
        assert_eq!(
            env.store
                .publication(REPO, &id)
                .unwrap()
                .unwrap()
                .branch_state,
            "pr_attempting"
        );
        let hidden = env
            .fake
            .with(|s| std::mem::take(&mut s.repos.get_mut(&REPO_ID).unwrap().pulls));
        let count = env.requests("POST", "/pulls");
        env.api(Method::POST, &path, &env.token, Some(json!({})))
            .await;
        assert_eq!(
            env.requests("POST", "/pulls"),
            count,
            "list lag cannot grant retry"
        );
        env.fake
            .with(|s| s.repos.get_mut(&REPO_ID).unwrap().pulls = hidden);
        // Both head and target move: the immutable create marker still associates
        // the historical PR, while the result explicitly reports stale history.
        run(&env.bare, &["update-ref", "refs/heads/main", &env.commit]);
        run(
            &env.bare,
            &["update-ref", &format!("refs/heads/{branch}"), &env.seed],
        );
        env.fake.with(|s| {
            for pull in s.repos.get_mut(&REPO_ID).unwrap().pulls.values_mut() {
                if pull.head_ref == branch {
                    pull.head_sha = env.seed.clone();
                }
            }
        });
        let (status, recovered) = env
            .api(
                Method::POST,
                &format!("{BASE}/{id}/recover"),
                &env.token,
                Some(json!({})),
            )
            .await;
        assert_eq!(status, 200, "{recovered}");
        assert!(
            recovered["states"]["pull_request"]["number"]
                .as_i64()
                .is_some(),
            "{recovered}"
        );
        assert!(recovered["error"].as_str().unwrap().contains("historical"));
        assert_eq!(env.requests("POST", "/pulls"), count);
        run(&env.bare, &["update-ref", "refs/heads/main", &env.seed]);
    }
}

#[tokio::test]
async fn stale_unattempted_candidate_continues_prior_pr_after_supersession() {
    stale_candidate_handler(Env::start().await).await;
}

#[test]
fn stale_unattempted_candidate_continues_prior_pr_on_postgres() {
    let Some(pg) = hub_pg::PgSchema::create() else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        stale_candidate_handler(Env::start_with_database(None, Some(&pg.url)).await).await;
    });
}

async fn stale_candidate_handler(env: Env) {
    let branch = "agents/lineage";
    run(
        &env.bare,
        &[
            "fetch",
            env.root.path().join("mirror.git").to_str().unwrap(),
            &format!("{}:refs/heads/{branch}", env.commit),
        ],
    );
    let a = candidate_manifest(&env, branch, 1, None);
    let aid = a["publication_id"].as_str().unwrap().to_owned();
    assert_eq!(
        env.api(Method::POST, BASE, &env.token, Some(a)).await.0,
        200
    );
    let (_, opened) = env
        .api(
            Method::POST,
            &format!("{BASE}/{aid}/run"),
            &env.token,
            Some(json!({})),
        )
        .await;
    assert_eq!(opened["states"]["pull_request"]["number"], 1, "{opened}");
    let b = candidate_manifest(&env, branch, 2, Some(&aid));
    let bid = b["publication_id"].as_str().unwrap().to_owned();
    assert_eq!(
        env.api(Method::POST, BASE, &env.token, Some(b)).await.0,
        200
    );
    run(&env.bare, &["update-ref", "refs/heads/main", &env.commit]);
    let (_, stale) = env
        .api(
            Method::POST,
            &format!("{BASE}/{bid}/run"),
            &env.token,
            Some(json!({})),
        )
        .await;
    assert!(stale["error"].as_str().unwrap().contains("target differs"));
    let store = env.store.clone();
    let retained_id = bid.clone();
    assert!(
        tokio::task::spawn_blocking(move || store
            .publication(REPO, &retained_id)
            .unwrap()
            .unwrap()
            .pr_number
            .is_none())
        .await
        .unwrap()
    );
    let mut c = candidate_manifest(&env, branch, 3, Some(&bid));
    let mut subject: agentscm::candidate::CandidateSubject =
        serde_json::from_value(c["candidate"]["subject"].clone()).unwrap();
    subject.baseline_commit = env.commit.clone();
    subject.id = subject.canonical_id().unwrap();
    let cid = agentscm::hub::publish::derive_candidate_id(REPO, &subject.id, &env.commit, branch);
    c["candidate"]["subject"] = json!(subject);
    c["publication_id"] = json!(cid);
    let (status, body) = env.api(Method::POST, BASE, &env.token, Some(c)).await;
    assert_eq!(status, 200, "{body}");
    let (_, continued) = env
        .api(
            Method::POST,
            &format!("{BASE}/{cid}/run"),
            &env.token,
            Some(json!({})),
        )
        .await;
    assert_eq!(
        continued["states"]["pull_request"]["number"], 1,
        "{continued}"
    );
    assert_eq!(env.requests("POST", "/pulls"), 1);
}

#[tokio::test]
async fn refused_candidate_allows_manual_adoption_after_target_moves() {
    let env = Env::start().await;
    let branch = "agents/manual";
    run(
        &env.bare,
        &[
            "fetch",
            env.root.path().join("mirror.git").to_str().unwrap(),
            &format!("{}:refs/heads/{branch}", env.commit),
        ],
    );
    let m = candidate_manifest(&env, branch, 1, None);
    let id = m["publication_id"].as_str().unwrap().to_owned();
    assert_eq!(
        env.api(Method::POST, BASE, &env.token, Some(m)).await.0,
        200
    );
    env.fake.with(|s| s.refuse_next_pull = true);
    let (_, refused) = env
        .api(
            Method::POST,
            &format!("{BASE}/{id}/run"),
            &env.token,
            Some(json!({})),
        )
        .await;
    assert!(
        refused["error"].as_str().unwrap().contains("refused (422)"),
        "{refused}"
    );
    assert_eq!(
        env.store
            .publication(REPO, &id)
            .unwrap()
            .unwrap()
            .branch_state,
        "pr_refused"
    );
    env.api(
        Method::POST,
        &format!("{BASE}/{id}/run"),
        &env.token,
        Some(json!({})),
    )
    .await;
    assert_eq!(env.requests("POST", "/pulls"), 1);
    assert!(env.pulls().is_empty());
    env.fake.with(|s| {
        let mut pull = fake_github::Pull::new(42, branch, &[]);
        pull.head_sha = env.commit.clone();
        s.repos.get_mut(&REPO_ID).unwrap().pulls.insert(42, pull);
    });
    run(&env.bare, &["update-ref", "refs/heads/main", &env.commit]);
    let (_, observed) = env
        .api(
            Method::POST,
            &format!("{BASE}/{id}/recover"),
            &env.token,
            Some(json!({})),
        )
        .await;
    assert_eq!(
        observed["states"]["pull_request"]["number"], 42,
        "{observed}"
    );
    assert!(observed["error"].as_str().unwrap().contains("historical"));
    assert_eq!(env.requests("POST", "/pulls"), 1);
}

fn reidentify_candidate(m: &mut Value) {
    let mut subject: agentscm::candidate::CandidateSubject =
        serde_json::from_value(m["candidate"]["subject"].clone()).unwrap();
    subject.id = subject.canonical_id().unwrap();
    m["publication_id"] = json!(agentscm::hub::publish::derive_candidate_id(
        REPO,
        &subject.id,
        m["commit"].as_str().unwrap(),
        m["branch"].as_str().unwrap()
    ));
    m["candidate"]["subject"] = json!(subject);
}

#[tokio::test]
async fn candidate_endpoint_and_actual_default_are_checked_before_reservation() {
    let env = Env::start().await;
    let mut wrong_host = candidate_manifest(&env, "agents/wrong-host", 1, None);
    wrong_host["candidate"]["endpoint"] = json!("https://elsewhere.invalid/acme/widgets.git");
    wrong_host["candidate"]["subject"]["endpoint"] = wrong_host["candidate"]["endpoint"].clone();
    reidentify_candidate(&mut wrong_host);
    assert_eq!(
        env.api(Method::POST, BASE, &env.token, Some(wrong_host))
            .await
            .0,
        409
    );
    let mut default = candidate_manifest(&env, "main", 2, None);
    default["base"] = json!("develop");
    default["candidate"]["subject"]["target"] = json!("develop");
    reidentify_candidate(&mut default);
    assert_eq!(
        env.api(Method::POST, BASE, &env.token, Some(default))
            .await
            .0,
        409
    );
    for alias in ["HEAD", "refs/heads/main"] {
        let m = candidate_manifest(&env, alias, 3, None);
        assert_ne!(
            env.api(Method::POST, BASE, &env.token, Some(m)).await.0,
            200
        );
    }
    env.fake
        .with(|s| s.repos.get_mut(&REPO_ID).unwrap().default_branch = None);
    let unknown = candidate_manifest(&env, "agents/unknown-default", 4, None);
    assert_eq!(
        env.api(Method::POST, BASE, &env.token, Some(unknown))
            .await
            .0,
        409
    );
    let db = rusqlite::Connection::open(env.root.path().join("hub/hub.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM hub_candidate_heads", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(env.writes(), 0);
}
