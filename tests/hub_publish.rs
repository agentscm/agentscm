//! Publishing a native revision as a GitHub branch and pull request, end to end: a real
//! git-export mirror, the `agentscm hub publish` binary, an in-process hub, and a fake
//! GitHub backed by a real bare Git repository. Synthetic data only.
#![cfg(unix)]
#[path = "support/fake_github.rs"]
mod fake_github;

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
        let root = TempDir::new().unwrap();
        // The forge's base: one commit with a file the revision deletes.
        let seed_repo = root.path().join("seed");
        fs::create_dir(&seed_repo).unwrap();
        run(&seed_repo, &["init", "--object-format=sha1"]);
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
        let bare = bare_clone(root.path(), &seed_repo);
        fake.with(|s| {
            let mut repo = Repo::new(REPO_ID, REPO);
            repo.git = Some(bare.clone());
            s.repos.insert(REPO_ID, repo);
            s.pats
                .insert(PAT.into(), ("robot".into(), 77, [REPO_ID].into()));
        });

        let dir = root.path().join("hub");
        fs::create_dir(&dir).unwrap();
        let store = store::open(&format!("sqlite://{}", dir.join("hub.sqlite").display())).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let secrets = dir.join("secrets");
        sync::prepare_secrets_dir(&secrets, true).unwrap();
        let base = fake.base.clone();
        let config = SyncConfig::from_lookup(&url, secrets, 14, |name| match name {
            "AGENTSCM_GITHUB_URL" | "AGENTSCM_GITHUB_API_URL" => Some(base.clone()),
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
        let host = fake.base.trim_start_matches("http://").to_owned();
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
        let token_file = root.path().join("token");
        fs::write(&token_file, secret.expose()).unwrap();
        Self {
            fake,
            url,
            store,
            bare,
            revision: event.after.revision,
            commit: exported.record.commit.unwrap(),
            seed,
            connection,
            token: secret.expose().to_owned(),
            token_file,
            owner: owner.id,
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
