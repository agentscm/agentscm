#![cfg(unix)]
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentscm::git_snapshot::{self, SnapshotRequest};
use agentscm::store::Engine;
use serde_json::{Value, json};
use tempfile::TempDir;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;

fn git_path() -> PathBuf {
    let path = PathBuf::from(
        std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".to_owned()),
    );
    assert!(
        path.exists(),
        "set AGENTSCM_TEST_GIT to an absolute git path"
    );
    path
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(git_path())
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

fn git_input(dir: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = Command::new(git_path())
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn git_bytes(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new(git_path())
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
    out.stdout
}

fn install_head(repo: &Path, tree: &str, message: &str) -> String {
    let commit = git(repo, &["commit-tree", tree, "-m", message]);
    git(repo, &["update-ref", "HEAD", &commit]);
    commit
}

/// Runs the CLI with an empty PATH: capture uses the absolute Git, nothing else may.
fn run(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("PATH", "")
        .output()
        .unwrap()
}

fn success(data: &Path, args: &[&str]) -> Value {
    let output = run(data, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn failure(data: &Path, args: &[&str]) -> String {
    let output = run(data, args);
    assert!(!output.status.success(), "{args:?}");
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    value["error"].as_str().unwrap().to_owned()
}

fn shell_quote(value: &Path) -> String {
    format!("'{}'", value.to_str().unwrap().replace('\'', "'\\''"))
}

fn versioned_git(root: &Path, version_output: &str, forward: bool) -> (PathBuf, PathBuf) {
    let script = root.join("synthetic-git");
    let calls = root.join("git-calls");
    let ready = root.join("version-checked");
    let forwarding = if forward {
        format!("exec {} \"$@\"", shell_quote(&git_path()))
    } else {
        "exit 88".to_owned()
    };
    let source = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nif [ ! -e {} ]; then\n  [ \"$#\" -eq 1 ] && [ \"$1\" = --version ] || exit 87\n  : > {}\n  printf '%s' {}\n  exit 0\nfi\n{}\n",
        shell_quote(&calls),
        shell_quote(&ready),
        shell_quote(&ready),
        shell_quote(Path::new(version_output)),
        forwarding,
    );
    fs::write(&script, source).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    (script, calls)
}

/// Files under `data/objects`, so any publication changes the count.
fn objects(data: &Path) -> usize {
    fn walk(dir: &Path) -> usize {
        fs::read_dir(dir).map_or(0, |entries| {
            entries
                .map(|e| {
                    let p = e.unwrap().path();
                    if p.is_dir() { walk(&p) } else { 1 }
                })
                .sum()
        })
    }
    walk(&data.join("objects"))
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    commit: String,
}

fn fixture() -> Fixture {
    fixture_with_object_format("sha1")
}

fn fixture_with_object_format(object_format: &str) -> Fixture {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(repo.join("src/deep")).unwrap();
    git(
        &repo,
        &["init", "-q", &format!("--object-format={object_format}")],
    );
    fs::write(repo.join("README.md"), "hello\n").unwrap();
    fs::write(repo.join("src/lib.rs"), "fn lib() {}\n").unwrap();
    fs::write(repo.join("src/deep/mod.rs"), "fn deep() {}\n").unwrap();
    std::os::unix::fs::symlink("README.md", repo.join("link")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    Fixture {
        _temp: temp,
        data: root.join("data"),
        root,
        repo,
        commit,
    }
}

#[test]
fn snapshot_checks_supported_git_version_before_repository_commands() {
    for version in [
        "git version 2.45.0\n",
        "git version 2.50.1 (Apple Git-155)\n",
        "git version 2.45.0.windows.1\n",
    ] {
        let f = fixture();
        let (synthetic_git, calls) = versioned_git(&f.root, version, true);
        let args = [
            "git-snapshot",
            "preflight",
            "--git",
            synthetic_git.to_str().unwrap(),
            "--repository",
            f.repo.to_str().unwrap(),
            "--revision",
            &f.commit,
            "--exclude",
            "link",
        ];
        let report = success(&f.data, &args);
        assert_eq!(report["preflight"]["git_version"], version.trim_end());
        let calls = fs::read_to_string(calls).unwrap();
        assert_eq!(calls.lines().next(), Some("--version"));
        assert!(calls.lines().nth(1).unwrap().contains("rev-parse"));
        assert!(!f.data.exists());
    }
}

#[test]
fn snapshot_rejects_old_or_malformed_git_before_other_commands() {
    for (version, expected) in [
        (
            "git version 2.44.9\n",
            "unsupported Git version: requires Git 2.45.0 or newer (found git version 2.44.9)",
        ),
        (
            "git version 2.45.0.rc1\n",
            "unsupported Git version: requires Git 2.45.0 or newer (version output was not recognized)",
        ),
        (
            "git version 2.45.0\nextra\n",
            "unsupported Git version: requires Git 2.45.0 or newer (version output was not recognized)",
        ),
    ] {
        let f = fixture();
        let (synthetic_git, calls) = versioned_git(&f.root, version, false);
        let error = failure(
            &f.data,
            &[
                "git-snapshot",
                "preflight",
                "--git",
                synthetic_git.to_str().unwrap(),
                "--repository",
                f.repo.to_str().unwrap(),
                "--revision",
                &f.commit,
            ],
        );
        assert_eq!(error, expected);
        assert_eq!(fs::read_to_string(calls).unwrap(), "--version\n");
        assert!(!f.data.exists());
    }
}

#[test]
fn snapshot_version_failure_is_typed_and_output_bounded() {
    let f = fixture();
    let (old_git, _) = versioned_git(&f.root, "git version 2.1.0\n", false);
    let request = SnapshotRequest {
        git: old_git,
        repository: f.repo.clone(),
        revision: f.commit.clone(),
        exclude: Vec::new(),
    };
    assert!(matches!(
        git_snapshot::preflight(&request),
        Err(agentscm::Error::UnsupportedGitVersion {
            minimum: "2.45.0",
            found: Some(found),
        }) if found == "git version 2.1.0"
    ));

    for (name, source) in [
        (
            "invalid-utf8",
            "#!/bin/sh\nprintf 'git version 2.45.0 \\377\\n'\n",
        ),
        ("nonzero", "#!/bin/sh\nprintf 'probe failed' >&2\nexit 2\n"),
    ] {
        let script = f.root.join(name);
        fs::write(&script, source).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let request = SnapshotRequest {
            git: script,
            repository: f.repo.clone(),
            revision: f.commit.clone(),
            exclude: Vec::new(),
        };
        assert!(matches!(
            git_snapshot::preflight(&request),
            Err(agentscm::Error::UnsupportedGitVersion {
                minimum: "2.45.0",
                found: None,
            })
        ));
    }

    let flood_root = f.root.join("flood");
    fs::create_dir(&flood_root).unwrap();
    let script = flood_root.join("git");
    fs::write(
        &script,
        "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 5000 ]; do printf x >&2; i=$((i + 1)); done\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let request = SnapshotRequest {
        git: script,
        repository: f.repo,
        revision: f.commit,
        exclude: Vec::new(),
    };
    assert!(matches!(
        git_snapshot::preflight(&request),
        Err(agentscm::Error::UnsupportedGitVersion {
            minimum: "2.45.0",
            found: None,
        })
    ));
}

fn loose_object(repo: &Path, oid: &str) -> PathBuf {
    repo.join(".git/objects").join(&oid[..2]).join(&oid[2..])
}

impl Fixture {
    fn init(&self) -> String {
        success(&self.data, &["init"])["repository"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn args<'a>(&'a self, sub: &'a str, receipt: Option<&'a str>) -> Vec<String> {
        let mut args: Vec<String> = [
            "git-snapshot",
            sub,
            "--repository",
            self.repo.to_str().unwrap(),
            "--revision",
            &self.commit,
            "--exclude",
            "link",
        ]
        .map(String::from)
        .into();
        if let Some(receipt) = receipt {
            args.extend(["--receipt".to_owned(), receipt.to_owned()]);
        }
        args
    }

    fn capture(&self, receipt: &Path) -> Output {
        let args = self.args("capture", Some(receipt.to_str().unwrap()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&self.data, &args)
    }
}

#[test]
fn sha1_and_sha256_snapshots_capture_verified_content_end_to_end() {
    for (object_format, oid_length, verification) in [
        ("sha1", 40, "sha1-rehashed"),
        ("sha256", 64, "sha256-rehashed"),
    ] {
        let mut f = fixture_with_object_format(object_format);
        fs::write(f.repo.join("empty"), []).unwrap();
        fs::write(f.repo.join("binary"), [0, 255, 1, 128]).unwrap();
        git(&f.repo, &["add", "empty", "binary"]);
        git(&f.repo, &["commit", "-q", "-m", "add byte fixtures"]);
        f.commit = git(&f.repo, &["rev-parse", "HEAD"]);
        assert_eq!(f.commit.len(), oid_length);
        let preflight_args = f.args("preflight", None);
        let preflight = success(
            &f.data,
            &preflight_args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        assert!(!f.data.exists(), "{object_format} preflight created data");
        let report = &preflight["preflight"];
        assert_eq!(report["eligible"], true, "{object_format}");
        assert_eq!(report["object_format"], object_format);
        assert_eq!(report["commit"].as_str().unwrap().len(), oid_length);
        assert_eq!(report["tree"].as_str().unwrap().len(), oid_length);
        assert!(
            report["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["oid"].as_str().unwrap().len() == oid_length),
            "{object_format} manifest contains the wrong OID width"
        );

        f.init();
        let target_before = success(&f.data, &["target", "main"]);
        let events_before = success(&f.data, &["events", "--after", "0", "--limit", "10"]);
        let receipt = f.root.join(format!("{object_format}-receipt.json"));
        let capture = f.capture(&receipt);
        assert!(
            capture.status.success(),
            "{object_format}: {}",
            String::from_utf8_lossy(&capture.stderr)
        );
        let captured: Value = serde_json::from_slice(&capture.stdout).unwrap();
        assert_eq!(captured["existing"], false, "{object_format}");
        let revision = captured["revision"].as_str().unwrap();
        let receipt_bytes = fs::read(&receipt).unwrap();
        let receipt_doc: Value = serde_json::from_slice(&receipt_bytes).unwrap();
        assert_eq!(receipt_doc["git"]["object_format"], object_format);
        assert_eq!(
            receipt_doc["git"]["commit"].as_str().unwrap().len(),
            oid_length
        );
        assert_eq!(
            receipt_doc["git"]["tree"].as_str().unwrap().len(),
            oid_length
        );
        assert_eq!(receipt_doc["content_verification"], verification);

        let engine = Engine::open(&f.data).unwrap();
        for (path, expected) in [
            ("empty", &b""[..]),
            ("README.md", &b"hello\n"[..]),
            ("binary", &[0, 255, 1, 128][..]),
        ] {
            assert_eq!(
                engine.read_file(revision, path).unwrap(),
                expected,
                "{object_format} captured incorrect bytes for {path}"
            );
        }
        assert_eq!(success(&f.data, &["target", "main"]), target_before);
        assert_eq!(
            success(&f.data, &["events", "--after", "0", "--limit", "10"]),
            events_before
        );

        let ids = object_ids(&f.data);
        let retry = f.capture(&receipt);
        assert!(
            retry.status.success(),
            "{object_format}: {}",
            String::from_utf8_lossy(&retry.stderr)
        );
        let again: Value = serde_json::from_slice(&retry.stdout).unwrap();
        assert_eq!(again["existing"], true, "{object_format}");
        assert_eq!(again["revision"], revision, "{object_format}");
        assert_eq!(fs::read(&receipt).unwrap(), receipt_bytes);
        assert_eq!(object_ids(&f.data), ids);
        assert_eq!(success(&f.data, &["target", "main"]), target_before);
        assert_eq!(
            success(&f.data, &["events", "--after", "0", "--limit", "10"]),
            events_before
        );
    }
}

#[test]
fn sha1_and_sha256_same_size_blob_corruption_fails_before_publication() {
    for object_format in ["sha1", "sha256"] {
        let f = fixture_with_object_format(object_format);
        f.init();
        let objects_before = object_ids(&f.data);
        let target_before = success(&f.data, &["target", "main"]);
        let events_before = success(&f.data, &["events", "--after", "0", "--limit", "10"]);
        let original = git(&f.repo, &["rev-parse", "HEAD:README.md"]);
        let replacement = git_input(&f.repo, &["hash-object", "-w", "--stdin"], b"jello\n");
        assert_ne!(replacement, original);
        fs::remove_file(loose_object(&f.repo, &original)).unwrap();
        fs::copy(
            loose_object(&f.repo, &replacement),
            loose_object(&f.repo, &original),
        )
        .unwrap();
        assert_eq!(
            git_bytes(&f.repo, &["cat-file", "blob", &original]),
            b"jello\n",
            "{object_format} Git did not return the substituted bytes"
        );

        let preflight_args = f.args("preflight", None);
        let preflight = success(
            &f.data,
            &preflight_args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        assert_eq!(preflight["preflight"]["eligible"], true, "{object_format}");
        let receipt = f.root.join(format!("{object_format}-corrupt-receipt.json"));
        let error = failure(
            &f.data,
            &f.args("capture", Some(receipt.to_str().unwrap()))
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            error, "invalid: content of README.md does not hash to its object ID",
            "{object_format}"
        );
        assert!(!receipt.exists());
        assert_eq!(object_ids(&f.data), objects_before);
        assert_eq!(success(&f.data, &["target", "main"]), target_before);
        assert_eq!(
            success(&f.data, &["events", "--after", "0", "--limit", "10"]),
            events_before
        );
    }
}

#[test]
fn legacy_git_reported_sha1_receipt_conflicts_before_publication() {
    let f = fixture();
    f.init();
    let seed_receipt = f.root.join("seed-receipt.json");
    assert!(f.capture(&seed_receipt).status.success());
    let mut legacy: Value = serde_json::from_slice(&fs::read(seed_receipt).unwrap()).unwrap();

    let fresh_data = f.root.join("fresh-data");
    let repository = success(&fresh_data, &["init"])["repository"]
        .as_str()
        .unwrap()
        .to_owned();
    legacy["native"]["repository"] = repository.into();
    legacy["content_verification"] = "git-reported-sha1".into();
    let receipt = f.root.join("legacy-receipt.json");
    fs::write(&receipt, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let receipt_bytes = fs::read(&receipt).unwrap();
    let objects_before = object_ids(&fresh_data);
    let target_before = success(&fresh_data, &["target", "main"]);
    let events_before = success(&fresh_data, &["events", "--after", "0", "--limit", "10"]);

    let error = failure(
        &fresh_data,
        &f.args("capture", Some(receipt.to_str().unwrap()))
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    assert!(error.starts_with("conflict: receipt path"), "{error}");
    assert_eq!(fs::read(&receipt).unwrap(), receipt_bytes);
    assert_eq!(object_ids(&fresh_data), objects_before);
    assert_eq!(success(&fresh_data, &["target", "main"]), target_before);
    assert_eq!(
        success(&fresh_data, &["events", "--after", "0", "--limit", "10"]),
        events_before
    );
}

struct DeepFixture {
    _temp: TempDir,
    root: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    receipt: PathBuf,
    commit: String,
    expected: BTreeMap<String, Vec<u8>>,
}

fn deep_fixture() -> DeepFixture {
    const FILES: usize = 130;
    const DEPTH: usize = 127;

    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let directory = std::iter::repeat_n("d", DEPTH)
        .collect::<Vec<_>>()
        .join("/");
    fs::create_dir_all(repo.join(&directory)).unwrap();
    git(&repo, &["init", "-q"]);
    let mut expected = BTreeMap::new();
    for number in 0..FILES {
        let path = format!("{directory}/f{number:03}");
        let content = vec![number as u8, 0, 255, (number * 3) as u8];
        fs::write(repo.join(&path), &content).unwrap();
        expected.insert(path, content);
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "deep prefixes"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    DeepFixture {
        data: root.join("data"),
        receipt: root.join("receipt.json"),
        _temp: temp,
        root,
        repo,
        commit,
        expected,
    }
}

impl DeepFixture {
    fn args(&self, sub: &str, receipt: Option<&Path>) -> Vec<String> {
        let mut args = vec![
            "git-snapshot".to_owned(),
            sub.to_owned(),
            "--repository".to_owned(),
            self.repo.to_str().unwrap().to_owned(),
            "--revision".to_owned(),
            self.commit.clone(),
        ];
        if let Some(receipt) = receipt {
            args.extend(["--receipt".to_owned(), receipt.to_str().unwrap().to_owned()]);
        }
        args
    }
}

fn object_ids(data: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    for shard in fs::read_dir(data.join("objects")).unwrap() {
        let shard = shard.unwrap();
        let prefix = shard.file_name().into_string().unwrap();
        for object in fs::read_dir(shard.path()).unwrap() {
            let suffix = object.unwrap().file_name().into_string().unwrap();
            ids.push(format!("{prefix}{suffix}"));
        }
    }
    ids.sort();
    ids
}

fn assert_limit_rejected_before_publication(
    root: &Path,
    repo: &Path,
    commit: &str,
    expected_violations: Value,
) {
    let unopened_data = root.join("unopened-data");
    let args = [
        "git-snapshot",
        "preflight",
        "--repository",
        repo.to_str().unwrap(),
        "--revision",
        commit,
    ];
    let preflight = success(&unopened_data, &args);
    assert!(
        !unopened_data.exists(),
        "preflight created its data directory"
    );
    let report = &preflight["preflight"];
    assert_eq!(report["cleanliness"]["clean"], false);
    assert_eq!(report["eligible"], false);
    assert_eq!(report["limit_violations"], expected_violations);

    let data = root.join("data");
    success(&data, &["init"]);
    let objects_before = objects(&data);
    let target_before = success(&data, &["target", "main"]);
    let receipt = root.join("receipt.json");
    let error = failure(
        &data,
        &[
            "git-snapshot",
            "capture",
            "--repository",
            repo.to_str().unwrap(),
            "--revision",
            commit,
            "--receipt",
            receipt.to_str().unwrap(),
        ],
    );
    assert!(error.contains("not eligible"), "{error}");
    assert_eq!(objects(&data), objects_before);
    assert!(!receipt.exists());
    assert_eq!(success(&data, &["target", "main"]), target_before);
}

#[test]
#[ignore = "qualification test constructs the production 100,000-entry Git boundary"]
fn snapshot_entry_limit_is_precise_before_publication() {
    const DIRECTORIES: usize = 50_000;

    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let blob = git_input(&repo, &["hash-object", "-w", "--stdin"], b"x");
    let subtree = git_input(
        &repo,
        &["mktree"],
        format!("100644 blob {blob}\tf\n").as_bytes(),
    );
    let mut root_tree = Vec::with_capacity(DIRECTORIES * 70);
    for number in 0..DIRECTORIES {
        writeln!(root_tree, "040000 tree {subtree}\td{number:05}").unwrap();
    }
    let tree = git_input(&repo, &["mktree"], &root_tree);
    let commit = install_head(&repo, &tree, "entry boundary");

    // Plumbing-only commits need no 100,000-path checkout. Their deliberately
    // dirty checkout is independent of the precise tree limit classification.
    assert_limit_rejected_before_publication(
        &root,
        &repo,
        &commit,
        json!([{"limit": "entries", "observed": 100_000, "maximum": 99_999}]),
    );
}

#[test]
#[ignore = "qualification test hashes a sparse blob above the production byte bounds"]
fn snapshot_byte_limits_are_precise_before_publication() {
    const BLOB_BYTES: u64 = 64 * 1024 * 1024 + 1;

    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let sparse = root.join("sparse-blob");
    File::create(&sparse).unwrap().set_len(BLOB_BYTES).unwrap();
    let blob = git(&repo, &["hash-object", "-w", sparse.to_str().unwrap()]);
    let tree = git_input(
        &repo,
        &["mktree"],
        format!("100644 blob {blob}\ta\n100644 blob {blob}\tb\n100644 blob {blob}\tc\n").as_bytes(),
    );
    let commit = install_head(&repo, &tree, "byte boundaries");

    assert_limit_rejected_before_publication(
        &root,
        &repo,
        &commit,
        json!([
            {"limit": "file_bytes", "observed": BLOB_BYTES, "maximum": 64 * 1024 * 1024, "path": "a"},
            {"limit": "file_bytes", "observed": BLOB_BYTES, "maximum": 64 * 1024 * 1024, "path": "b"},
            {"limit": "file_bytes", "observed": BLOB_BYTES, "maximum": 64 * 1024 * 1024, "path": "c"},
            {"limit": "total_bytes", "observed": BLOB_BYTES * 3, "maximum": 128 * 1024 * 1024}
        ]),
    );
}

struct CaptureChild(Option<Child>);

impl Drop for CaptureChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
#[ignore = "child process for capture interruption qualification"]
fn capture_interruption_child() {
    let data = PathBuf::from(std::env::var_os("AGENTSCM_CAPTURE_DATA").unwrap());
    let repository = PathBuf::from(std::env::var_os("AGENTSCM_CAPTURE_REPOSITORY").unwrap());
    let revision = std::env::var("AGENTSCM_CAPTURE_REVISION").unwrap();
    let repository_id = std::env::var("AGENTSCM_CAPTURE_REPOSITORY_ID").unwrap();
    let ready = PathBuf::from(std::env::var_os("AGENTSCM_CAPTURE_READY").unwrap());
    let request = SnapshotRequest {
        git: PathBuf::from(std::env::var_os("AGENTSCM_CAPTURE_GIT").unwrap()),
        repository,
        revision,
        exclude: Vec::new(),
    };
    let report = git_snapshot::preflight(&request).unwrap();
    let engine = Engine::open(data).unwrap();
    git_snapshot::capture_with_progress(&engine, &repository_id, &request, report, &mut |line| {
        if line == "wrote batch 1/2" {
            fs::write(&ready, line).unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
            panic!("parent did not interrupt capture at the batch boundary");
        }
    })
    .unwrap();
}

#[test]
#[ignore = "qualification test exercises capture across production checkpoint batches"]
fn capture_qualifies_shared_deep_prefixes_across_native_batches() {
    const FILES: usize = 130;
    const DEPTH: usize = 127;

    let fixture = deep_fixture();
    let DeepFixture {
        repo,
        data,
        receipt,
        commit,
        expected,
        ..
    } = &fixture;
    success(data, &["init"]);

    let args = |sub: &str, receipt_path: Option<&Path>| {
        let mut args = vec![
            "git-snapshot".to_owned(),
            sub.to_owned(),
            "--repository".to_owned(),
            repo.to_str().unwrap().to_owned(),
            "--revision".to_owned(),
            commit.clone(),
        ];
        if let Some(receipt_path) = receipt_path {
            args.extend([
                "--receipt".to_owned(),
                receipt_path.to_str().unwrap().to_owned(),
            ]);
        }
        args
    };
    let preflight_args = args("preflight", None);
    let preflight = success(
        data,
        &preflight_args
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    let report = &preflight["preflight"];
    assert_eq!(report["eligible"], true);
    assert_eq!(
        report["totals"],
        json!({"files": FILES, "directories": DEPTH, "bytes": FILES * 4})
    );
    assert_eq!(
        report["totals"]["files"].as_u64().unwrap()
            + report["totals"]["directories"].as_u64().unwrap(),
        257
    );
    let entries = report["entries"].as_array().unwrap();
    assert_eq!(entries.len(), FILES);
    assert!(
        entries
            .iter()
            .all(|entry| entry["path"].as_str().unwrap().len() < 1024)
    );
    let prefix_sum: usize = entries
        .iter()
        .map(|entry| {
            entry["path"]
                .as_str()
                .unwrap()
                .bytes()
                .filter(|byte| *byte == b'/')
                .count()
        })
        .sum();
    assert_eq!(prefix_sum, FILES * DEPTH);
    assert_eq!(prefix_sum, 16_510);

    let target_before = success(data, &["target", "main"]);
    let events_before = success(data, &["events", "--after", "0", "--limit", "10"]);
    let capture_args = args("capture", Some(receipt));
    let captured = success(
        data,
        &capture_args.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(captured["existing"], false);
    assert_eq!(captured["batches"], 2);
    let revision = captured["revision"].as_str().unwrap().to_owned();
    let receipt_bytes = fs::read(receipt).unwrap();
    let receipt_doc: Value = serde_json::from_slice(&receipt_bytes).unwrap();
    assert_eq!(receipt_doc["native"]["revision"], revision);
    assert_eq!(receipt_doc["native"]["batches"], 2);

    let engine = Engine::open(data).unwrap();
    assert!(engine.load_revision(&revision).unwrap().parents.is_empty());
    let history = success(data, &["history", &revision, "--limit", "10"]);
    assert_eq!(history["page"]["entries"].as_array().unwrap().len(), 1);
    assert_eq!(history["page"]["entries"][0]["id"], revision);
    assert_eq!(
        history["page"]["entries"][0]["revision"]["parents"],
        json!([])
    );
    assert_eq!(history["page"]["needed_objects"], json!([]));
    assert!(history["page"]["next"].is_null());

    let mut listed = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = engine.list_files(&revision, cursor.as_deref(), 17).unwrap();
        for entry in page.entries {
            assert!(
                listed
                    .insert(
                        entry.path.clone(),
                        engine.read_file(&revision, &entry.path).unwrap()
                    )
                    .is_none()
            );
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(&listed, expected);
    assert_eq!(success(data, &["target", "main"]), target_before);
    assert_eq!(
        success(data, &["events", "--after", "0", "--limit", "10"]),
        events_before
    );

    let again = success(
        data,
        &capture_args.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(again["existing"], true);
    assert_eq!(again["revision"], revision);
    assert_eq!(again["batches"], 2);
    assert_eq!(fs::read(receipt).unwrap(), receipt_bytes);
    assert_eq!(success(data, &["target", "main"]), target_before);
    assert_eq!(
        success(data, &["events", "--after", "0", "--limit", "10"]),
        events_before
    );
}

#[test]
#[ignore = "qualification test sends SIGKILL after a durable production capture batch"]
fn interrupted_multi_batch_capture_retries_to_complete_identical_revision() {
    let fixture = deep_fixture();
    let repository_id = success(&fixture.data, &["init"])["repository"]
        .as_str()
        .unwrap()
        .to_owned();
    let target_before = success(&fixture.data, &["target", "main"]);
    let events_before = success(&fixture.data, &["events", "--after", "0", "--limit", "10"]);
    let ready = fixture.root.join("capture.ready");
    let stdout_path = fixture.root.join("child.stdout");
    let stderr_path = fixture.root.join("child.stderr");
    let mut owned = CaptureChild(Some(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "capture_interruption_child",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("AGENTSCM_CAPTURE_DATA", &fixture.data)
            .env("AGENTSCM_CAPTURE_REPOSITORY", &fixture.repo)
            .env("AGENTSCM_CAPTURE_REVISION", &fixture.commit)
            .env("AGENTSCM_CAPTURE_REPOSITORY_ID", &repository_id)
            .env("AGENTSCM_CAPTURE_READY", &ready)
            .env("AGENTSCM_CAPTURE_GIT", git_path())
            .stdin(Stdio::null())
            .stdout(File::create(&stdout_path).unwrap())
            .stderr(File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap(),
    ));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(fs::metadata(&stdout_path).unwrap().len() <= 64 * 1024);
        assert!(fs::metadata(&stderr_path).unwrap().len() <= 64 * 1024);
        if matches!(fs::read(&ready), Ok(bytes) if bytes == b"wrote batch 1/2") {
            break;
        }
        assert!(
            owned.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "child exited before the batch boundary: {}",
            fs::read_to_string(&stderr_path).unwrap()
        );
        assert!(Instant::now() < deadline, "batch boundary was not reached");
        thread::sleep(Duration::from_millis(10));
    }
    let child = owned.0.as_mut().unwrap();
    child.kill().unwrap();
    let status = child.wait().unwrap();
    drop(owned.0.take());
    assert_eq!(status.signal(), Some(9));
    assert!(fs::metadata(&stdout_path).unwrap().len() <= 64 * 1024);
    assert!(fs::metadata(&stderr_path).unwrap().len() <= 64 * 1024);

    assert!(!fixture.receipt.exists());
    assert_eq!(success(&fixture.data, &["target", "main"]), target_before);
    assert_eq!(
        success(&fixture.data, &["events", "--after", "0", "--limit", "10"]),
        events_before
    );
    let interrupted_ids = object_ids(&fixture.data);
    assert!(
        !interrupted_ids.is_empty(),
        "first batch retained no objects"
    );
    let interrupted_engine = Engine::open(&fixture.data).unwrap();
    assert!(
        interrupted_ids
            .iter()
            .all(|id| interrupted_engine.load_revision(id).is_err()),
        "an interrupted capture published a revision object"
    );

    let capture_args = fixture.args("capture", Some(&fixture.receipt));
    let retried = success(
        &fixture.data,
        &capture_args.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(retried["existing"], false);
    assert_eq!(retried["batches"], 2);
    let revision = retried["revision"].as_str().unwrap();
    let complete_ids = object_ids(&fixture.data);
    assert!(
        interrupted_ids.iter().all(|id| complete_ids.contains(id)),
        "retry discarded retained orphan objects"
    );

    let clean_data = fixture.root.join("clean-data");
    success(&clean_data, &["init"]);
    let clean_receipt = fixture.root.join("clean-receipt.json");
    let clean_args = fixture.args("capture", Some(&clean_receipt));
    let uninterrupted = success(
        &clean_data,
        &clean_args.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(uninterrupted["revision"], revision);
    assert_eq!(uninterrupted["batches"], 2);

    let engine = Engine::open(&fixture.data).unwrap();
    assert!(engine.load_revision(revision).unwrap().parents.is_empty());
    let mut listed = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = engine.list_files(revision, cursor.as_deref(), 17).unwrap();
        for entry in page.entries {
            assert!(
                listed
                    .insert(
                        entry.path.clone(),
                        engine.read_file(revision, &entry.path).unwrap()
                    )
                    .is_none()
            );
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(listed, fixture.expected);
    let history = success(&fixture.data, &["history", revision, "--limit", "10"]);
    assert_eq!(history["page"]["entries"].as_array().unwrap().len(), 1);
    assert_eq!(history["page"]["entries"][0]["id"], revision);
    assert_eq!(history["page"]["needed_objects"], json!([]));
    assert!(history["page"]["next"].is_null());
    assert_eq!(success(&fixture.data, &["target", "main"]), target_before);
    assert_eq!(
        success(&fixture.data, &["events", "--after", "0", "--limit", "10"]),
        events_before
    );
}

#[test]
fn capture_progress_is_opt_in_and_bounded() {
    let f = fixture();
    f.init();
    let receipt = f.root.join("receipt.json");
    let quiet = f.capture(&receipt);
    assert!(quiet.status.success());
    assert!(quiet.stderr.is_empty(), "piped stderr stays empty");

    let mut args = f.args("capture", Some(receipt.to_str().unwrap()));
    args.push("--progress".into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let loud = run(&f.data, &args);
    assert!(loud.status.success());
    let result: Value = serde_json::from_slice(&loud.stdout).unwrap();
    let first: Value = serde_json::from_slice(&quiet.stdout).unwrap();
    assert_eq!(result["revision"], first["revision"]);
    assert_eq!(result["existing"], true, "rerun reuses the receipt");
    let stderr = String::from_utf8(loud.stderr).unwrap();
    let lines: Vec<&str> = stderr.lines().collect();
    assert!(
        lines.iter().all(|l| l.starts_with("progress: ")),
        "{stderr}"
    );
    let batches = result["batches"].as_u64().unwrap() as usize;
    // At most one line per file chunk (1024 files each; the fixture is far
    // below the 16 MiB chunk cap) and per batch, plus the published line. The
    // excluded symlink is not captured.
    let files = git(&f.repo, &["ls-files"])
        .lines()
        .filter(|name| *name != "link")
        .count();
    let chunks = files.div_ceil(1024);
    assert!(lines.len() <= chunks + batches + 1, "{stderr}");
    let published = format!(
        "progress: published revision {}",
        result["revision"].as_str().unwrap()
    );
    assert_eq!(lines.last().copied(), Some(published.as_str()), "{stderr}");
}

#[test]
fn capture_then_native_workflow_without_git() {
    let f = fixture();
    let repository = f.init();
    let receipt = f.root.join("receipt.json");
    let head_before = git(&f.repo, &["rev-parse", "HEAD"]);
    let output = f.capture(&receipt);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        (result["existing"].as_bool(), result["batches"].as_u64()),
        (Some(false), Some(1))
    );
    let revision = result["revision"].as_str().unwrap().to_owned();
    let bytes = fs::read(&receipt).unwrap();
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(doc["git"]["commit"], f.commit);
    assert_eq!(doc["git"]["object_format"], "sha1");
    assert_eq!(doc["native"]["revision"], revision);
    assert_eq!(doc["native"]["repository"], repository);
    assert_eq!(doc["profile"], "agentscm-git-snapshot/v1");
    assert_eq!(doc["manifest_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(doc["excluded"][0]["path"], "link");

    // Rerun: same revision, receipt reused byte for byte.
    let again: Value = serde_json::from_slice(&f.capture(&receipt).stdout).unwrap();
    assert_eq!(
        (again["existing"].as_bool(), again["revision"].as_str()),
        (Some(true), Some(revision.as_str()))
    );
    assert_eq!(fs::read(&receipt).unwrap(), bytes);

    // Start a Change with declared provenance and incomplete criteria.
    let context = json!({
        "version":1,"change":{"repository":repository,"id":"snapshot-check"},"change_version":1,
        "revision":revision,"base":null,"target":"main","policy_epoch":0,
        "intent":"Check the Git snapshot","acceptance_criteria":["Edits checkpoint without Git"],
        "scope":["src/lib.rs"],"exclusions":[],"omissions":["criteria not yet reviewed"],
        "dependencies":[
            {"identity":"https://example.invalid/issues/20","state":{"state":"declared"}},
            {"identity":format!("git:sha1:{}", f.commit),"state":{"state":"declared"}}],
        "recipe":{"state":"unknown"},
        "toolchain":{"state":"unknown"},"environment":{"state":"unknown"},
        "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
    });
    let context_path = f.root.join("context.json");
    fs::write(&context_path, serde_json::to_vec(&context).unwrap()).unwrap();
    let started = success(&f.data, &["start", context_path.to_str().unwrap()]);
    assert_eq!(started["readiness"]["status"], "incomplete");
    let ws = f.root.join("ws");
    let ws_arg = ws.to_str().unwrap();
    success(
        &f.data,
        &["workspace", "create", ws_arg, "snapshot-check", &revision],
    );
    assert!(!ws.join(".git").exists());
    assert_eq!(
        fs::read(ws.join("src/deep/mod.rs")).unwrap(),
        b"fn deep() {}\n"
    );
    assert!(!ws.join("link").exists());

    fs::write(ws.join("src/lib.rs"), "fn lib() { 1 }\n").unwrap();
    fs::write(ws.join("new.txt"), "new\n").unwrap();
    let first = success(&f.data, &["workspace", "checkpoint", ws_arg, "op-1", "0"]);
    let first = first["outcome"]["revision"].as_str().unwrap().to_owned();
    fs::write(ws.join("new.txt"), "newer\n").unwrap();
    let second = success(&f.data, &["workspace", "checkpoint", ws_arg, "op-2", "1"]);
    let second = second["outcome"]["revision"].as_str().unwrap().to_owned();
    assert_ne!(first, second);
    let read = |rev: &str, path: &str| success(&f.data, &["read", rev, path])["content"].clone();
    assert_eq!(read(&first, "new.txt"), json!(b"new\n".to_vec()));
    assert_eq!(read(&second, "new.txt"), json!(b"newer\n".to_vec()));
    assert_eq!(
        read(&second, "src/lib.rs"),
        json!(b"fn lib() { 1 }\n".to_vec())
    );
    assert_eq!(
        read(&revision, "src/lib.rs"),
        json!(b"fn lib() {}\n".to_vec())
    );

    assert!(success(&f.data, &["target", "main"])["target"].is_null());
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
    assert!(!f.repo.join(".agentscm").exists());
}

#[test]
fn native_workspace_preserves_claude_command_alias() {
    const COMMANDS: [&str; 6] = ["commit", "fix-pr", "handoff", "implement", "pr", "review"];

    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let commands = repo.join(".agents/commands");
    fs::create_dir_all(&commands).unwrap();
    git(&repo, &["init", "-q"]);
    for name in COMMANDS {
        fs::write(
            commands.join(format!("{name}.md")),
            format!("synthetic command: {name}\n"),
        )
        .unwrap();
    }
    std::os::unix::fs::symlink(".agents", repo.join(".claude")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "commands"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let data = root.join("data");
    let receipt = root.join("receipt.json");
    let explicit_args = |sub: &str, receipt: Option<&Path>| {
        let mut args = vec![
            "git-snapshot".to_owned(),
            sub.to_owned(),
            "--repository".to_owned(),
            repo.to_str().unwrap().to_owned(),
            "--revision".to_owned(),
            commit.clone(),
        ];
        if let Some(receipt) = receipt {
            args.extend(["--receipt".to_owned(), receipt.to_str().unwrap().to_owned()]);
        }
        args
    };
    let preflight_args = explicit_args("preflight", None);
    let preflight_refs: Vec<&str> = preflight_args.iter().map(String::as_str).collect();
    let preflight = success(&data, &preflight_refs);
    assert_eq!(preflight["preflight"]["eligible"], true);
    assert_eq!(
        preflight["preflight"]["profile"],
        "agentscm-git-snapshot/v2"
    );
    assert_eq!(preflight["preflight"]["excluded"], json!([]));
    assert!(!data.exists());

    let repository = success(&data, &["init"])["repository"]
        .as_str()
        .unwrap()
        .to_owned();
    let capture_args = explicit_args("capture", Some(&receipt));
    let capture_refs: Vec<&str> = capture_args.iter().map(String::as_str).collect();
    let captured = success(&data, &capture_refs);
    let revision = captured["revision"].as_str().unwrap();
    let receipt_doc: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(receipt_doc["profile"], "agentscm-git-snapshot/v2");
    assert_eq!(receipt_doc["excluded"], json!([]));
    let claude = receipt_doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == ".claude")
        .unwrap();
    assert_eq!(claude["mode"], "120000");

    let context = json!({
        "version":1,"change":{"repository":repository,"id":"commands"},"change_version":1,
        "revision":revision,"base":null,"target":"main","policy_epoch":0,
        "intent":"Verify synthetic command layout","acceptance_criteria":["Commands are readable"],
        "scope":[".agents/commands"],"exclusions":[],"omissions":["live command invocation not verified"],
        "dependencies":[],"recipe":{"state":"unknown"},"toolchain":{"state":"unknown"},
        "environment":{"state":"unknown"},"required_evidence":[],
        "required_capabilities":["agentscm.review-context/v1"]
    });
    let context_path = root.join("context.json");
    fs::write(&context_path, serde_json::to_vec(&context).unwrap()).unwrap();
    success(&data, &["start", context_path.to_str().unwrap()]);
    let workspace = root.join("workspace");
    success(
        &data,
        &[
            "workspace",
            "create",
            workspace.to_str().unwrap(),
            "commands",
            revision,
        ],
    );

    assert_eq!(
        fs::read_link(workspace.join(".claude")).unwrap(),
        Path::new(".agents")
    );
    for name in COMMANDS {
        let relative = format!("commands/{name}.md");
        let direct = fs::read(workspace.join(".agents").join(&relative)).unwrap();
        assert_eq!(
            fs::read(workspace.join(".claude").join(relative)).unwrap(),
            direct
        );
        assert_eq!(direct, format!("synthetic command: {name}\n").as_bytes());
    }
    assert!(!workspace.join(".git").exists());
    let status = success(&data, &["workspace", "status", workspace.to_str().unwrap()]);
    assert_eq!(status["status"]["complete"], true);
    assert_eq!(status["status"]["changes"], json!([]));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), commit);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
}

#[test]
fn conflicting_receipt_and_failed_captures_leave_files_untouched() {
    let f = fixture();
    f.init();
    let receipt = f.root.join("receipt.json");
    f.capture(&receipt);
    let bytes = fs::read(&receipt).unwrap();
    let published = objects(&f.data);
    fs::write(f.repo.join("more.txt"), "m\n").unwrap();
    git(&f.repo, &["add", "more.txt"]);
    git(&f.repo, &["commit", "-q", "-m", "two"]);
    let newer = git(&f.repo, &["rev-parse", "HEAD"]);
    let fixture_args = [
        "git-snapshot",
        "capture",
        "--repository",
        f.repo.to_str().unwrap(),
        "--revision",
        &newer,
        "--exclude",
        "link",
        "--receipt",
        receipt.to_str().unwrap(),
    ];
    assert!(failure(&f.data, &fixture_args).starts_with("conflict:"));
    assert_eq!(fs::read(&receipt).unwrap(), bytes);
    assert_eq!(objects(&f.data), published);

    // Dirty tracked file and a missing receipt directory: precise failure, no receipt.
    fs::write(f.repo.join("README.md"), "dirty\n").unwrap();
    let missing = f.root.join("nope/receipt.json");
    let output = f.capture(&missing);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"].as_str().unwrap().contains("No such file"),
        "{error}"
    );
    assert!(!missing.exists());
    let other = f.root.join("other.json");
    let message = {
        let args = f.args("capture", Some(other.to_str().unwrap()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        failure(&f.data, &args)
    };
    assert!(
        message.contains("not clean") && message.contains("README.md"),
        "{message}"
    );
    assert!(!other.exists());
}

#[test]
fn overlapping_paths_are_rejected_before_any_write() {
    let f = fixture();
    // Data directory inside the repository.
    let inside = f.repo.join("data");
    success(&inside, &["init"]);
    let args = f.args("capture", Some(f.root.join("r.json").to_str().unwrap()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let status_before = git(&f.repo, &["status", "--porcelain"]);
    assert!(failure(&inside, &args).contains("non-nested"));
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), status_before);

    f.init();
    for (name, receipt) in [
        ("repository", f.repo.join("receipt.json")),
        ("data", f.data.join("receipt.json")),
        ("git directory", f.repo.join(".git/receipt.json")),
    ] {
        let args = f.args("capture", Some(receipt.to_str().unwrap()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let message = failure(&f.data, &args);
        assert!(
            message.contains("receipt must be outside"),
            "{name}: {message}"
        );
        assert!(!receipt.exists(), "{name}");
    }
}

#[test]
fn preflight_reports_without_opening_the_data_directory() {
    let f = fixture();
    let args = f.args("preflight", None);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let report = success(&f.data, &args);
    assert_eq!(report["preflight"]["eligible"], true);
    assert_eq!(report["preflight"]["commit"], f.commit);
    assert!(!f.data.exists());

    // Not eligible is still a successful observation.
    fs::write(f.repo.join("README.md"), "dirty\n").unwrap();
    let report = success(&f.data, &args);
    assert_eq!(report["preflight"]["eligible"], false);
    assert!(!f.data.exists());

    // Capture on an uninitialized data directory fails without creating it.
    let message = failure(
        &f.data,
        &f.args("capture", Some(f.root.join("x.json").to_str().unwrap()))
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    assert!(message.contains("No such file or directory"), "{message}");
    assert!(!f.data.exists());
}

#[test]
fn non_regular_or_foreign_receipt_conflicts_before_publication() {
    let f = fixture();
    f.init();
    let before = objects(&f.data);
    let directory = f.root.join("receipt-dir");
    fs::create_dir(&directory).unwrap();
    let foreign = f.root.join("foreign.json");
    fs::write(&foreign, "not a receipt").unwrap();
    for path in [&directory, &foreign] {
        let args = f.args("capture", Some(path.to_str().unwrap()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let message = failure(&f.data, &args);
        assert!(message.starts_with("conflict:"), "{message}");
    }
    assert_eq!(fs::read(&foreign).unwrap(), b"not a receipt");
    assert_eq!(objects(&f.data), before);
}

#[test]
fn rerun_after_new_untracked_file_conflicts_before_publication() {
    let f = fixture();
    f.init();
    let receipt = f.root.join("receipt.json");
    assert!(f.capture(&receipt).status.success());
    let bytes = fs::read(&receipt).unwrap();
    let published = objects(&f.data);
    assert!(published > 0);

    // An identical rerun is idempotent.
    let args = f.args("capture", Some(receipt.to_str().unwrap()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    assert_eq!(success(&f.data, &args)["existing"], true);

    // A changed checkout observation changes the receipt: refuse before publishing.
    fs::write(f.repo.join("untracked.txt"), "u\n").unwrap();
    let message = failure(&f.data, &args);
    assert!(message.starts_with("conflict:"), "{message}");
    assert_eq!(fs::read(&receipt).unwrap(), bytes);
    assert_eq!(objects(&f.data), published);
}

#[test]
fn non_object_receipt_conflicts_without_panicking() {
    let f = fixture();
    f.init();
    let before = objects(&f.data);
    let receipt = f.root.join("array.json");
    fs::write(&receipt, "[]").unwrap();
    let output = f.capture(&receipt);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    let message = value["error"].as_str().unwrap();
    assert!(message.starts_with("conflict:"), "{message}");
    assert_eq!(fs::read(&receipt).unwrap(), b"[]");
    assert_eq!(objects(&f.data), before);
}
