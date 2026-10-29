//! `local-validation reconcile` against a fake GitHub and a local bare `origin`. Synthetic keys,
//! repositories and tokens only.
#![cfg(unix)]
use crate::fake_github;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use agentscm::local_validation::{
    CHECK_KIND, RecipeCommand, Services, ValidationPolicy, ValidationRecipe,
};
use agentscm::policy::issuers::{
    CommonControl, IssuerKeyRecord, IssuerRecord, IssuerRegistrySnapshot, issuer_key_id,
};
use fake_github::{Comment, Failure, FakeGithub, Pull, Repo};

const REPOSITORY: &str = "github.com/o/r";
const TOKEN: &str = "synthetic-secret";
const MARKED: &str = "<!-- agentscm-local-validation:v1 ";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

struct Env {
    dir: tempfile::TempDir,
    fake: FakeGithub,
    head: String,
}

impl Env {
    async fn new() -> Self {
        Self::with_recipe(
            vec![
                ("a.txt", "a"),
                ("Cargo.lock", "# synthetic lock\n"),
                ("rust-toolchain.toml", "[toolchain]\n"),
            ],
            vec![RecipeCommand {
                id: "gate".into(),
                argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
                timeout_secs: 30,
                paths: None,
            }],
            None,
        )
        .await
    }

    async fn with_recipe(
        files: Vec<(&str, &str)>,
        commands: Vec<RecipeCommand>,
        dependency_files: Option<Vec<String>>,
    ) -> Self {
        Self::with_exempt(files, commands, dependency_files, None).await
    }

    /// A docs-style exemption list whose `.github/**` must still not exempt the trust inputs.
    async fn exempting(files: Vec<(&str, &str)>) -> Self {
        Self::with_exempt(
            files,
            vec![RecipeCommand {
                id: "gate".into(),
                argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
                timeout_secs: 30,
                paths: None,
            }],
            None,
            Some(
                ["*.md", "docs/**", ".github/**"]
                    .map(str::to_owned)
                    .to_vec(),
            ),
        )
        .await
    }

    async fn with_exempt(
        files: Vec<(&str, &str)>,
        commands: Vec<RecipeCommand>,
        dependency_files: Option<Vec<String>>,
        exempt_paths: Option<Vec<String>>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let work = root.join("work");
        fs::create_dir(&work).unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        fs::write(root.join("key"), [7; 32]).unwrap();
        fs::set_permissions(root.join("key"), fs::Permissions::from_mode(0o600)).unwrap();
        let public_key_hex: String = key
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let registry = IssuerRegistrySnapshot {
            version: 1,
            repository: REPOSITORY.into(),
            epoch: 1,
            issuers: vec![IssuerRecord {
                issuer_id: "maintainer".into(),
                trust_domain: "local".into(),
                common_control: CommonControl::Unknown {},
                allowed_check_kinds: vec![CHECK_KIND.into()],
                keys: vec![IssuerKeyRecord {
                    key_id: issuer_key_id(&public_key_hex).unwrap(),
                    public_key_hex,
                    not_before_ms: 0,
                    not_after_ms: 4_000_000_000_000,
                    revoked_at_ms: None,
                }],
            }],
        };
        let recipe = ValidationRecipe {
            recipe_id: "gate".into(),
            commands,
            dependency_files,
            toolchain_probes: None,
        };
        let policy = ValidationPolicy {
            repository: REPOSITORY.into(),
            recipe_sha256: recipe.sha256().unwrap(),
            required_services: Services {
                postgres: false,
                s3: false,
            },
            max_age_seconds: 3600,
            epoch: 1,
            exempt_paths,
        };
        let config = root.join("config");
        fs::create_dir(&config).unwrap();
        fs::write(
            config.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        fs::write(
            config.join("recipe.json"),
            serde_json::to_vec(&recipe).unwrap(),
        )
        .unwrap();
        fs::write(
            config.join("policy.json"),
            serde_json::to_vec(&policy).unwrap(),
        )
        .unwrap();

        git(&work, &["init", "-q", "-b", "main"]);
        fs::write(work.join("base.txt"), "base").unwrap();
        // The verifier configuration lives in the default branch, as in a real repository.
        let in_repo = work.join(".github/local-validation");
        fs::create_dir_all(&in_repo).unwrap();
        for name in ["policy.json", "recipe.json", "registry.json"] {
            fs::copy(config.join(name), in_repo.join(name)).unwrap();
        }
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "base"]);
        let base = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        for (name, text) in files {
            let path = work.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "--allow-empty", "-m", "work"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        // `origin` serves the default branch and the pull request head ref.
        let bare = root.join("origin.git");
        git(
            &root,
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(
            &work,
            &[
                "push",
                "-q",
                bare.to_str().unwrap(),
                &format!("{base}:refs/heads/main"),
                &format!("{head}:refs/pull/1/head"),
            ],
        );
        git(&root, &["clone", "-q", bare.to_str().unwrap(), "checkout"]);

        let fake = FakeGithub::start().await;
        fake.with(|s| {
            let mut repo = Repo::new(1, "o/r");
            repo.pulls.insert(
                1,
                Pull {
                    head_sha: head.clone(),
                    ..Pull::new(1, "feature", &[])
                },
            );
            s.repos.insert(1, repo);
            s.pats.insert(
                TOKEN.into(),
                ("maintainer-login".into(), 9, [1].into_iter().collect()),
            );
        });
        Self { dir, fake, head }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn cli(&self, args: &[&str]) -> Output {
        cli(&self.root(), args)
    }

    /// Signs an attestation of the work commit and returns its comment body.
    fn attest(&self) -> String {
        let root = self.root();
        let p = |n: &str| root.join(n).to_str().unwrap().to_owned();
        let config = |n: &str| root.join("config").join(n).to_str().unwrap().to_owned();
        let out = self.cli(&[
            "run",
            "--repo",
            &p("work"),
            "--recipe",
            &config("recipe.json"),
            "--policy",
            &config("policy.json"),
            "--registry",
            &config("registry.json"),
            "--key-file",
            &p("key"),
            "--issuer-id",
            "maintainer",
            "--state-dir",
            &p("state"),
        ]);
        let json: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
        assert!(out.status.success());
        let shown = self.cli(&[
            "show",
            "--state-dir",
            &p("state"),
            "--attestation",
            json["envelope_sha256"].as_str().unwrap(),
        ]);
        assert!(shown.status.success());
        String::from_utf8(shown.stdout).unwrap()
    }

    fn reconcile_args(&self) -> Vec<String> {
        let root = self.root();
        [
            "reconcile",
            "--github-repo",
            "o/r",
            "--repo",
            root.join("checkout").to_str().unwrap(),
            "--api-url",
            &self.fake.base,
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn reconcile(&self) -> Output {
        let args = self.reconcile_args();
        self.cli(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// Runs `reconcile` while the first request containing `stall` is held; `during` runs
    /// against the fake at that moment.
    async fn reconcile_stalled(&self, stall: &str, during: impl FnOnce(&FakeGithub)) -> Output {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        self.fake
            .with(|s| s.stall_next = Some((stall.into(), gate.clone())));
        let (root, args) = (self.root(), self.reconcile_args());
        let run = tokio::task::spawn_blocking(move || {
            cli(&root, &args.iter().map(String::as_str).collect::<Vec<_>>())
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while self.fake.with(|s| s.stall_next.is_some()) {
            assert!(
                std::time::Instant::now() < deadline,
                "request never arrived"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        during(&self.fake);
        gate.add_permits(1);
        run.await.unwrap()
    }

    fn attest_comment(&self, id: u64, association: &str, body: String) {
        self.fake.with(|s| {
            s.repos.get_mut(&1).unwrap().comments.insert(
                1,
                vec![Comment {
                    id,
                    body,
                    author: "maintainer-login".into(),
                    author_association: association.into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                }],
            );
        });
    }

    /// `(state, description)` of our statuses on `sha`, oldest first.
    fn statuses(&self, sha: &str) -> Vec<(String, String)> {
        self.fake.with(|s| {
            s.repos[&1]
                .statuses
                .iter()
                .filter(|(at, v)| at == sha && v["context"] == "agentscm/local-validation")
                .map(|(_, v)| {
                    (
                        v["state"].as_str().unwrap().to_owned(),
                        v["description"].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        })
    }

    fn states(&self, sha: &str) -> Vec<String> {
        self.statuses(sha).into_iter().map(|(s, _)| s).collect()
    }

    fn posts(&self) -> usize {
        self.fake
            .with(|s| s.requests.iter().filter(|r| r.method == "POST").count())
    }

    /// Commits `files` on top of the current head, publishes it as the pull request head and
    /// returns the new sha.
    fn push_head(&mut self, files: &[(&str, &str)]) -> String {
        let root = self.root();
        let work = root.join("work");
        for (name, text) in files {
            let path = work.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "more"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        let bare = root.join("origin.git");
        git(
            &work,
            &[
                "push",
                "-q",
                "-f",
                bare.to_str().unwrap(),
                &format!("{head}:refs/pull/1/head"),
            ],
        );
        self.fake.with(|s| {
            s.repos
                .get_mut(&1)
                .unwrap()
                .pulls
                .get_mut(&1)
                .unwrap()
                .head_sha = head.clone();
        });
        self.head = head.clone();
        head
    }

    fn prior_success(&self) {
        self.fake.with(|s| {
            let status = serde_json::json!({"state": "success", "context": "agentscm/local-validation", "description": "old"});
            s.repos
                .get_mut(&1)
                .unwrap()
                .record_status(&self.head, status, "bot");
        });
    }
}

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(["--no-telemetry", "--data-dir"])
        .arg(root.join("data"))
        .arg("local-validation")
        .args(args)
        .env("GH_TOKEN", TOKEN)
        .output()
        .unwrap()
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pull_request_that_never_opted_in_gets_no_status() {
    let env = Env::new().await;
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.posts(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_fetch_refuses_a_nonproduction_api_before_any_request() {
    let env = Env::new().await;
    let mut args = env.reconcile_args();
    args.push("--github-authenticated-fetch".into());
    let out = tokio::task::block_in_place(|| {
        env.cli(&args.iter().map(String::as_str).collect::<Vec<_>>())
    });
    assert_refused_before_any_request(&env, &out);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("requires --api-url https://api.github.com")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_fetch_refuses_a_noncanonical_origin_without_leaking_the_token() {
    let env = Env::new().await;
    git(
        &env.root().join("checkout"),
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com.evil/o/r.git",
        ],
    );
    let mut args = env.reconcile_args();
    let api = args.iter().position(|arg| arg == "--api-url").unwrap() + 1;
    args[api] = "https://api.github.com".into();
    args.push("--github-authenticated-fetch".into());
    let out = tokio::task::block_in_place(|| {
        env.cli(&args.iter().map(String::as_str).collect::<Vec<_>>())
    });
    assert_refused_before_any_request(&env, &out);
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!output.contains(TOKEN));
    assert!(output.contains("authenticated fetch requires the canonical GitHub origin URL"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_valid_attestation_posts_pending_then_success_on_that_head() {
    let env = Env::new().await;
    let body = tokio::task::block_in_place(|| env.attest());
    env.attest_comment(10, "OWNER", body);
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["pending", "success"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&env.head) && !stdout.contains(MARKED));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(TOKEN));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_author_association_is_ignored() {
    let env = Env::new().await;
    let body = tokio::task::block_in_place(|| env.attest());
    env.attest_comment(10, "CONTRIBUTOR", body);
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0);
    assert_eq!(env.posts(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_attestation_replaces_the_old_success_with_pending() {
    let env = Env::new().await;
    env.prior_success();
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let states = env.states(&env.head);
    assert_eq!(states.last().unwrap(), "pending");
    assert!(!states[1..].contains(&"success".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_that_removes_the_marker_replaces_the_old_success_with_pending() {
    let env = Env::new().await;
    env.prior_success();
    env.attest_comment(10, "OWNER", "edited, no attestation any more".into());
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0);
    assert_eq!(env.states(&env.head).last().unwrap(), "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_head_that_moves_before_the_verdict_is_left_pending() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    let moved = "c".repeat(40);
    let out = env
        .reconcile_stalled(&format!("/statuses/{}", env.head), |fake| {
            fake.with(|s| {
                s.repos
                    .get_mut(&1)
                    .unwrap()
                    .pulls
                    .get_mut(&1)
                    .unwrap()
                    .head_sha = moved.clone()
            })
        })
        .await;
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["pending"]);
    assert!(env.states(&moved).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pull_request_closed_before_the_verdict_gets_no_verdict() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    let out = env
        .reconcile_stalled(&format!("/statuses/{}", env.head), |fake| {
            fake.with(|s| {
                s.repos
                    .get_mut(&1)
                    .unwrap()
                    .pulls
                    .get_mut(&1)
                    .unwrap()
                    .state = "closed".into()
            })
        })
        .await;
    assert_eq!(code(&out), 0);
    assert_eq!(env.states(&env.head), ["pending"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn attestation_comments_that_change_during_the_run_get_no_verdict() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    let out = env
        .reconcile_stalled(&format!("/statuses/{}", env.head), |fake| {
            fake.with(|s| {
                s.repos.get_mut(&1).unwrap().comments.get_mut(&1).unwrap()[0].updated_at =
                    "2026-02-02T00:00:00Z".into()
            })
        })
        .await;
    assert_eq!(code(&out), 0);
    assert_eq!(env.states(&env.head), ["pending"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn unread_or_unreadable_listings_fail_closed_without_writes() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body.clone());
    // More comments than the reader will page through.
    env.fake.with(|s| {
        let comments = s.repos.get_mut(&1).unwrap().comments.get_mut(&1).unwrap();
        for id in 100..1200 {
            comments.push(Comment {
                id,
                body: "chatter".into(),
                author: "x".into(),
                author_association: "NONE".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            });
        }
    });
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 1);
    assert_eq!(env.posts(), 0);

    env.fake.with(|s| {
        s.repos
            .get_mut(&1)
            .unwrap()
            .comments
            .get_mut(&1)
            .unwrap()
            .truncate(1);
    });
    for path in ["/pulls", "/comments", "/commits/"] {
        if path == "/commits/" {
            // Opt-in then depends on the head's statuses.
            env.fake
                .with(|s| s.repos.get_mut(&1).unwrap().comments.clear());
        }
        env.fake.with(|s| s.arrayless_path = Some(path.into()));
        let out = tokio::task::block_in_place(|| env.reconcile());
        assert_eq!(code(&out), 1, "{path}");
        assert_eq!(env.posts(), 0, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_head_shared_by_two_open_pull_requests_stays_pending() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    env.fake.with(|s| {
        s.repos.get_mut(&1).unwrap().pulls.insert(
            2,
            Pull {
                head_sha: env.head.clone(),
                base_ref: "release".into(),
                ..Pull::new(2, "other", &[])
            },
        );
    });
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let statuses = env.statuses(&env.head);
    assert_eq!(env.states(&env.head), ["pending", "pending"]);
    assert!(statuses[1].1.contains("shared"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_status_reply_is_an_error_and_a_rerun_converges() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    env.fake.with(|s| s.lose_next_status_reply = true);
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 1);
    assert_eq!(env.states(&env.head), ["pending"]);
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head).last().unwrap(), "success");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_and_rate_limit_failures_stop_the_run() {
    for status in [401, 429] {
        let env = Env::new().await;
        let body = env.attest();
        env.attest_comment(10, "OWNER", body);
        env.fake.with(|s| {
            s.fail.push_back(Failure {
                path_contains: "/statuses/".into(),
                status,
                headers: vec![],
            })
        });
        let out = tokio::task::block_in_place(|| env.reconcile());
        assert_eq!(code(&out), 1, "{status}");
        assert_eq!(env.posts(), 1, "{status}");
        assert!(env.statuses(&env.head).is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_for_another_repository_is_refused_before_any_request() {
    let env = Env::new().await;
    let policy = env
        .root()
        .join("checkout/.github/local-validation/policy.json");
    let text = fs::read_to_string(&policy).unwrap();
    fs::write(&policy, text.replace(REPOSITORY, "github.com/o/other")).unwrap();
    git(
        &env.root().join("checkout"),
        &["commit", "-q", "-am", "other repo"],
    );
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 2);
    assert!(env.fake.with(|s| s.requests.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bun_shaped_recipe_without_services_produces_posts_and_reconciles() {
    let env = Env::with_recipe(
        vec![("bun.lock", "# synthetic lock\n")],
        vec![RecipeCommand {
            id: "bun-test".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "test -f bun.lock".into()],
            timeout_secs: 30,
            paths: None,
        }],
        Some(vec!["bun.lock".into()]),
    )
    .await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["pending", "success"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_head_that_cannot_be_fetched_stays_pending_and_fails_the_run() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    git(
        &env.root().join("checkout"),
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );
    let out = env.reconcile();
    assert_eq!(code(&out), 1, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["pending"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("PR #1") && stderr.contains(&env.head));
}

#[tokio::test(flavor = "multi_thread")]
async fn verifier_configuration_that_changed_on_the_default_branch_gets_no_verdict() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    // The default branch advances its verifier inputs after this checkout was made.
    let root = env.root();
    let newer = root.join("newer");
    git(
        &root,
        &[
            "clone",
            "-q",
            root.join("origin.git").to_str().unwrap(),
            "newer",
        ],
    );
    fs::write(newer.join(".github/local-validation/registry.json"), "{}\n").unwrap();
    git(&newer, &["commit", "-q", "-am", "change registry"]);
    git(&newer, &["push", "-q", "origin", "HEAD:main"]);
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["pending"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("configuration changed since checkout"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_target_url_is_refused_before_any_request() {
    let env = Env::new().await;
    let mut args = env.reconcile_args();
    args.extend(["--target-url".into(), "javascript:alert(1)".into()]);
    let out = tokio::task::block_in_place(|| {
        env.cli(&args.iter().map(String::as_str).collect::<Vec<_>>())
    });
    assert_eq!(code(&out), 2);
    assert!(env.fake.with(|s| s.requests.is_empty()));
}

fn assert_refused_before_any_request(env: &Env, out: &Output) {
    assert_eq!(code(out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(env.fake.with(|s| s.requests.is_empty()));
    assert_eq!(env.posts(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_config_dirs_are_refused_before_any_request() {
    let env = Env::new().await;
    for dir in ["/abs/dir", "../up", "a/../b", "./x", "-x", ""] {
        let mut args = env.reconcile_args();
        args.push(format!("--config-dir={dir}"));
        let out = tokio::task::block_in_place(|| {
            env.cli(&args.iter().map(String::as_str).collect::<Vec<_>>())
        });
        assert_refused_before_any_request(&env, &out);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_config_dir_with_uncommitted_changes_is_refused_before_any_request() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    let dir = env.root().join("checkout/.github/local-validation");
    let registry = dir.join("registry.json");
    fs::write(&registry, "{}\n").unwrap();
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_refused_before_any_request(&env, &out);
    git(&env.root().join("checkout"), &["checkout", "--", "."]);
    fs::write(dir.join("untracked.json"), "{}").unwrap();
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_refused_before_any_request(&env, &out);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_config_dir_that_is_not_committed_at_head_is_refused_before_any_request() {
    let env = Env::new().await;
    let mut args = env.reconcile_args();
    args.push("--config-dir=missing".into());
    let out = tokio::task::block_in_place(|| {
        env.cli(&args.iter().map(String::as_str).collect::<Vec<_>>())
    });
    assert_refused_before_any_request(&env, &out);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_base_that_advanced_outside_the_config_dir_still_gets_its_verdict() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    let root = env.root();
    let newer = root.join("newer");
    git(
        &root,
        &[
            "clone",
            "-q",
            root.join("origin.git").to_str().unwrap(),
            "newer",
        ],
    );
    fs::write(newer.join("unrelated.txt"), "later").unwrap();
    git(&newer, &["add", "."]);
    git(&newer, &["commit", "-q", "-m", "unrelated"]);
    git(&newer, &["push", "-q", "origin", "HEAD:main"]);
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["pending", "success"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_head_that_moves_during_a_failed_fetch_is_left_pending_without_error() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    let root = env.root();
    let out = env
        .reconcile_stalled(&format!("/statuses/{}", env.head), |fake| {
            let work = root.join("work");
            git(&work, &["commit", "-q", "--allow-empty", "-m", "moved"]);
            let moved = git(&work, &["rev-parse", "HEAD"]);
            git(
                &work,
                &[
                    "push",
                    "-q",
                    "-f",
                    root.join("origin.git").to_str().unwrap(),
                    &format!("{moved}:refs/pull/1/head"),
                ],
            );
            fake.with(|s| {
                s.repos
                    .get_mut(&1)
                    .unwrap()
                    .pulls
                    .get_mut(&1)
                    .unwrap()
                    .head_sha = moved
            })
        })
        .await;
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["pending"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("left pending"));
}

/// Commits a copy of the verifier configuration at `sub/cfg` in the checkout and publishes it, so
/// `--repo` can be the subdirectory `sub` with `--config-dir=cfg`.
fn config_in_subdirectory(env: &Env) -> Vec<String> {
    let checkout = env.root().join("checkout");
    let cfg = checkout.join("sub/cfg");
    fs::create_dir_all(&cfg).unwrap();
    for name in ["policy.json", "recipe.json", "registry.json"] {
        fs::copy(
            checkout.join(".github/local-validation").join(name),
            cfg.join(name),
        )
        .unwrap();
    }
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "-q", "-m", "sub config"]);
    git(&checkout, &["push", "-q", "origin", "HEAD:main"]);
    let mut args = env.reconcile_args();
    let at = args.iter().position(|a| a == "--repo").unwrap() + 1;
    args[at] = checkout.join("sub").to_str().unwrap().to_owned();
    args.push("--config-dir=cfg".into());
    args
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repo_that_is_a_subdirectory_resolves_a_relative_config_dir_there() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    let args = config_in_subdirectory(&env);
    let out = tokio::task::block_in_place(|| {
        env.cli(&args.iter().map(String::as_str).collect::<Vec<_>>())
    });
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["pending", "success"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_config_dir_in_a_subdirectory_repo_is_refused_before_any_request() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    let args = config_in_subdirectory(&env);
    fs::write(env.root().join("checkout/sub/cfg/registry.json"), "{}\n").unwrap();
    let out = tokio::task::block_in_place(|| {
        env.cli(&args.iter().map(String::as_str).collect::<Vec<_>>())
    });
    assert_refused_before_any_request(&env, &out);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ignored_file_in_the_config_dir_is_refused_before_any_request() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body);
    let checkout = env.root().join("checkout");
    fs::write(checkout.join(".git/info/exclude"), "ignored.json\n").unwrap();
    fs::write(checkout.join(".github/local-validation/ignored.json"), "{}").unwrap();
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_refused_before_any_request(&env, &out);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_head_fetch_followed_by_an_auth_failure_on_the_recheck_stops_the_run() {
    for status in [401, 429] {
        let env = Env::new().await;
        let body = env.attest();
        env.attest_comment(10, "OWNER", body.clone());
        env.fake.with(|s| {
            let repo = s.repos.get_mut(&1).unwrap();
            repo.pulls.insert(2, Pull::new(2, "second", &[]));
            repo.comments.insert(
                2,
                vec![Comment {
                    id: 20,
                    body,
                    author: "maintainer-login".into(),
                    author_association: "OWNER".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                }],
            );
        });
        git(
            &env.root().join("checkout"),
            &["remote", "set-url", "origin", "/nonexistent/origin.git"],
        );
        let out = env
            .reconcile_stalled(&format!("/statuses/{}", env.head), |fake| {
                fake.with(|s| {
                    s.fail.push_back(Failure {
                        path_contains: "/pulls?state=open".into(),
                        status,
                        headers: vec![],
                    })
                })
            })
            .await;
        assert_eq!(code(&out), 1, "{status}");
        // The initial listing and the one failed recheck; the second pull request is never tried.
        let listings = env.fake.with(|s| {
            s.requests
                .iter()
                .filter(|r| r.path_and_query.contains("/pulls?state=open"))
                .count()
        });
        assert_eq!(listings, 2, "{status}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configuration_change_stops_the_loop_so_every_head_stays_pending() {
    let env = Env::new().await;
    let body = env.attest();
    env.attest_comment(10, "OWNER", body.clone());
    let root = env.root();
    let work = root.join("work");
    git(&work, &["commit", "-q", "--allow-empty", "-m", "second"]);
    let second = git(&work, &["rev-parse", "HEAD"]);
    git(
        &work,
        &[
            "push",
            "-q",
            root.join("origin.git").to_str().unwrap(),
            &format!("{second}:refs/pull/2/head"),
        ],
    );
    env.fake.with(|s| {
        let repo = s.repos.get_mut(&1).unwrap();
        let pull = Pull {
            head_sha: second.clone(),
            ..Pull::new(2, "second", &[])
        };
        repo.pulls.insert(2, pull);
        repo.comments.insert(
            2,
            vec![Comment {
                id: 20,
                body,
                author: "maintainer-login".into(),
                author_association: "OWNER".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            }],
        );
    });
    let newer = root.join("newer");
    git(
        &root,
        &[
            "clone",
            "-q",
            root.join("origin.git").to_str().unwrap(),
            "newer",
        ],
    );
    fs::write(newer.join(".github/local-validation/registry.json"), "{}\n").unwrap();
    git(&newer, &["commit", "-q", "-am", "change registry"]);
    git(&newer, &["push", "-q", "origin", "HEAD:main"]);
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["pending"]);
    assert_eq!(env.states(&second), ["pending"]);
}

const EXEMPT_DESCRIPTION: &str = "not required: only exempt paths changed";

#[tokio::test(flavor = "multi_thread")]
async fn an_unmarked_pull_request_changing_only_exempt_paths_passes_without_an_attestation() {
    let env = Env::exempting(vec![("README.md", "x"), ("docs/a.txt", "y")]).await;
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        env.statuses(&env.head),
        [("success".to_owned(), EXEMPT_DESCRIPTION.to_owned())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unmarked_pull_request_that_also_changes_a_required_path_gets_no_status() {
    let env = Env::exempting(vec![("README.md", "x"), ("src/a.rs", "y")]).await;
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.posts(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_to_the_config_dir_or_verifier_workflow_is_never_exempt() {
    for path in [
        ".github/local-validation/recipe.json",
        ".github/workflows/local-validation.yml",
    ] {
        let env = Env::exempting(vec![(path, "changed")]).await;
        let out = tokio::task::block_in_place(|| env.reconcile());
        assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(env.posts(), 0, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_change_is_never_exempt() {
    let env = Env::exempting(vec![]).await;
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.posts(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_that_adds_a_required_path_loses_the_exempt_success() {
    let mut env = Env::exempting(vec![("README.md", "x")]).await;
    let first = env.head.clone();
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&first), ["success"]);
    let second = env.push_head(&[("src/a.rs", "y")]);
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(env.states(&second).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exempt_change_beats_an_unusable_attestation_and_a_prior_status() {
    let env = Env::exempting(vec![("README.md", "x")]).await;
    env.attest_comment(10, "OWNER", format!("{MARKED}garbage -->"));
    env.prior_success();
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        env.statuses(&env.head).last().unwrap(),
        &("success".to_owned(), EXEMPT_DESCRIPTION.to_owned())
    );
    // Invalidated first, then replaced by the exemption.
    assert_eq!(env.states(&env.head), ["success", "pending", "success"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exempt_head_shared_by_two_pull_requests_ends_pending() {
    let env = Env::exempting(vec![("README.md", "x")]).await;
    env.prior_success();
    env.fake.with(|s| {
        s.repos.get_mut(&1).unwrap().pulls.insert(
            2,
            Pull {
                head_sha: env.head.clone(),
                ..Pull::new(2, "other", &[])
            },
        );
    });
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let statuses = env.statuses(&env.head);
    assert_eq!(statuses.last().unwrap().0, "pending");
    assert!(statuses.iter().all(|(_, d)| d != EXEMPT_DESCRIPTION));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_attestation_listing_a_path_scoped_command_as_not_required_passes() {
    let command = |id: &str, paths: Option<Vec<String>>| RecipeCommand {
        id: id.into(),
        argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
        timeout_secs: 30,
        paths,
    };
    let env = Env::with_recipe(
        vec![("a.txt", "a"), ("bun.lock", "# synthetic lock\n")],
        vec![
            command("gate", None),
            command("lint", Some(vec!["src".into()])),
        ],
        Some(vec!["bun.lock".into()]),
    )
    .await;
    let body = tokio::task::block_in_place(|| env.attest());
    env.attest_comment(10, "OWNER", body);
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["pending", "success"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unmarked_head_that_cannot_be_fetched_gets_no_status_and_does_not_fail_the_run() {
    let env = Env::exempting(vec![("README.md", "x")]).await;
    git(
        &env.root().join("checkout"),
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );
    let out = tokio::task::block_in_place(|| env.reconcile());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.posts(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opted_in_exempt_head_whose_comments_change_before_the_write_ends_pending() {
    let env = Env::exempting(vec![("README.md", "x")]).await;
    env.prior_success();
    let out = env
        .reconcile_stalled(&format!("/statuses/{}", env.head), |fake| {
            fake.with(|s| {
                s.repos.get_mut(&1).unwrap().comments.insert(
                    1,
                    vec![Comment {
                        id: 10,
                        body: MARKED.into(),
                        author: "maintainer-login".into(),
                        author_association: "OWNER".into(),
                        updated_at: "2026-01-01T00:00:00Z".into(),
                    }],
                );
            })
        })
        .await;
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.states(&env.head), ["success", "pending"]);
}
