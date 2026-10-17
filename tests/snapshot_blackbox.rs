//! One small CLI-only synthetic acceptance; real shadow work is a separate gate.
#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type Files = BTreeMap<String, Vec<u8>>;
const GIT: &str = "/usr/bin/git";

// Direct children are reaped locally; descendants inherit the outer runner's group.
struct OwnedChild(Option<Child>, File, File);
impl OwnedChild {
    fn spawn(mut command: Command) -> Self {
        // Recovery inventories can exceed a pipe buffer before the child exits.
        let root = command.get_current_dir().unwrap();
        let stdout = tempfile::tempfile_in(root).unwrap();
        let stderr = tempfile::tempfile_in(root).unwrap();
        command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap());
        Self(
            Some(command.spawn().expect("spawn isolated command")),
            stdout,
            stderr,
        )
    }
    fn interrupt(&mut self) -> ExitStatus {
        let child = self.0.as_mut().expect("owned child");
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before interruption"
        );
        child.kill().expect("SIGKILL owned direct child");
        let status = child.wait().expect("reap interrupted child");
        drop(self.0.take());
        status
    }
    fn output(mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "isolated command deadline");
            assert!(self.1.metadata().unwrap().len() <= 16 * 1024 * 1024);
            assert!(self.2.metadata().unwrap().len() <= 16 * 1024 * 1024);
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = self.0.as_mut().unwrap().wait().unwrap();
        drop(self.0.take());
        let read = |file: &mut File| {
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut bytes = Vec::new();
            file.take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .unwrap();
            assert!(
                bytes.len() <= 16 * 1024 * 1024,
                "fixture output exceeds 16 MiB"
            );
            bytes
        };
        Output {
            status,
            stdout: read(&mut self.1),
            stderr: read(&mut self.2),
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// The one trusted paused Git wrapper acknowledges its terminal no-CAS decision.
struct ExportGate {
    cancel: PathBuf,
    ack: PathBuf,
    acknowledged: bool,
}
impl ExportGate {
    fn cancel_and_wait(&mut self) -> std::io::Result<String> {
        fs::write(&self.cancel, b"cancel")?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match fs::read_to_string(&self.ack) {
                Ok(ack) if ack == "cancelled\n" || ack == "deadline\n" => {
                    self.acknowledged = true;
                    return Ok(ack);
                }
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "trusted Git wrapper did not ACK cancellation",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for ExportGate {
    fn drop(&mut self) {
        if !self.acknowledged
            && let Err(error) = self.cancel_and_wait()
        {
            eprintln!("export cancellation cleanup: {error}");
        }
    }
}

fn command(executable: &Path, root: &Path) -> Command {
    let mut c = Command::new(executable);
    c.current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Synthetic operator")
        .env("GIT_AUTHOR_EMAIL", "synthetic@example.invalid")
        .env("GIT_COMMITTER_NAME", "Synthetic operator")
        .env("GIT_COMMITTER_EMAIL", "synthetic@example.invalid")
        .env("GIT_AUTHOR_DATE", "1780000000 +0000")
        .env("GIT_COMMITTER_DATE", "1780000000 +0000");
    c
}
fn output(c: Command) -> Output {
    OwnedChild::spawn(c).output()
}
fn success(o: &Output) {
    assert!(
        o.status.success(),
        "status {:?}\nstdout {}\nstderr {}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}
fn git(root: &Path, repo: &Path, args: &[&str]) -> Vec<u8> {
    let mut c = command(Path::new(GIT), root);
    c.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgSign=false",
        "-C",
    ])
    .arg(repo)
    .args(args);
    let o = output(c);
    success(&o);
    o.stdout
}
fn string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap().trim().to_owned()
}
fn text(v: &Value) -> &str {
    v.as_str().expect("public string field")
}
fn write(root: &Path, path: &str, content: &[u8]) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, content).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap();
}
fn request(root: &Path, name: &str, value: &Value) -> PathBuf {
    let p = root.join(format!("{name}.json"));
    assert!(!p.exists());
    fs::write(&p, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    p
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn native_blob_id(bytes: &[u8]) -> String {
    // Pinned experimental native blob profile; this is not raw-file SHA-256.
    let mut encoded = b"AGSCM-B1\0".to_vec();
    encoded.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    encoded.extend_from_slice(bytes);
    sha(&encoded)
}

struct Cli {
    root: PathBuf,
    data: PathBuf,
}
impl Cli {
    fn command(&self, args: &[&str]) -> Command {
        let mut c = command(Path::new(env!("CARGO_BIN_EXE_agentscm")), &self.root);
        c.arg("--data-dir").arg(&self.data).args(args);
        c
    }
    fn ok(&self, args: &[&str]) -> Value {
        let o = output(self.command(args));
        success(&o);
        let v: Value = serde_json::from_slice(&o.stdout).expect("one JSON response");
        assert_eq!(v["ok"], true, "{args:?}: {v}");
        v
    }
    fn fail(&self, args: &[&str]) {
        let o = output(self.command(args));
        assert!(
            !o.status.success(),
            "unexpected success {args:?}: {}",
            String::from_utf8_lossy(&o.stdout)
        );
        let error: Value =
            serde_json::from_slice(&o.stderr).expect("failure JSON, not a signal/usage failure");
        assert_eq!(error["ok"], false);
        assert!(!text(&error["error"]).is_empty());
    }
    fn start(&self, id: &str, workspace: &Path, revision: &str, expected: Option<&[u8]>) {
        let digest = "1".repeat(64);
        let mut args = vec![
            "workflow",
            "start",
            "--change",
            id,
            "--workspace",
            workspace.to_str().unwrap(),
            "--revision",
            revision,
            "--issue",
            "https://example.invalid/synthetic",
            "--intent",
            "Exact isolated synthetic tree",
            "--criterion",
            "Every supported byte and path matches",
            "--scope",
            ".",
            "--recipe",
            &digest,
            "--toolchain",
            &digest,
            "--environment",
            &digest,
        ];
        let source = expected.map(|b| format!("src/message.txt={}", sha(b)));
        if let Some(s) = &source {
            args.extend(["--require-source-path", s]);
        }
        self.ok(&args);
    }
    fn inspect(&self) -> Value {
        self.ok(&["workflow", "inspect", "--limit", "4096"])
    }
    fn check(&self, id: &str, expected: &[u8]) -> Value {
        self.ok(&["workflow", "check", id]);
        let view = self.inspect();
        let attempt = &view["evidence"]["controller_attempt"];
        assert_eq!(view["evidence"]["controller_status"], "success");
        assert_eq!(attempt["attempt_id"], id);
        assert_eq!(attempt["state"], "success");
        assert_eq!(attempt["subject"]["change"], view["context"]["change"]);
        assert_eq!(
            attempt["subject"]["change_version"],
            view["context"]["change_version"]
        );
        assert_eq!(attempt["subject"]["revision"], view["context"]["revision"]);
        assert_eq!(
            attempt["subject"]["context_digest"],
            view["change"]["context_digest"]
        );
        for input in ["recipe", "toolchain", "environment"] {
            assert_eq!(
                attempt["subject"][format!("{input}_sha256")],
                view["context"][input]["sha256"]
            );
        }
        let evidence = self.ok(&["check-evidence", text(&attempt["evidence_digest"])]);
        assert_eq!(evidence["evidence"]["attempt_id"], id);
        assert_eq!(evidence["evidence"]["state"], "success");
        assert_eq!(evidence["evidence"]["subject"], attempt["subject"]);
        for field in ["request_digest", "check_kind", "limits", "run_generation"] {
            assert_eq!(
                evidence["evidence"][field], attempt[field],
                "artifact binding {field}"
            );
        }
        assert_eq!(attempt["check_kind"], "source_paths/v1");
        assert_eq!(
            evidence["evidence"]["execution_basis"],
            "controller_builtin"
        );
        assert_eq!(
            evidence["evidence"]["observations"],
            json!([{
                "path":"src/message.txt","sha256":sha(expected),"bytes":expected.len()
            }])
        );
        view
    }
    fn approve(&self, id: &str, view: &Value) {
        assert_eq!(view["reviewable"], true);
        assert_eq!(view["diff"]["complete"], true);
        self.ok(&[
            "workflow",
            "approve",
            id,
            "--inspection",
            text(&view["inspection"]),
        ]);
    }
    fn land(&self, id: &str, view: &Value) -> Value {
        let result = self.ok(&[
            "workflow",
            "integrate",
            id,
            "--inspection",
            text(&view["inspection"]),
        ]);
        let event = result["event"].clone();
        assert!(!event.is_null());
        assert_eq!(event["context_digest"], view["change"]["context_digest"]);
        assert_eq!(event["after"]["revision"], view["context"]["revision"]);
        assert_eq!(event["before"], view["expected_target"]);
        assert_eq!(self.ok(&["workflow", "resume"])["event"], event);
        event
    }
    fn export(&self, mirror: &Path, input: &Path, git_exe: &Path) -> Value {
        self.ok(&[
            "git-export",
            "export",
            "--git",
            git_exe.to_str().unwrap(),
            "--destination",
            mirror.to_str().unwrap(),
            input.to_str().unwrap(),
        ])["export"]
            .clone()
    }
}

fn exact_context(view: &Value, id: &str, criterion: &str, source_check: bool) {
    let context = &view["context"];
    assert_eq!(view["issue"], "https://example.invalid/synthetic");
    assert_eq!(context["version"], 1);
    assert_eq!(context["change"]["id"], id);
    assert_eq!(context["intent"], "Exact isolated synthetic tree");
    assert_eq!(context["acceptance_criteria"], json!([criterion]));
    assert_eq!(context["scope"], json!(["."]));
    assert_eq!(context["target"], "main");
    for field in ["exclusions", "omissions", "dependencies"] {
        assert_eq!(context[field], json!([]));
    }
    for field in ["toolchain", "environment"] {
        assert_eq!(
            context[field],
            json!({"state":"digest","sha256":"1".repeat(64)})
        );
    }
    assert_eq!(context["change_version"], view["change"]["version"]);
    assert_eq!(context["policy_epoch"], view["current_policy_epoch"]);
    assert_eq!(view["expected_target"], view["current_target"]);
    if source_check {
        assert_eq!(context["recipe"]["state"], "digest");
        assert_eq!(
            context["required_capabilities"],
            json!([
                "agentscm.checks.source-paths/v1",
                "agentscm.review-context/v1"
            ])
        );
        assert_eq!(
            context["required_evidence"],
            json!([{"identity":"source_paths/v1","digest":context["recipe"]}])
        );
    } else {
        assert_eq!(
            context["recipe"],
            json!({"state":"digest","sha256":"1".repeat(64)})
        );
        assert_eq!(
            context["required_capabilities"],
            json!(["agentscm.review-context/v1"])
        );
        assert_eq!(context["required_evidence"], json!([]));
    }
}
fn exact_diff(view: &Value, before: &Files, after: &Files) {
    assert_eq!(view["diff"]["complete"], true);
    let expected: BTreeSet<_> = before
        .keys()
        .chain(after.keys())
        .filter(|p| before.get(*p) != after.get(*p))
        .cloned()
        .collect();
    let entries = view["diff"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), expected.len());
    let mut actual = BTreeSet::new();
    for entry in entries {
        let path = text(&entry["path"]);
        assert!(actual.insert(path.to_owned()));
        assert_eq!(
            entry["before_bytes"],
            json!(before.get(path)),
            "before {path}"
        );
        assert_eq!(entry["after_bytes"], json!(after.get(path)), "after {path}");
        assert_eq!(
            entry["before_sha256"],
            json!(before.get(path).map(|b| native_blob_id(b)))
        );
        assert_eq!(
            entry["after_sha256"],
            json!(after.get(path).map(|b| native_blob_id(b)))
        );
    }
    assert_eq!(actual, expected);
}
fn exact_tree(cli: &Cli, mirror: &Path, oid: &str, revision: &str, expected: &Files) {
    let listing = git(&cli.root, mirror, &["ls-tree", "-r", "-z", oid]);
    let mut actual = Files::new();
    for entry in listing.split(|b| *b == 0).filter(|v| !v.is_empty()) {
        let tab = entry.iter().position(|b| *b == b'\t').unwrap();
        let fields: Vec<_> = std::str::from_utf8(&entry[..tab])
            .unwrap()
            .split(' ')
            .collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(&fields[..2], &["100644", "blob"]);
        let path = std::str::from_utf8(&entry[tab + 1..]).unwrap().to_owned();
        let bytes = git(&cli.root, mirror, &["cat-file", "blob", fields[2]]);
        assert!(actual.insert(path.clone(), bytes.clone()).is_none());
        assert_eq!(cli.ok(&["read", revision, &path])["content"], json!(bytes));
    }
    assert_eq!(&actual, expected, "full path/mode/byte/deletion equality");
}
fn integration_request(view: &Value, operation: &str, approval: &str) -> Value {
    json!({"operation_id":operation,"target":view["context"]["target"],"expected_target":view["expected_target"],
        "revision":view["context"]["revision"],"context_digest":view["change"]["context_digest"],
        "approval_id":approval,"change":view["context"]["change"],"change_version":view["context"]["change_version"],
        "policy_epoch":view["context"]["policy_epoch"]})
}
fn export_request(event: &Value, previous: Option<&str>, label: &str) -> Value {
    json!({"event_sequence":event["sequence"],"expected_oid":previous,"metadata":{
        "name":"Synthetic operator","email":"synthetic@example.invalid","timestamp":1780000000u64,"message":label}})
}
fn exact_commit(
    cli: &Cli,
    mirror: &Path,
    status: &Value,
    event: &Value,
    previous: Option<&str>,
    label: &str,
    repository: &Value,
) {
    let record = &status["record"];
    let oid = text(&record["commit"]);
    assert_eq!(record["event"], *event);
    assert_eq!(record["repository"], *repository);
    assert_eq!(record["request"], export_request(event, previous, label));
    assert_eq!(record["profile"], "agentscm-git-snapshot/v1");
    assert_eq!(record["state"], "mirrored");
    assert_eq!(status["ref_matches"], true);
    assert!(status["object_error"].is_null());
    assert_eq!(status["current_oid"], oid);
    let tree = string(git(
        &cli.root,
        mirror,
        &["rev-parse", &format!("{oid}^{{tree}}")],
    ));
    assert_eq!(record["tree"], tree);
    let parent = previous
        .map(|oid| format!("parent {oid}\n"))
        .unwrap_or_default();
    let identity = "Synthetic operator <synthetic@example.invalid> 1780000000 +0000";
    let expected = format!(
        "tree {tree}\n{parent}author {identity}\ncommitter {identity}\n\n{label}\n\nNative-Repository: {}\nNative-Revision: {}\nNative-Event: {}\nSnapshot-Profile: agentscm-git-snapshot/v1\n",
        text(repository),
        text(&event["after"]["revision"]),
        event["sequence"].as_u64().unwrap()
    );
    assert_eq!(
        git(&cli.root, mirror, &["cat-file", "commit", oid]),
        expected.as_bytes()
    );
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

#[test]
fn independent_cli_roundtrip_and_quiesced_recovery() {
    assert!(
        Path::new(GIT).is_file(),
        "this test requires trusted /usr/bin/git"
    );
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let cli = Cli {
        root: root.clone(),
        data: root.join("data"),
    };
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    let initial = Files::from([
        ("src/message.txt".into(), b"captured\n".to_vec()),
        ("docs/obsolete.txt".into(), b"remove later\n".to_vec()),
        ("assets/raw.bin".into(), vec![0, 255, 10, 128]),
        ("empty".into(), Vec::new()),
    ]);
    for (p, b) in &initial {
        write(&source, p, b);
    }
    symlink("src/message.txt", source.join("excluded-link")).unwrap();
    git(&root, &source, &["init", "--initial-branch=main"]);
    git(&root, &source, &["add", "."]);
    git(&root, &source, &["commit", "-m", "Synthetic capture"]);
    let head = string(git(&root, &source, &["rev-parse", "HEAD"]));
    let source_tree = git(&root, &source, &["ls-tree", "-r", "-z", "HEAD"]);
    let status_before = git(
        &root,
        &source,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    );
    assert!(status_before.is_empty());
    let repository = cli.ok(&["init"])["repository"].clone();
    let preflight = [
        "git-snapshot",
        "preflight",
        "--repository",
        source.to_str().unwrap(),
        "--revision",
        &head,
    ];
    // Without the exclusion the in-tree symlink is captured under profile v2.
    let unexcluded = cli.ok(&preflight);
    assert_eq!(unexcluded["preflight"]["eligible"], true);
    assert_eq!(
        unexcluded["preflight"]["profile"],
        "agentscm-git-snapshot/v2"
    );
    let mut eligible = preflight.to_vec();
    eligible.extend(["--exclude", "excluded-link"]);
    let excluded = cli.ok(&eligible);
    assert_eq!(excluded["preflight"]["eligible"], true);
    assert_eq!(excluded["preflight"]["profile"], "agentscm-git-snapshot/v1");
    let receipt = root.join("capture.json");
    let captured = cli.ok(&[
        "git-snapshot",
        "capture",
        "--repository",
        source.to_str().unwrap(),
        "--revision",
        &head,
        "--exclude",
        "excluded-link",
        "--receipt",
        receipt.to_str().unwrap(),
    ]);
    let capture_revision = text(&captured["revision"]);
    assert!(
        fs::read(&receipt)
            .unwrap()
            .windows(b"excluded-link".len())
            .any(|w| w == b"excluded-link")
    );
    assert!(cli.ok(&["target", "main"])["target"].is_null());

    // Bootstrap is explicit administrative initialization, separate from two edits.
    let boot = root.join("bootstrap");
    cli.start("bootstrap", &boot, capture_revision, None);
    cli.ok(&["workflow", "prepare", "bootstrap-prepare"]);
    let bootstrap = cli.inspect();
    exact_diff(&bootstrap, &Files::new(), &initial);
    exact_context(
        &bootstrap,
        "bootstrap",
        "Every supported byte and path matches",
        false,
    );
    assert_eq!(bootstrap["context"]["revision"], capture_revision);
    assert_eq!(bootstrap["evidence"]["controller_status"], "not_requested");
    cli.approve("bootstrap-approval", &bootstrap);
    let initialized = cli.land("bootstrap-initialize", &bootstrap);
    assert_eq!(initialized["kind"], "initialize");

    // Change one: two immutable checkpoints, all stale paths fail before final approval.
    let w1 = root.join("work-one");
    cli.start(
        "edit-one",
        &w1,
        text(&initialized["after"]["revision"]),
        Some(b"first edit\n"),
    );
    write(&w1, "src/message.txt", b"first edit\n");
    let c1 = cli.ok(&["workflow", "checkpoint", "edit-one-checkpoint-one", "0"]);
    assert_eq!(c1["workspace"]["generation"], 1);
    write(&w1, "notes/new.txt", b"checkpoint two\n");
    let c2 = cli.ok(&["workflow", "checkpoint", "edit-one-checkpoint-two", "1"]);
    assert_eq!(c2["workspace"]["generation"], 2);
    assert_ne!(
        c1["workspace"]["base_revision"],
        c2["workspace"]["base_revision"]
    );
    cli.ok(&["workflow", "prepare", "edit-one-prepare"]);
    let missing = cli.inspect();
    assert!(missing["inspection"].is_null());
    cli.fail(&[
        "workflow",
        "approve",
        "missing-check",
        "--inspection",
        &"0".repeat(64),
    ]);
    cli.ok(&[
        "workflow",
        "note",
        "--command",
        "synthetic assertion declaration",
        "--environment",
        "isolated fixture",
        "--result",
        "operator claim only; no project command executed",
    ]);
    let note_only = cli.inspect();
    assert_eq!(
        note_only["evidence"]["controller_status"],
        "not_recorded_in_workflow"
    );
    assert_eq!(note_only["reviewable"], false);
    assert!(note_only["inspection"].is_null());
    cli.fail(&[
        "workflow",
        "approve",
        "operator-note-not-check",
        "--inspection",
        &"0".repeat(64),
    ]);
    let checked = cli.check("edit-one-check-one", b"first edit\n");
    assert_eq!(checked["evidence"]["controller_status"], "success");
    assert_eq!(checked["evidence"]["operator_notes_authenticated"], false);
    assert_eq!(
        checked["evidence"]["operator_notes"][0]["basis"],
        "unverified_operator_claim"
    );
    cli.approve("criteria-old", &checked);
    cli.ok(&[
        "workflow",
        "criteria",
        "--criterion",
        "Exact bytes and no extra path after criteria update",
    ]);
    cli.fail(&[
        "workflow",
        "integrate",
        "criteria-stale",
        "--inspection",
        text(&checked["inspection"]),
    ]);
    let criteria = cli.check("edit-one-check-two", b"first edit\n");
    cli.approve("source-old", &criteria);
    write(&w1, "src/message.txt", b"uncheckpointed drift\n");
    cli.fail(&[
        "workflow",
        "integrate",
        "source-stale",
        "--inspection",
        text(&criteria["inspection"]),
    ]);
    write(&w1, "src/message.txt", b"first edit\n");
    let source_current = cli.inspect();
    cli.approve("policy-old", &source_current);
    let epoch = source_current["current_policy_epoch"]
        .as_u64()
        .unwrap()
        .to_string();
    cli.ok(&["policy", "--advance", &epoch]);
    cli.fail(&[
        "workflow",
        "integrate",
        "policy-stale",
        "--inspection",
        text(&source_current["inspection"]),
    ]);
    cli.ok(&["workflow", "refresh"]);
    cli.ok(&["workflow", "prepare", "edit-one-reprepare"]);
    let one = cli.check("edit-one-check-three", b"first edit\n");
    exact_context(
        &one,
        "edit-one",
        "Exact bytes and no extra path after criteria update",
        true,
    );
    assert_eq!(one["expected_target"], initialized["after"]);
    let mut after_one = initial.clone();
    after_one.insert("src/message.txt".into(), b"first edit\n".to_vec());
    after_one.insert("notes/new.txt".into(), b"checkpoint two\n".to_vec());
    exact_diff(&one, &initial, &after_one);
    cli.approve("edit-one-final", &one);
    let event_one = cli.land("edit-one-integrate", &one);
    assert_eq!(event_one["kind"], "integrate");
    let mirror = root.join("mirror.git");
    cli.ok(&[
        "git-export",
        "init",
        "--git",
        GIT,
        "--destination",
        mirror.to_str().unwrap(),
    ]);
    let export_one = request(
        &root,
        "export-one",
        &export_request(&event_one, None, "First edited tree"),
    );
    let first = cli.export(&mirror, &export_one, Path::new(GIT));
    assert_eq!(first["record"]["state"], "mirrored");
    assert_eq!(first["ref_matches"], true);
    exact_commit(
        &cli,
        &mirror,
        &first,
        &event_one,
        None,
        "First edited tree",
        &repository,
    );
    let oid_one = text(&first["record"]["commit"]).to_owned();
    exact_tree(
        &cli,
        &mirror,
        &oid_one,
        text(&event_one["after"]["revision"]),
        &after_one,
    );
    assert_eq!(
        cli.export(&mirror, &export_one, Path::new(GIT))["record"]["commit"],
        oid_one
    );

    // Change two; a separately approved already-authored checkpoint advances the target.
    let w2 = root.join("work-two");
    cli.start(
        "edit-two",
        &w2,
        text(&event_one["after"]["revision"]),
        Some(b"second edit\n"),
    );
    write(&w2, "src/message.txt", b"second edit\n");
    write(&w2, "assets/raw.bin", &[255, 0, 1, 2, 0]);
    let cp1 = cli.ok(&["workflow", "checkpoint", "edit-two-checkpoint-one", "0"]);
    fs::remove_file(w2.join("docs/obsolete.txt")).unwrap();
    fs::remove_file(w2.join("notes/new.txt")).unwrap();
    write(&w2, "docs/final.txt", b"complete deletion evidence\n");
    let cp2 = cli.ok(&["workflow", "checkpoint", "edit-two-checkpoint-two", "1"]);
    assert_eq!(cp2["workspace"]["generation"], 2);
    assert_ne!(
        cp1["workspace"]["base_revision"],
        cp2["workspace"]["base_revision"]
    );
    cli.ok(&["workflow", "prepare", "edit-two-prepare"]);
    let target_old = cli.check("edit-two-check-one", b"second edit\n");
    cli.approve("target-old", &target_old);
    let mut race_context = target_old["context"].clone();
    race_context["change"]["id"] = json!("checkpoint-target-probe");
    race_context["change_version"] = json!(1);
    race_context["revision"] = cp1["workspace"]["base_revision"].clone();
    race_context["required_capabilities"] = json!(["agentscm.review-context/v1"]);
    race_context["required_evidence"] = json!([]);
    race_context["intent"] = json!(
        "Explicit local administrative target-CAS probe for an already-authored checkpoint; no executed-check claim"
    );
    let race_file = request(&root, "target-probe-context", &race_context);
    let race = cli.ok(&["start", race_file.to_str().unwrap()]);
    let mut race_view = target_old.clone();
    race_view["context"] = race_context;
    race_view["change"] = race["change"].clone();
    let race_request = integration_request(
        &race_view,
        "checkpoint-target-integrate",
        "checkpoint-target-approval",
    );
    let race_approval = request(
        &root,
        "target-probe-approval",
        &json!({"approval_id":race_request["approval_id"],
        "target":race_request["target"],"revision":race_request["revision"],"context_digest":race_request["context_digest"],
        "change":race_request["change"],"change_version":race_request["change_version"],"policy_epoch":race_request["policy_epoch"]}),
    );
    cli.ok(&["approve", race_approval.to_str().unwrap()]);
    let race_input = request(&root, "target-probe-integrate", &race_request);
    let target_probe_event = cli.ok(&["integrate", race_input.to_str().unwrap()])["event"].clone();
    cli.fail(&[
        "workflow",
        "integrate",
        "target-stale",
        "--inspection",
        text(&target_old["inspection"]),
    ]);
    cli.ok(&["workflow", "refresh"]);
    cli.ok(&["workflow", "prepare", "edit-two-reprepare"]);
    let two = cli.check("edit-two-check-two", b"second edit\n");
    exact_context(
        &two,
        "edit-two",
        "Every supported byte and path matches",
        true,
    );
    assert_eq!(two["expected_target"], target_probe_event["after"]);
    let mut checkpoint_one = after_one.clone();
    checkpoint_one.insert("src/message.txt".into(), b"second edit\n".to_vec());
    checkpoint_one.insert("assets/raw.bin".into(), vec![255, 0, 1, 2, 0]);
    let mut after_two = checkpoint_one.clone();
    after_two.remove("docs/obsolete.txt");
    after_two.remove("notes/new.txt");
    after_two.insert(
        "docs/final.txt".into(),
        b"complete deletion evidence\n".to_vec(),
    );
    exact_diff(&two, &checkpoint_one, &after_two);
    cli.approve("edit-two-final", &two);
    let old_request = request(
        &root,
        "restored-old-approval",
        &integration_request(&two, "edit-two-integrate", "edit-two-final"),
    );
    let events_at_backup = cli.ok(&["events", "--after", "0", "--limit", "100"])["events"].clone();
    let workspace_at_backup =
        cli.ok(&["workspace", "state", w2.to_str().unwrap()])["workspace"].clone();
    let receipt_at_backup = fs::read(&receipt).unwrap();
    // All CLI processes are stopped here. Backup records the unconsumed approval.
    let backup = root.join("backup");
    let saved = cli.ok(&[
        "recovery",
        "backup",
        backup.to_str().unwrap(),
        "--quiesced",
        "--workspace",
        boot.to_str().unwrap(),
        "--workspace",
        w1.to_str().unwrap(),
        "--workspace",
        w2.to_str().unwrap(),
        "--attachment",
        mirror.to_str().unwrap(),
        "--attachment",
        receipt.to_str().unwrap(),
    ]);
    cli.ok(&["recovery", "verify", backup.to_str().unwrap()]);
    let event_two = cli.land("edit-two-integrate", &two);
    let events = cli.ok(&["events", "--after", "0", "--limit", "100"])["events"].clone();
    assert_eq!(
        events.as_array().unwrap().len(),
        4,
        "bootstrap, edit one, checkpoint target probe, edit two only"
    );
    let export_two = request(
        &root,
        "export-two",
        &export_request(&event_two, Some(&oid_one), "Second edited tree"),
    );

    // Actual SIGKILL before public Git CAS, not synthetic journal edits or power loss.
    let gate = root.join("pause-export");
    let ready = root.join("export-ready");
    let wrapper = root.join("git-wrapper");
    let cancel = root.join("export-cancel");
    let ack = root.join("export-ack");
    let script = format!(
        "#!/bin/sh\nfor arg do\n if [ \"$arg\" = update-ref ] && [ -f {} ]; then\n  : > {}\n  count=0\n  while [ \"$count\" -lt 100 ]; do\n   if [ -f {} ]; then\n    printf 'cancelled\\n' > {}\n    exit 75\n   fi\n   /bin/sleep 0.1\n   count=$((count + 1))\n  done\n  printf 'deadline\\n' > {}\n  exit 75\n fi\ndone\nexec {} \"$@\"\n",
        quote(&gate),
        quote(&ready),
        quote(&cancel),
        quote(&ack),
        quote(&ack),
        quote(Path::new(GIT))
    );
    fs::write(&wrapper, script).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&gate, b"pause").unwrap();
    let mut export_gate = ExportGate {
        cancel,
        ack,
        acknowledged: false,
    };
    let mut interrupted = OwnedChild::spawn(cli.command(&[
        "git-export",
        "export",
        "--git",
        wrapper.to_str().unwrap(),
        "--destination",
        mirror.to_str().unwrap(),
        export_two.to_str().unwrap(),
    ]));
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() {
        assert!(
            interrupted
                .0
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none(),
            "export exited before CAS pause"
        );
        assert!(Instant::now() < deadline, "Git CAS pause deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(interrupted.interrupt().signal(), Some(libc::SIGKILL));
    assert_eq!(
        export_gate
            .cancel_and_wait()
            .expect("trusted Git wrapper cancellation ACK"),
        "cancelled\n"
    );
    fs::remove_file(&gate).unwrap();
    let sequence_two = event_two["sequence"].as_u64().unwrap().to_string();
    let pending = cli.ok(&[
        "git-export",
        "status",
        "--git",
        GIT,
        "--destination",
        mirror.to_str().unwrap(),
        "--event",
        &sequence_two,
    ]);
    assert_eq!(pending["export"]["record"]["state"], "verified");
    assert_eq!(pending["export"]["current_oid"], oid_one);
    let intended_oid = pending["export"]["record"]["commit"].clone();
    assert!(intended_oid.is_string());
    let second = cli.export(&mirror, &export_two, Path::new(GIT));
    assert_eq!(second["record"]["commit"], intended_oid);
    assert_eq!(second["ref_matches"], true);
    let oid_two = text(&second["record"]["commit"]).to_owned();
    exact_commit(
        &cli,
        &mirror,
        &second,
        &event_two,
        Some(&oid_one),
        "Second edited tree",
        &repository,
    );
    exact_tree(
        &cli,
        &mirror,
        &oid_two,
        text(&event_two["after"]["revision"]),
        &after_two,
    );
    assert_eq!(
        cli.export(&mirror, &export_two, Path::new(GIT))["record"]["commit"],
        oid_two
    );
    assert_eq!(
        cli.ok(&["events", "--after", "0", "--limit", "100"])["events"],
        events
    );
    let mappings = cli.ok(&[
        "git-export",
        "lookup",
        "--git",
        GIT,
        "--destination",
        mirror.to_str().unwrap(),
        "--revision",
        text(&event_two["after"]["revision"]),
    ]);
    assert_eq!(mappings["exports"].as_array().unwrap().len(), 1);
    assert_eq!(mappings["exports"][0], second["record"]);
    // Rewinding an owned test ref makes exact retry fail; it must remain unchanged.
    git(
        &root,
        &mirror,
        &["update-ref", "refs/heads/agentscm", &oid_one, &oid_two],
    );
    cli.fail(&[
        "git-export",
        "export",
        "--git",
        GIT,
        "--destination",
        mirror.to_str().unwrap(),
        export_two.to_str().unwrap(),
    ]);
    assert_eq!(
        string(git(&root, &mirror, &["rev-parse", "refs/heads/agentscm"])),
        oid_one
    );
    git(
        &root,
        &mirror,
        &["update-ref", "refs/heads/agentscm", &oid_two, &oid_one],
    );
    cli.fail(&[
        "git-export",
        "export",
        "--git",
        GIT,
        "--destination",
        mirror.to_str().unwrap(),
        export_one.to_str().unwrap(),
    ]);
    assert_eq!(
        string(git(&root, &mirror, &["rev-parse", "refs/heads/agentscm"])),
        oid_two
    );

    // Source checkout and all supported contents remain unchanged throughout.
    assert_eq!(string(git(&root, &source, &["rev-parse", "HEAD"])), head);
    assert_eq!(
        git(&root, &source, &["ls-tree", "-r", "-z", "HEAD"]),
        source_tree
    );
    assert_eq!(
        git(
            &root,
            &source,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        ),
        status_before
    );
    for (path, bytes) in &initial {
        assert_eq!(fs::read(source.join(path)).unwrap(), *bytes);
    }
    assert_eq!(
        fs::read_link(source.join("excluded-link")).unwrap(),
        Path::new("src/message.txt")
    );

    // No more processes open original native state; retire all old native paths.
    let restore = root.join("restore");
    assert_eq!(
        cli.ok(&[
            "recovery",
            "restore",
            backup.to_str().unwrap(),
            restore.to_str().unwrap()
        ])["quarantined"],
        true
    );
    for (index, old) in [&cli.data, &boot, &w1, &w2].into_iter().enumerate() {
        let retired = root.join(format!("retired-native-{index}"));
        assert!(!retired.exists());
        fs::rename(old, retired).unwrap();
        assert!(!old.exists());
    }
    // This is sole-operator old-path retirement, not hostile-writer fencing.
    let mapped = |original: &Path| -> PathBuf {
        let roots = saved["manifest"]["roots"].as_array().unwrap();
        let r = roots
            .iter()
            .find(|r| Path::new(text(&r["original"])) == original)
            .unwrap();
        restore.join(text(&r["destination"]))
    };
    let restored = Cli {
        root: root.clone(),
        data: mapped(&cli.data),
    };
    restored.fail(&["target", "main"]);
    restored.fail(&["recovery", "activate", restore.to_str().unwrap()]);
    let activated = restored.ok(&[
        "recovery",
        "activate",
        restore.to_str().unwrap(),
        "--old-writer-disabled",
        "--external-state-reconciled",
    ]);
    // Revocation is reported; replay rejection below independently checks the fence.
    assert_eq!(activated["approvals_revoked"], true);
    assert!(
        activated["policy_epoch"].as_u64().unwrap()
            > saved["manifest"]["policy_epoch"].as_u64().unwrap()
    );
    restored.fail(&["integrate", old_request.to_str().unwrap()]);
    let retained_events = restored.ok(&["events", "--after", "0", "--limit", "100"]);
    assert_eq!(retained_events["events"], events_at_backup);
    assert_eq!(retained_events["events"].as_array().unwrap().len(), 3);
    assert!(
        retained_events["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["operation_id"] != "edit-two-integrate")
    );
    assert_eq!(
        restored.ok(&["target", "main"])["target"],
        two["expected_target"]
    );
    let retained = restored.ok(&["workflow", "status"]);
    assert_eq!(retained["context"], two["context"]);
    assert_eq!(retained["change"], two["change"]);
    let restored_workspace = mapped(&w2);
    assert_eq!(
        restored.ok(&["workspace", "state", restored_workspace.to_str().unwrap()])["workspace"],
        workspace_at_backup
    );
    for (path, bytes) in &after_two {
        assert_eq!(fs::read(restored_workspace.join(path)).unwrap(), *bytes);
        assert_eq!(
            restored.ok(&["read", text(&two["context"]["revision"]), path])["content"],
            json!(bytes)
        );
    }
    assert!(!restored_workspace.join("docs/obsolete.txt").exists());
    assert!(!restored_workspace.join("notes/new.txt").exists());
    assert_eq!(fs::read(mapped(&receipt)).unwrap(), receipt_at_backup);
    let copied_mirror = mapped(&mirror);
    assert_eq!(
        string(git(
            &root,
            &copied_mirror,
            &["rev-parse", "refs/heads/agentscm"]
        )),
        oid_one
    );
    exact_tree(
        &restored,
        &copied_mirror,
        &oid_one,
        text(&event_one["after"]["revision"]),
        &after_one,
    );
    restored.fail(&[
        "workflow",
        "integrate",
        "restored-token",
        "--inspection",
        text(&two["inspection"]),
    ]);
    restored.fail(&["workflow", "resume"]);
    let reattached = restored.ok(&[
        "workflow",
        "resume",
        "--workspace",
        restored_workspace.to_str().unwrap(),
    ]);
    assert!(reattached["inspection"].is_null());
    assert_eq!(reattached["approval_recorded"], false);
    let reattached_status = restored.ok(&["workflow", "status"]);
    assert_eq!(
        reattached_status["workspace_path"],
        json!(restored_workspace)
    );
    assert_eq!(reattached_status["restored_reattachment_required"], false);
    restored.fail(&[
        "workflow",
        "integrate",
        "reattached-old-token",
        "--inspection",
        text(&two["inspection"]),
    ]);
    restored.fail(&[
        "git-export",
        "export",
        "--git",
        GIT,
        "--destination",
        mirror.to_str().unwrap(),
        export_one.to_str().unwrap(),
    ]);
    assert_eq!(
        string(git(&root, &mirror, &["rev-parse", "refs/heads/agentscm"])),
        oid_two
    );
    eprintln!(
        "SYNTHETIC_ROUNDTRIP capture={capture_revision} first_native={} second_native={} first_git={oid_one} second_git={oid_two}; CAS-process-interruption+retry observed; no powerloss or real-shadow claim",
        event_one["after"]["revision"], event_two["after"]["revision"]
    );
}
