//! `local-validation` CLI against a synthetic Git repository. Synthetic keys only.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agentscm::local_validation::{
    CHECK_KIND, RecipeCommand, Services, ValidationPolicy, ValidationRecipe,
};
use agentscm::policy::issuers::{
    CommonControl, IssuerKeyRecord, IssuerRecord, IssuerRegistrySnapshot, issuer_key_id,
};
use ed25519_dalek::SigningKey;
use serde_json::Value;

struct Fixture {
    dir: tempfile::TempDir,
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(repo)
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
}

fn command(id: &str, script: &str, timeout_secs: u64) -> RecipeCommand {
    RecipeCommand {
        id: id.into(),
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        timeout_secs,
    }
}

impl Fixture {
    fn new(commands: Vec<RecipeCommand>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("a.txt"), "a").unwrap();
        fs::write(repo.join("Cargo.lock"), "# synthetic lock\n").unwrap();
        fs::write(repo.join("rust-toolchain.toml"), "[toolchain]\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        fs::write(repo.join("b.txt"), "b").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "work"]);

        let key = SigningKey::from_bytes(&[7; 32]);
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
            repository: "repo-1".into(),
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
        };
        let policy = ValidationPolicy {
            repository: "repo-1".into(),
            recipe_sha256: recipe.sha256().unwrap(),
            required_services: Services {
                postgres: false,
                s3: false,
            },
            max_age_seconds: 3600,
            epoch: 1,
        };
        fs::write(
            root.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("recipe.json"),
            serde_json::to_vec(&recipe).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("policy.json"),
            serde_json::to_vec(&policy).unwrap(),
        )
        .unwrap();
        Self { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn repo(&self) -> PathBuf {
        self.root().join("repo")
    }

    fn state(&self) -> PathBuf {
        self.root().join("state")
    }

    fn cli(&self, args: &[&str]) -> Output {
        let data = self.root().join("data");
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["--no-telemetry", "--data-dir"])
            .arg(data)
            .arg("local-validation")
            .args(args)
            .env("GH_TOKEN", "synthetic-secret")
            .env(
                "AGENTSCM_TEST_DATABASE_URL",
                "postgres://synthetic.invalid/x",
            )
            .env("AGENTSCM_TEST_S3_ENDPOINT", "http://synthetic.invalid")
            .env("AGENTSCM_TEST_S3_BUCKET", "b")
            .env("AGENTSCM_TEST_S3_ACCESS_KEY_ID", "k")
            .env("AGENTSCM_TEST_S3_SECRET_ACCESS_KEY", "s")
            .output()
            .unwrap()
    }

    fn run(&self) -> Output {
        let root = self.root();
        let p = |n: &str| root.join(n).to_str().unwrap().to_owned();
        self.cli(&[
            "run",
            "--repo",
            self.repo().to_str().unwrap(),
            "--recipe",
            &p("recipe.json"),
            "--policy",
            &p("policy.json"),
            "--registry",
            &p("registry.json"),
            "--key-file",
            &p("key"),
            "--issuer-id",
            "maintainer",
            "--state-dir",
            self.state().to_str().unwrap(),
        ])
    }

    /// Runs `run` and returns the envelope digest and exit code.
    fn attest(&self) -> (String, i32) {
        let out = self.run();
        let json: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
        (
            json["envelope_sha256"].as_str().unwrap().into(),
            out.status.code().unwrap(),
        )
    }

    fn show_to_file(&self, sha: &str, name: &str) -> PathBuf {
        let out = self.cli(&[
            "show",
            "--state-dir",
            self.state().to_str().unwrap(),
            "--attestation",
            sha,
        ]);
        assert!(out.status.success());
        let path = self.root().join(name);
        fs::write(&path, &out.stdout).unwrap();
        path
    }

    fn verify(&self, body: &Path, repository: &str) -> (i32, Value) {
        self.verify_all(&[body], repository)
    }

    /// Bodies oldest first.
    fn verify_all(&self, bodies: &[&Path], repository: &str) -> (i32, Value) {
        let root = self.root();
        let mut args = vec!["verify"];
        for body in bodies {
            args.extend(["--comment-body", body.to_str().unwrap()]);
        }
        let out = self.cli(
            &[
                &args[..],
                &[
                    "--policy",
                    root.join("policy.json").to_str().unwrap(),
                    "--registry",
                    root.join("registry.json").to_str().unwrap(),
                    "--recipe",
                    root.join("recipe.json").to_str().unwrap(),
                    "--repo",
                    self.repo().to_str().unwrap(),
                    "--repository",
                    repository,
                ][..],
            ]
            .concat(),
        );
        let json = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
        (out.status.code().unwrap(), json)
    }

    fn record(&self, sha: &str) -> Value {
        serde_json::from_slice(&fs::read(self.state().join(format!("{sha}.record.json"))).unwrap())
            .unwrap()
    }
}

fn ok_fixture() -> Fixture {
    Fixture::new(vec![command(
        "gate",
        r#"test -z "$GH_TOKEN" && test -n "$AGENTSCM_TEST_DATABASE_URL""#,
        30,
    )])
}

#[test]
fn success_flow_then_stale_after_new_commit() {
    let f = ok_fixture();
    let (sha, code) = f.attest();
    assert_eq!(code, 0, "GH_TOKEN must not reach recipe commands");
    let body = f.show_to_file(&sha, "body.md");
    let text = fs::read_to_string(&body).unwrap();
    assert!(text.starts_with(&format!("<!-- agentscm-local-validation:v1 {sha} -->\n")));
    let again = f.show_to_file(&sha, "body2.md");
    assert_eq!(text, fs::read_to_string(again).unwrap());
    let (code, verdict) = f.verify(&body, "repo-1");
    assert_eq!(
        (code, verdict["state"].as_str()),
        (0, Some("success")),
        "{verdict}"
    );
    assert_eq!(verdict["envelope_sha256"], sha.as_str());

    fs::write(f.repo().join("c.txt"), "c").unwrap();
    git(&f.repo(), &["add", "."]);
    git(&f.repo(), &["commit", "-q", "-m", "more"]);
    let (code, verdict) = f.verify(&body, "repo-1");
    assert_eq!(
        (code, verdict["reason"].as_str()),
        (3, Some("no attestation for this head")),
        "{verdict}"
    );
}

#[test]
fn wrong_repository_tampering_and_marker_mismatch_fail() {
    let f = ok_fixture();
    let (sha, _) = f.attest();
    let body = f.show_to_file(&sha, "body.md");
    assert_eq!(f.verify(&body, "repo-2").0, 3);

    let text = fs::read_to_string(&body).unwrap();
    let at = text.find("```agentscm-record-v1\n").unwrap() + "```agentscm-record-v1\n".len();
    let mut tampered = text.clone().into_bytes();
    tampered[at + 4] = if tampered[at + 4] == b'A' { b'B' } else { b'A' };
    let path = f.root().join("tampered.md");
    fs::write(&path, tampered).unwrap();
    assert_eq!(f.verify(&path, "repo-1").0, 3);

    let path = f.root().join("marker.md");
    fs::write(&path, text.replace(&sha, &"0".repeat(64))).unwrap();
    let (code, verdict) = f.verify(&path, "repo-1");
    assert_eq!(
        (code, verdict["reason"].as_str()),
        (3, Some("no attestation for this head"))
    );

    let path = f.root().join("dup.md");
    let record_block = &text[text.find("```agentscm-record-v1").unwrap()..];
    fs::write(&path, format!("{text}\n{record_block}")).unwrap();
    assert_eq!(f.verify(&path, "repo-1").0, 3);
}

#[test]
fn missing_lockfile_is_an_error_not_an_empty_digest() {
    let f = ok_fixture();
    git(&f.repo(), &["rm", "-q", "Cargo.lock"]);
    git(&f.repo(), &["commit", "-q", "-m", "drop lock"]);
    let out = f.run();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Cargo.lock"));
    assert!(!f.state().exists() || fs::read_dir(f.state()).unwrap().next().is_none());
}

#[test]
fn verify_selects_the_newest_attestation_for_the_head() {
    let f = Fixture::new(vec![command("gate", "test -f marker", 30)]);
    let (bad, code) = f.attest();
    assert_eq!(code, 1);
    let bad = f.show_to_file(&bad, "bad.md");
    // Not an attestation at all.
    let junk = f.root().join("junk.md");
    fs::write(&junk, "hello").unwrap();
    let (code, verdict) = f.verify_all(&[&junk], "repo-1");
    assert_eq!((code, verdict["state"].as_str()), (3, Some("pending")));
    let (code, _) = f.verify_all(&[&junk, &bad], "repo-1");
    assert_eq!(code, 1);
    // A later junk body does not override the failure.
    let (code, _) = f.verify_all(&[&bad, &junk], "repo-1");
    assert_eq!(code, 1);
}

#[test]
fn verify_input_errors_are_error_verdicts_not_failures() {
    let f = ok_fixture();
    let missing = f.root().join("missing.md");
    let (code, verdict) = f.verify_all(&[&missing], "repo-1");
    assert_eq!((code, verdict["state"].as_str()), (2, Some("error")));
    fs::remove_file(f.root().join("policy.json")).unwrap();
    let (code, verdict) = f.verify_all(&[&missing], "repo-1");
    assert_eq!((code, verdict["state"].as_str()), (2, Some("error")));
}

#[test]
fn command_output_is_retained_privately_and_matches_the_record_digests() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new(vec![command("noisy", "echo out; echo err >&2; exit 1", 30)]);
    let (sha, code) = f.attest();
    assert_eq!(code, 1);
    let record = f.record(&sha);
    for (stream, key) in [("stdout", "stdout_sha256"), ("stderr", "stderr_sha256")] {
        let path = f.state().join(format!("{sha}.cmd1.{stream}"));
        let bytes = fs::read(&path).unwrap();
        assert_eq!(
            bytes,
            if stream == "stdout" {
                b"out\n"
            } else {
                b"err\n"
            }
        );
        assert_eq!(
            record["results"][0][key].as_str().unwrap(),
            format!("{:x}", Sha256::digest(&bytes))
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn dirty_tree_is_refused_before_running() {
    let f = ok_fixture();
    fs::write(f.repo().join("untracked.txt"), "x").unwrap();
    let out = f.run();
    assert!(!out.status.success());
    assert!(!f.state().exists() || fs::read_dir(f.state()).unwrap().next().is_none());
}

#[test]
fn failures_are_signed_retained_and_never_verify() {
    let f = Fixture::new(vec![
        command("bad", "exit 1", 30),
        command("never", "exit 0", 30),
    ]);
    let (sha, code) = f.attest();
    assert_eq!(code, 1);
    assert_eq!(
        f.record(&sha)["results"].as_array().unwrap().len(),
        1,
        "stops at first failure"
    );
    let body = f.show_to_file(&sha, "body.md");
    assert_eq!(f.verify(&body, "repo-1").0, 1);
}

#[test]
fn timeout_and_output_flood_are_recorded_as_stops() {
    let f = Fixture::new(vec![command("slow", "sleep 20", 1)]);
    let (sha, code) = f.attest();
    assert_eq!(code, 1);
    assert_eq!(f.record(&sha)["results"][0]["stop"], "timed_out");

    let f = Fixture::new(vec![command("flood", "head -c 20000000 /dev/zero", 60)]);
    let (sha, code) = f.attest();
    assert_eq!(code, 1);
    let result = &f.record(&sha)["results"][0];
    assert_eq!(
        (&result["stop"], &result["truncated"]),
        (&Value::from("output_limit"), &Value::from(true))
    );
}

#[test]
fn command_that_dirties_the_tree_is_incomplete() {
    let f = Fixture::new(vec![command("dirty", "echo x > generated.txt", 30)]);
    let (sha, code) = f.attest();
    assert_eq!(code, 1);
    assert_eq!(f.record(&sha)["tree_unchanged_after"], false);
    let body = f.show_to_file(&sha, "body.md");
    let (code, verdict) = f.verify(&body, "repo-1");
    assert_eq!(
        (code, verdict["reason"].as_str()),
        (1, Some("tree changed")),
        "{verdict}"
    );
}

#[path = "support/fake_github.rs"]
mod fake_github;

use fake_github::{Comment, FakeGithub, Pull, Repo};

const TOKEN: &str = "synthetic-token";

impl Fixture {
    fn head(&self) -> String {
        let out = Command::new("git")
            .current_dir(self.repo())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn post(&self, api: &str, sha: &str) -> Output {
        let token = self.root().join("token");
        fs::write(&token, TOKEN).unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        tokio::task::block_in_place(|| {
            self.cli(&[
                "post",
                "--state-dir",
                self.state().to_str().unwrap(),
                "--attestation",
                sha,
                "--github-repo",
                "o/r",
                "--pr",
                "7",
                "--token-file",
                token.to_str().unwrap(),
                "--api-url",
                api,
            ])
        })
    }
}

async fn fake(head: &str) -> FakeGithub {
    let fake = FakeGithub::start().await;
    fake.with(|s| {
        let mut repo = Repo::new(1, "o/r");
        repo.pulls.insert(
            7,
            Pull {
                head_sha: head.into(),
                ..Pull::new(7, "feature", &[])
            },
        );
        s.repos.insert(1, repo);
        s.pats.insert(
            TOKEN.into(),
            ("maintainer-login".into(), 9, [1].into_iter().collect()),
        );
    });
    fake
}

fn comments(fake: &FakeGithub) -> Vec<Comment> {
    fake.with(|s| s.repos[&1].comments.get(&7).cloned().unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread")]
async fn post_creates_once_and_rerun_adopts_the_comment() {
    let f = ok_fixture();
    let fake = fake(&f.head()).await;
    let (sha, _) = tokio::task::block_in_place(|| f.attest());
    let out = f.post(&fake.base, &sha);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let first = comments(&fake);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].author, "maintainer-login");
    let receipt: Value =
        serde_json::from_slice(&fs::read(f.state().join(format!("{sha}.posted.json"))).unwrap())
            .unwrap();
    assert_eq!(receipt["comment_id"], first[0].id);
    assert_eq!(receipt["pr"], 7);
    assert!(
        receipt["html_url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("#issuecomment-{}", first[0].id))
    );

    assert!(f.post(&fake.base, &sha).status.success());
    assert_eq!(comments(&fake).len(), 1);
    // The producer side never posts a commit status.
    assert!(fake.with(|s| s.repos[&1].statuses.is_empty()));

    // A different attestation is a new comment; the earlier one stays.
    let (second, _) = tokio::task::block_in_place(|| f.attest());
    assert_ne!(second, sha);
    assert!(f.post(&fake.base, &second).status.success());
    assert_eq!(comments(&fake).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_create_reply_is_adopted_not_duplicated() {
    let f = ok_fixture();
    let fake = fake(&f.head()).await;
    let (sha, _) = tokio::task::block_in_place(|| f.attest());
    fake.with(|s| s.lose_next_comment_reply = true);
    assert!(!f.post(&fake.base, &sha).status.success());
    assert_eq!(comments(&fake).len(), 1);
    assert!(!f.state().join(format!("{sha}.posted.json")).exists());
    assert!(f.post(&fake.base, &sha).status.success());
    assert_eq!(comments(&fake).len(), 1);
    assert!(f.state().join(format!("{sha}.posted.json")).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn another_authors_marker_comment_is_not_adopted() {
    let f = ok_fixture();
    let fake = fake(&f.head()).await;
    let (sha, _) = tokio::task::block_in_place(|| f.attest());
    let body = String::from_utf8(
        tokio::task::block_in_place(|| {
            f.cli(&[
                "show",
                "--state-dir",
                f.state().to_str().unwrap(),
                "--attestation",
                &sha,
            ])
        })
        .stdout,
    )
    .unwrap();
    fake.with(|s| {
        s.repos.get_mut(&1).unwrap().comments.insert(
            7,
            vec![Comment {
                id: 50,
                body,
                author: "mallory".into(),
            }],
        );
    });
    assert!(f.post(&fake.base, &sha).status.success());
    let all = comments(&fake);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].author, "mallory");
    assert_eq!(all[1].author, "maintainer-login");
}

#[tokio::test(flavor = "multi_thread")]
async fn moved_pr_head_is_refused_and_nothing_is_posted() {
    let f = ok_fixture();
    let fake = fake(&"9".repeat(40)).await;
    let (sha, _) = tokio::task::block_in_place(|| f.attest());
    let out = f.post(&fake.base, &sha);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("head differs"));
    assert!(comments(&fake).is_empty());
    assert!(!f.state().join(format!("{sha}.posted.json")).exists());
}
