//! Synthetic local-transport qualification for reconciliation fetch isolation.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentscm::local_validation::fetch::FetchContext;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Fixture {
    _root: tempfile::TempDir,
    checkout: PathBuf,
    head: String,
    canary: PathBuf,
    global_config: PathBuf,
    fake_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("checkout");
        fs::create_dir(&checkout).unwrap();
        git(&checkout, &["init", "-q", "-b", "main"]);
        fs::create_dir(checkout.join("cfg")).unwrap();
        fs::write(checkout.join("cfg/policy.json"), "{}").unwrap();
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-q", "-m", "base"]);
        let base = git(&checkout, &["rev-parse", "HEAD"]);
        fs::write(checkout.join("feature.txt"), "feature").unwrap();
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-q", "-m", "head"]);
        let head = git(&checkout, &["rev-parse", "HEAD"]);

        let origin = root.path().join("origin.git");
        git(
            root.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        git(
            &checkout,
            &[
                "push",
                "-q",
                origin.to_str().unwrap(),
                &format!("{base}:refs/heads/main"),
                &format!("{head}:refs/pull/1/head"),
            ],
        );
        git(
            &checkout,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );

        let canary = root.path().join("transport-executed");
        let upload_pack = root.path().join("upload-pack");
        fs::write(
            &upload_pack,
            format!("#!/bin/sh\ntouch '{}'\nexit 97\n", canary.display()),
        )
        .unwrap();
        fs::set_permissions(&upload_pack, fs::Permissions::from_mode(0o700)).unwrap();
        git(
            &checkout,
            &[
                "config",
                "--local",
                "remote.origin.uploadpack",
                upload_pack.to_str().unwrap(),
            ],
        );
        let included = root.path().join("included.gitconfig");
        fs::write(
            &included,
            format!(
                "[credential]\n\thelper = !touch '{}'\n[http]\n\textraHeader = Authorization: Basic should-not-be-read\n",
                canary.display()
            ),
        )
        .unwrap();
        git(
            &checkout,
            &[
                "config",
                "--local",
                "include.path",
                included.to_str().unwrap(),
            ],
        );
        let nonexistent = root.path().join("rewritten.git");
        git(
            &checkout,
            &[
                "config",
                "--local",
                &format!("url.{}.insteadOf", nonexistent.display()),
                origin.to_str().unwrap(),
            ],
        );

        let global_config = root.path().join("global.gitconfig");
        fs::write(
            &global_config,
            format!(
                "[url \"{}\"]\n\tinsteadOf = {}\n",
                nonexistent.display(),
                origin.display()
            ),
        )
        .unwrap();
        let fake_home = root.path().join("home");
        fs::create_dir(&fake_home).unwrap();
        fs::write(
            fake_home.join(".gitconfig"),
            format!(
                "[url \"{}\"]\n\tinsteadOf = {}\n",
                nonexistent.display(),
                origin.display()
            ),
        )
        .unwrap();

        Self {
            _root: root,
            checkout,
            head,
            canary,
            global_config,
            fake_home,
        }
    }
}

#[test]
fn anonymous_local_fetch_ignores_checkout_and_ambient_git_configuration() {
    let fixture = Fixture::new();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "local_validation_fetch::ambient_git_environment_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("AGENTSCM_FETCH_TEST_REPO", &fixture.checkout)
        .env("AGENTSCM_FETCH_TEST_HEAD", &fixture.head)
        .env("GIT_DIR", fixture.checkout.join("missing-git-dir"))
        .env("GIT_WORK_TREE", fixture.checkout.join("missing-work-tree"))
        .env("GIT_CONFIG_GLOBAL", &fixture.global_config)
        .env("GIT_CONFIG_SYSTEM", &fixture.global_config)
        .env("GIT_CONFIG_NOSYSTEM", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "url./nonexistent/from-env.insteadOf")
        .env(
            "GIT_CONFIG_VALUE_0",
            fixture.checkout.to_string_lossy().as_ref(),
        )
        .env("HOME", &fixture.fake_home)
        .env("XDG_CONFIG_HOME", &fixture.fake_home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.canary.exists());
}

#[test]
#[ignore = "subprocess helper"]
fn ambient_git_environment_helper() {
    let repo = PathBuf::from(std::env::var_os("AGENTSCM_FETCH_TEST_REPO").unwrap());
    let head = std::env::var("AGENTSCM_FETCH_TEST_HEAD").unwrap();
    let fetch = FetchContext::anonymous(&repo).unwrap();
    fetch.fetch_pull("main", 1, &head).unwrap();
    assert!(fetch.config_dir_unchanged("main", "cfg").unwrap());
    assert_eq!(
        git(&repo, &["rev-parse", "origin/main"]),
        git(&repo, &["rev-parse", "main~1"])
    );
}

#[test]
fn anonymous_fetch_rejects_credential_and_command_transports_before_execution() {
    for origin in [
        "https://user:password@github.com/o/r.git",
        "ext::sh -c touch should-not-run",
        "ssh://git@github.com/o/r.git",
        "git@github.com:o/r.git",
    ] {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q", "-b", "main"]);
        git(root.path(), &["remote", "add", "origin", origin]);
        assert!(
            FetchContext::anonymous(root.path()).is_err(),
            "accepted {origin}"
        );
    }
}
