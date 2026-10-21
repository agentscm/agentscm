//! The attached-worktree candidate path against GitLab, through the real client binary: a
//! leased branch push over loopback HTTPS to a nested-group project, then a merge request
//! created by an in-process Hub through a fake GitLab backed by the same bare repository.
//! Synthetic data only.
#![cfg(unix)]
#[path = "support/fake_gitlab.rs"]
mod fake_gitlab;
#[allow(dead_code)]
#[path = "support/git_https.rs"]
mod https_fixture;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agentscm::authority::Authority;
use agentscm::backend::LocalBackend;
use agentscm::context::ReviewContext;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::{HubState, router};
use agentscm::store::Engine;
use fake_gitlab::{FakeGitlab, Project, Token};
use https_fixture::HttpsFixture;
use serde_json::{Value, json};
use tempfile::TempDir;

const NESTED: &str = "group/sub/widgets";
const REPO_ID: i64 = 100;
const TOKEN: &str = "synthetic-gitlab-token";
const GIT: &str = "/usr/bin/git";

fn git() -> PathBuf {
    PathBuf::from(GIT)
}

fn run(path: &Path, args: &[&str]) -> String {
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
    transport: HttpsFixture,
    root: TempDir,
    url: String,
    token_file: PathBuf,
}

impl Env {
    async fn start() -> Self {
        let transport = HttpsFixture::new(false).repository(NESTED);
        let root = TempDir::new().unwrap();
        let worktree = root.path().join("worktree");
        fs::create_dir(&worktree).unwrap();
        run(&worktree, &["init", "--initial-branch=main"]);
        run(&worktree, &["config", "core.excludesFile", ""]);
        fs::write(worktree.join("old"), b"old bytes").unwrap();
        run(&worktree, &["add", "."]);
        run(&worktree, &["commit", "-m", "base"]);
        let seed = run(&worktree, &["rev-parse", "HEAD"]);
        run(&worktree, &["remote", "add", "origin", &transport.url]);
        run(
            &worktree,
            &["update-ref", "refs/remotes/origin/main", &seed],
        );
        run(
            &transport.remote,
            &[
                "fetch",
                worktree.to_str().unwrap(),
                &format!("{seed}:refs/heads/main"),
            ],
        );
        let data = root.path().join("attached-data");
        drop(Engine::open(&data).unwrap());
        drop(Authority::open(&data).unwrap());

        let fake = FakeGitlab::start().await;
        let bare = transport.remote.clone();
        fake.with(|s| {
            let mut project = Project::new(REPO_ID, NESTED, "private");
            project.git = Some(bare);
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

        // The connected host is the one the attached `origin` names; the API is the fake.
        let web = transport
            .url
            .trim_end_matches(&format!("/{NESTED}.git"))
            .to_owned();
        let host = web.trim_start_matches("https://").to_owned();
        let hub_dir = root.path().join("hub");
        fs::create_dir(&hub_dir).unwrap();
        let store: Arc<dyn Store> = store::open(&format!(
            "sqlite://{}",
            hub_dir.join("hub.sqlite").display()
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let secrets = hub_dir.join("secrets");
        sync::prepare_secrets_dir(&secrets, true).unwrap();
        let api = format!("{}/api/v4", fake.base);
        let web_url = web.clone();
        let config = SyncConfig::from_lookup(&url, secrets, 14, |name| match name {
            "AGENTSCM_GITLAB_URL" => Some(web_url.clone()),
            "AGENTSCM_GITLAB_API_URL" => Some(api.clone()),
            "AGENTSCM_GITLAB_TOKEN" => Some(TOKEN.into()),
            _ => None,
        })
        .unwrap();
        let state = HubState::new(
            store.clone(),
            Arc::new(LocalBackend::open(hub_dir.join("objects")).unwrap()),
        )
        .with_sync(Sync::new(config).unwrap());
        sync::start(&state).await.unwrap();
        let app = router(state);
        tokio::spawn(async move { axum::serve(listener, app).await });
        let secret = tokio::task::spawn_blocking(move || {
            let connection = store.pat_connection(&host).unwrap().unwrap().id;
            store
                .upsert_repository(&connection, &host, REPO_ID, NESTED, None, None)
                .unwrap();
            store
                .set_connection_outbound(&connection, true, "test")
                .unwrap();
            let owner = store
                .upsert_user(
                    &ForgeIdentity {
                        provider: "gitlab".into(),
                        host,
                        user_id: 5,
                    },
                    "alice",
                )
                .unwrap();
            let (_, secret) =
                mint_agent_token(&*store, &owner.id, "agent-a", NESTED, 3600).unwrap();
            secret.expose().to_owned()
        })
        .await
        .unwrap();
        let token_file = root.path().join("token");
        fs::write(&token_file, secret).unwrap();
        Self {
            fake,
            transport,
            root,
            url,
            token_file,
        }
    }

    async fn cli(&self, args: &[&str]) -> (bool, Value, String) {
        let args: Vec<String> = args.iter().map(|s| (*s).into()).collect();
        let root = self.root.path().to_owned();
        let (url, token) = (self.url.clone(), self.token_file.clone());
        let output = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_agentscm"))
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
        (
            output.status.success(),
            serde_json::from_slice(&output.stdout).unwrap_or(Value::Null),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    async fn ok(&self, args: &[&str]) -> Value {
        let (ok, value, error) = self.cli(args).await;
        assert!(ok, "{args:?}: {error} {value}");
        value
    }

    /// Starts a review context over `old` and runs its exact-bytes check; the bound attachment.
    async fn qualify(&self, state: &Value, expected: &[u8]) -> Value {
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
        let context: ReviewContext = serde_json::from_value(json!({"version":1,"change":{"repository":repository,"id":"attached-change"},"change_version":1,"revision":state["candidate_revision"],"base":null,"target":"main","policy_epoch":0,"intent":"synthetic source-path qualification","acceptance_criteria":["exact expected bytes"],"scope":["old"],"exclusions":[],"omissions":[],"dependencies":[],"recipe":identity,"toolchain":{"state":"digest","sha256":"a".repeat(64)},"environment":{"state":"digest","sha256":"b".repeat(64)},"required_evidence":[{"identity":agentscm::checks::SOURCE_PATHS_EVIDENCE,"digest":identity}],"required_capabilities":["agentscm.review-context/v1",agentscm::checks::SOURCE_PATHS_CAPABILITY]})).unwrap();
        let path = self.root.path().join("context.json");
        fs::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
        self.ok(&["start", path.to_str().unwrap()]).await;
        let bound = self
            .ok(&[
                "attached-worktree",
                "bind-context",
                "--id",
                state["id"].as_str().unwrap(),
                "--expected-generation",
                &state["generation"].to_string(),
                "--change",
                "attached-change",
                "--operation-id",
                "bind",
            ])
            .await;
        let request = json!({"attempt_id":"check-1","change":context.change,"change_version":1,"context_digest":context.digest().unwrap(),"revision":context.revision,"recipe":recipe});
        fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
        self.ok(&["check-source-paths", path.to_str().unwrap()])
            .await;
        let checked = self.ok(&["check-run", "check-1", "0"]).await;
        assert_eq!(checked["attempt"]["state"], "success", "{checked}");
        bound["attachment"].clone()
    }

    async fn ok_args(&self, args: Vec<String>) -> Value {
        self.ok(&args.iter().map(String::as_str).collect::<Vec<_>>())
            .await
    }

    fn mrs(&self) -> Vec<fake_gitlab::Mr> {
        self.fake
            .with(|s| s.projects[&REPO_ID].mrs.values().cloned().collect())
    }
}

/// Attach, capture, qualify and prepare a candidate: (id, generation, mirror, candidate, commit).
async fn prepared_candidate(env: &Env) -> (String, String, PathBuf, String, String) {
    let worktree = env.root.path().join("worktree");
    let attached = env
        .ok(&[
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
    let id = attached["attachment"]["id"].as_str().unwrap().to_owned();
    fs::write(worktree.join("old"), b"fixed source\n").unwrap();
    let captured = env
        .ok(&[
            "attached-worktree",
            "capture",
            "--id",
            &id,
            "--expected-generation",
            &attached["attachment"]["generation"].to_string(),
            "--operation-id",
            "capture",
        ])
        .await;
    let state = env
        .qualify(&captured["capture"]["attachment"], b"fixed source\n")
        .await;
    let generation = state["generation"].to_string();
    let mirror = env.root.path().join("attached.git");
    let prepared = env
        .ok(&[
            "attached-worktree",
            "prepare",
            "--id",
            &id,
            "--expected-generation",
            &generation,
            "--mirror",
            mirror.to_str().unwrap(),
            "--operation-id",
            "prepare",
            "--name",
            "Synthetic",
            "--email",
            "synthetic@example.invalid",
            "--timestamp",
            "1780000000",
            "--message",
            "Reviewed candidate",
        ])
        .await;
    assert_eq!(prepared["ok"], true, "{prepared}");
    let candidate = prepared["candidate"]["request"]["subject"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let commit = prepared["candidate"]["commit"].as_str().unwrap().to_owned();
    (id, generation, mirror, candidate, commit)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attached_candidate_pushes_a_branch_and_adopts_a_lost_merge_request_in_a_nested_group() {
    let env = Env::start().await;
    let (id, generation, mirror, candidate, commit) = prepared_candidate(&env).await;
    let transport = &env.transport;
    let publish = |verb: &str| {
        let mut args: Vec<String> = [
            "attached-worktree",
            verb,
            "--id",
            &id,
            "--expected-generation",
            &generation,
            "--mirror",
            mirror.to_str().unwrap(),
            "--candidate",
            &candidate,
            "--operation-id",
            "push-1",
            "--credential-file",
            transport.credential.to_str().unwrap(),
            "--ca-file",
            transport.ca.to_str().unwrap(),
        ]
        .map(String::from)
        .into();
        if verb == "publish-pr" {
            args.extend(["--title".into(), "Synthetic attached change".into()]);
        }
        args
    };

    let pushed = env.ok_args(publish("publish-branch")).await;
    assert_eq!(pushed["record"]["state"], "published", "{pushed}");
    assert_eq!(
        run(
            &transport.remote,
            &["rev-parse", "refs/heads/agents/attached"]
        ),
        commit
    );
    assert_eq!(env.fake.with(|s| s.mr_creates()), 0);

    // The create reply is lost: the receipt is uncertain and nothing repeats the POST.
    env.fake.with(|s| s.lose_next_mr_reply = true);
    let lost = env.ok_args(publish("publish-pr")).await;
    assert_eq!(lost["ok"], false, "{lost}");
    let publication = lost["publication"]["id"].as_str().unwrap().to_owned();
    assert_eq!(env.fake.with(|s| s.mr_creates()), 1);
    // Even while the project list does not show it, a repeat grants no retry.
    let hidden = env
        .fake
        .with(|s| std::mem::take(&mut s.projects.get_mut(&REPO_ID).unwrap().mrs));
    let again = env.ok_args(publish("publish-pr")).await;
    assert_eq!(again["ok"], false, "{again}");
    assert!(again["publication"]["error"].is_string(), "{again}");
    assert_eq!(env.fake.with(|s| s.mr_creates()), 1, "no blind replay");
    env.fake
        .with(|s| s.projects.get_mut(&REPO_ID).unwrap().mrs = hidden);

    // Read-only recovery adopts the merge request by its marker.
    let recovered = env
        .ok(&[
            "attached-worktree",
            "recover-pr",
            "--id",
            &id,
            "--publication",
            &publication,
        ])
        .await;
    assert_eq!(
        recovered["publication"]["states"]["pull_request"]["number"], 1,
        "{recovered}"
    );
    let replay = env.ok_args(publish("publish-pr")).await;
    assert_eq!(
        replay["ok"], false,
        "historical replay is never current: {replay}"
    );
    assert_eq!(replay["publication"]["states"]["pull_request"]["number"], 1);
    let mrs = env.mrs();
    assert_eq!(mrs.len(), 1);
    assert_eq!(
        (mrs[0].source_branch.as_str(), mrs[0].target_branch.as_str()),
        ("agents/attached", "main")
    );
    assert_eq!(mrs[0].sha, commit);
    assert!(
        mrs[0]
            .description
            .contains(&format!("agentscm-candidate:{publication}:{commit}"))
    );
    assert_eq!(env.fake.with(|s| s.mr_creates()), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attached_branch_push_refused_by_a_remote_hook_is_rejected_and_blocks_the_merge_request() {
    let env = Env::start().await;
    let (id, generation, mirror, candidate, _) = prepared_candidate(&env).await;
    let hooks = env.transport.remote.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-receive");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let transport = &env.transport;
    let args = |verb: &str| -> Vec<String> {
        let mut args: Vec<String> = [
            "attached-worktree",
            verb,
            "--id",
            &id,
            "--expected-generation",
            &generation,
            "--mirror",
            mirror.to_str().unwrap(),
            "--candidate",
            &candidate,
            "--operation-id",
            "push-1",
            "--credential-file",
            transport.credential.to_str().unwrap(),
            "--ca-file",
            transport.ca.to_str().unwrap(),
        ]
        .map(String::from)
        .into();
        if verb == "publish-pr" {
            args.extend(["--title".into(), "Synthetic attached change".into()]);
        }
        args
    };
    let pushed = env.ok_args(args("publish-branch")).await;
    assert_eq!(pushed["ok"], false, "{pushed}");
    assert_eq!(pushed["record"]["state"], "rejected", "{pushed}");
    assert_eq!(pushed["record"]["rejection"], "hook_declined", "{pushed}");
    assert_eq!(
        run(&transport.remote, &["for-each-ref", "refs/heads/agents"]),
        ""
    );
    let (ok, _, _) = env
        .cli(
            &args("publish-pr")
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        )
        .await;
    assert!(!ok, "publish-pr must refuse a rejected branch receipt");
    assert_eq!(env.fake.with(|s| s.mr_creates()), 0);
}
