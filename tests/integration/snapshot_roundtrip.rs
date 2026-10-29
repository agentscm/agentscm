#![cfg(unix)]
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agentscm::authority::Authority;
use agentscm::store::Engine;
use serde_json::{Value, json};
use tempfile::TempDir;

fn git_executable() -> PathBuf {
    std::env::var_os("AGENTSCM_TEST_GIT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/git"))
}
fn git(directory: &Path, args: &[&str]) -> Vec<u8> {
    let result = Command::new(git_executable())
        .current_dir(directory)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Synthetic test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Synthetic test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_AUTHOR_DATE", "1780000000 +0000")
        .env("GIT_COMMITTER_DATE", "1780000000 +0000")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    result.stdout
}
fn run(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("PATH", "")
        .output()
        .unwrap()
}
fn ok(data: &Path, args: &[&str]) -> Value {
    let out = run(data, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn fail(data: &Path, args: &[&str]) {
    fail_at("command rejection", data, args);
}
fn fail_at(phase: &str, data: &Path, args: &[&str]) -> Value {
    let out = run(data, args);
    assert!(
        !out.status.success(),
        "{phase}: unexpected success {args:?}: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    serde_json::from_slice(&out.stderr).unwrap()
}
fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}
fn start(data: &Path, work: &Path, change: &str, revision: &str) {
    ok(
        data,
        &[
            "workflow",
            "start",
            "--change",
            change,
            "--workspace",
            text(work),
            "--revision",
            revision,
            "--issue",
            "https://example.invalid/issues/25",
            "--intent",
            "Synthetic round-trip maintenance",
            "--criterion",
            "Preserve all supported bytes and intentional deletion",
            "--scope",
            "a.txt",
            "--scope",
            "gone.txt",
            "--scope",
            "binary.dat",
            "--recipe",
            &"a".repeat(64),
            "--toolchain",
            &"b".repeat(64),
            "--environment",
            &"c".repeat(64),
        ],
    );
}
fn inspection(data: &Path) -> String {
    let view = ok(data, &["workflow", "inspect"]);
    assert_eq!(view["diff"]["complete"], true);
    assert_eq!(view["reviewable"], true);
    view["inspection"].as_str().unwrap().to_owned()
}
fn export(data: &Path, mirror: &Path, input: &Path) -> Value {
    ok(
        data,
        &[
            "git-export",
            "export",
            "--git",
            text(&git_executable()),
            "--destination",
            text(mirror),
            text(input),
        ],
    )
}
fn export_request(path: &Path, event: &Value, previous: Option<&str>) {
    fs::write(path,serde_json::to_vec(&json!({"event_sequence":event["sequence"],"expected_oid":previous,"metadata":{"name":"Synthetic roundtrip","email":"test@example.invalid","timestamp":1780000000u64,"message":format!("Native event {}",event["sequence"])}})).unwrap()).unwrap();
}
fn verify_full_tree(data: &Path, revision: &str, mirror: &Path, commit: &str) {
    let engine = Engine::open(data).unwrap();
    let mut native = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = engine
            .list_files(revision, cursor.as_deref(), 4096)
            .unwrap();
        for file in page.entries {
            native.insert(
                file.path.clone(),
                engine.read_file(revision, &file.path).unwrap(),
            );
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let listing = git(mirror, &["ls-tree", "-rz", commit]);
    let mut exported = BTreeMap::new();
    for row in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let row = std::str::from_utf8(row).unwrap();
        let (metadata, path) = row.split_once('\t').unwrap();
        assert!(metadata.starts_with("100644 blob "));
        let oid = metadata.split_whitespace().nth(2).unwrap();
        exported.insert(path.to_owned(), git(mirror, &["cat-file", "blob", oid]));
    }
    assert_eq!(native, exported);
}

#[test]
fn synthetic_git_native_review_git_recovery_roundtrip() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "--initial-branch=source", "--template="]);
    fs::create_dir_all(source.join("shared/nested")).unwrap();
    fs::write(source.join("a.txt"), b"initial\n").unwrap();
    fs::write(source.join("gone.txt"), b"intentional future deletion\n").unwrap();
    fs::write(source.join("shared/left.txt"), b"left sibling\n").unwrap();
    fs::write(source.join("shared/right.txt"), b"right sibling\n").unwrap();
    fs::write(source.join("shared/nested/binary.dat"), [0, 255, 13, 10]).unwrap();
    fs::write(source.join("shared/nested/empty.txt"), b"").unwrap();
    std::os::unix::fs::symlink("a.txt", source.join("unsupported-link")).unwrap();
    git(&source, &["add", "--all"]);
    git(&source, &["commit", "-m", "Synthetic snapshot source"]);
    let head = String::from_utf8(git(&source, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    let index = fs::read(source.join(".git/index")).unwrap();
    let status = git(
        &source,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    );
    let original_a = fs::read(source.join("a.txt")).unwrap();
    let original_gone = fs::read(source.join("gone.txt")).unwrap();
    let data = temp.path().join("data");
    ok(&data, &["init"]);
    let excluded = ok(
        &data,
        &[
            "git-snapshot",
            "preflight",
            "--git",
            text(&git_executable()),
            "--repository",
            text(&source),
            "--revision",
            &head,
        ],
    );
    assert_eq!(excluded["preflight"]["eligible"], true);
    assert_eq!(excluded["preflight"]["profile"], "agentscm-git-snapshot/v2");
    let receipt = temp.path().join("capture.json");
    let capture = ok(
        &data,
        &[
            "git-snapshot",
            "capture",
            "--git",
            text(&git_executable()),
            "--repository",
            text(&source),
            "--revision",
            &head,
            "--exclude",
            "unsupported-link",
            "--receipt",
            text(&receipt),
        ],
    );
    let revision = capture["revision"].as_str().unwrap();
    let saved: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(saved["excluded"][0]["path"], "unsupported-link");
    let work = temp.path().join("work1");
    start(&data, &work, "first", revision);
    fs::write(work.join("a.txt"), b"first maintenance\n").unwrap();
    ok(&data, &["workflow", "checkpoint", "first-1", "0"]);
    fs::write(work.join("binary.dat"), [0, 255, 13, 10]).unwrap();
    ok(&data, &["workflow", "checkpoint", "first-2", "1"]);
    ok(&data, &["workflow", "prepare", "prepare-first"]);
    let token = inspection(&data);
    ok(
        &data,
        &["workflow", "approve", "first-old", "--inspection", &token],
    );
    ok(
        &data,
        &[
            "workflow",
            "criteria",
            "--criterion",
            "Updated acceptance, identical source",
        ],
    );
    fail(
        &data,
        &[
            "workflow",
            "integrate",
            "stale-first",
            "--inspection",
            &token,
        ],
    );
    let token = inspection(&data);
    ok(
        &data,
        &[
            "workflow",
            "approve",
            "first-current",
            "--inspection",
            &token,
        ],
    );
    let first = ok(
        &data,
        &[
            "workflow",
            "integrate",
            "land-first",
            "--inspection",
            &token,
        ],
    )["event"]
        .clone();
    assert_eq!(ok(&data, &["workflow", "resume"])["event"], first);
    let mirror = temp.path().join("mirror.git");
    ok(
        &data,
        &[
            "git-export",
            "init",
            "--git",
            text(&git_executable()),
            "--destination",
            text(&mirror),
            "--seed-repository",
            text(&source),
            "--seed-commit",
            &head,
        ],
    );
    let request1 = temp.path().join("export1.json");
    export_request(&request1, &first, Some(&head));
    let one = export(&data, &mirror, &request1);
    let first_commit = one["export"]["record"]["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(export(&data, &mirror, &request1), one);
    assert_eq!(
        String::from_utf8(git(&mirror, &["rev-parse", &format!("{first_commit}^")]))
            .unwrap()
            .trim(),
        head
    );
    verify_full_tree(
        &data,
        first["after"]["revision"].as_str().unwrap(),
        &mirror,
        &first_commit,
    );
    let native_before = {
        let authority = Authority::open_read_only(&data).unwrap();
        (
            authority.events(0, 100).unwrap(),
            authority.target("main").unwrap(),
        )
    };
    let check_input = temp.path().join("external-check.json");
    let mut check = json!({
        "event_sequence":first["sequence"], "result_id":"local-cli-check",
        "tested_oid":"0".repeat(40), "outcome":"passed",
        "details":"Synthetic local declaration; no native admission"
    });
    fs::write(&check_input, serde_json::to_vec(&check).unwrap()).unwrap();
    let check_git = git_executable();
    let check_args = [
        "git-export",
        "record-check",
        "--git",
        text(&check_git),
        "--destination",
        text(&mirror),
        text(&check_input),
    ];
    fail(&data, &check_args);
    check["tested_oid"] = first_commit.clone().into();
    fs::write(&check_input, serde_json::to_vec(&check).unwrap()).unwrap();
    let recorded = ok(&data, &check_args);
    assert_eq!(recorded["check"]["provenance"], "local_operator_unverified");
    assert_eq!(recorded["check"]["tested_oid"], first_commit);
    assert_eq!(ok(&data, &check_args), recorded);
    let checks = ok(
        &data,
        &[
            "git-export",
            "checks",
            "--git",
            text(&git_executable()),
            "--destination",
            text(&mirror),
        ],
    );
    assert_eq!(checks["checks"], json!([recorded["check"].clone()]));
    {
        let authority = Authority::open_read_only(&data).unwrap();
        assert_eq!(authority.events(0, 100).unwrap(), native_before.0);
        assert_eq!(authority.target("main").unwrap(), native_before.1);
        assert!(authority.check_attempt("local-cli-check").is_err());
    }
    let work2 = temp.path().join("work2");
    start(
        &data,
        &work2,
        "second",
        first["after"]["revision"].as_str().unwrap(),
    );
    fs::write(work2.join("a.txt"), b"second maintenance\n").unwrap();
    ok(&data, &["workflow", "checkpoint", "second-1", "0"]);
    fs::remove_file(work2.join("gone.txt")).unwrap();
    ok(&data, &["workflow", "checkpoint", "second-2", "1"]);
    ok(&data, &["workflow", "prepare", "prepare-second"]);
    let token = inspection(&data);
    ok(
        &data,
        &["workflow", "approve", "second", "--inspection", &token],
    );
    let second = ok(
        &data,
        &[
            "workflow",
            "integrate",
            "land-second",
            "--inspection",
            &token,
        ],
    )["event"]
        .clone();
    let request2 = temp.path().join("export2.json");
    export_request(&request2, &second, Some(&first_commit));
    let two = export(&data, &mirror, &request2);
    let second_commit = two["export"]["record"]["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    // A declaration remains bound to its historical mapping after advancement.
    assert_eq!(ok(&data, &check_args), recorded);
    verify_full_tree(
        &data,
        second["after"]["revision"].as_str().unwrap(),
        &mirror,
        &second_commit,
    );
    assert!(
        !String::from_utf8(git(
            &mirror,
            &["ls-tree", "-r", "--name-only", &second_commit]
        ))
        .unwrap()
        .lines()
        .any(|p| p == "gone.txt")
    );
    // Inject a lost export reply at the retained verified state. This is not a
    // killed-process or power-loss durability test.
    {
        let db = rusqlite::Connection::open(mirror.join("agentscm-export.sqlite")).unwrap();
        let mut record = two["export"]["record"].clone();
        record["state"] = "verified".into();
        db.execute(
            "UPDATE exports SET record=?1 WHERE sequence=?2",
            rusqlite::params![
                serde_json::to_vec(&record).unwrap(),
                second["sequence"].as_u64().unwrap()
            ],
        )
        .unwrap();
    }
    assert_eq!(
        export(&data, &mirror, &request2)["export"]["record"]["commit"],
        second_commit
    );
    let backup = temp.path().join("backup");
    ok(
        &data,
        &[
            "recovery",
            "backup",
            text(&backup),
            "--quiesced",
            "--workspace",
            text(&work),
            "--workspace",
            text(&work2),
            "--attachment",
            text(&receipt),
            "--attachment",
            text(&mirror),
        ],
    );
    ok(&data, &["recovery", "verify", text(&backup)]);
    let restored = temp.path().join("restored");
    ok(
        &data,
        &["recovery", "restore", text(&backup), text(&restored)],
    );
    let restored_data = restored.join("data");
    fail_at(
        "completed restore before activation",
        &restored_data,
        &["workflow", "resume"],
    );
    ok(
        &data,
        &[
            "recovery",
            "activate",
            text(&restored),
            "--old-writer-disabled",
            "--external-state-reconciled",
        ],
    );
    // A completed operation returns only its historical receipt. Prove that
    // this path works while the original workspace cannot be opened.
    let unavailable = temp.path().join("work2-unavailable");
    fs::rename(&work2, &unavailable).unwrap();
    let retained = ok(&restored_data, &["workflow", "resume"]);
    assert_eq!(retained["event"], second);
    assert_eq!(
        retained["workspace_status"]["state"],
        "not_read_for_completed_operation"
    );
    assert!(!work2.exists());
    fs::rename(&unavailable, &work2).unwrap();
    // An active prepared/approved operation has a different contract: no
    // original workspace access until explicit matching reattachment, and no
    // authority to use an approval retained from the old policy epoch.
    let pending_work = temp.path().join("work-pending");
    start(
        &restored_data,
        &pending_work,
        "pending",
        second["after"]["revision"].as_str().unwrap(),
    );
    fs::write(pending_work.join("a.txt"), b"pending maintenance\n").unwrap();
    ok(
        &restored_data,
        &["workflow", "checkpoint", "pending-1", "0"],
    );
    ok(&restored_data, &["workflow", "prepare", "prepare-pending"]);
    let pending_token = inspection(&restored_data);
    ok(
        &restored_data,
        &[
            "workflow",
            "approve",
            "pending-old",
            "--inspection",
            &pending_token,
        ],
    );
    let pending_backup = temp.path().join("pending-backup");
    ok(
        &restored_data,
        &[
            "recovery",
            "backup",
            text(&pending_backup),
            "--quiesced",
            "--workspace",
            text(&pending_work),
        ],
    );
    let pending_restore = temp.path().join("pending-restored");
    ok(
        &restored_data,
        &[
            "recovery",
            "restore",
            text(&pending_backup),
            text(&pending_restore),
        ],
    );
    let pending_data = pending_restore.join("data");
    fail_at(
        "active restore before activation",
        &pending_data,
        &["workflow", "resume"],
    );
    ok(
        &restored_data,
        &[
            "recovery",
            "activate",
            text(&pending_restore),
            "--old-writer-disabled",
            "--external-state-reconciled",
        ],
    );
    let events_before = Authority::open_read_only(&pending_data)
        .unwrap()
        .events(0, 100)
        .unwrap();
    let before = ok(&pending_data, &["workflow", "status"]);
    assert_eq!(before["restored_reattachment_required"], true);
    assert_eq!(before["workspace_status"]["state"], "not_read_by_status");
    let denial = fail_at(
        "active restore with original workspace present",
        &pending_data,
        &["workflow", "resume"],
    );
    assert!(
        denial["error"]
            .as_str()
            .unwrap()
            .contains("restored workflow requires explicit workflow resume --workspace PATH"),
        "wrong recovery gate: {denial}"
    );
    let unavailable_pending = temp.path().join("pending-unavailable");
    fs::rename(&pending_work, &unavailable_pending).unwrap();
    fail_at(
        "active restore before explicit reattachment",
        &pending_data,
        &["workflow", "resume"],
    );
    assert!(!pending_work.exists());
    let reattached = ok(
        &pending_data,
        &[
            "workflow",
            "resume",
            "--workspace",
            text(&pending_restore.join("workspaces/0")),
        ],
    );
    assert_eq!(reattached["approval_recorded"], false);
    fail_at(
        "restored stale approval after explicit reattachment",
        &pending_data,
        &[
            "workflow",
            "integrate",
            "land-pending-old",
            "--inspection",
            &pending_token,
        ],
    );
    let after = ok(&pending_data, &["workflow", "resume"]);
    assert_eq!(after["current_target"], second["after"]);
    assert_eq!(after["event"], Value::Null);
    assert_eq!(after["approval_recorded"], false);
    assert_eq!(
        Authority::open_read_only(&pending_data)
            .unwrap()
            .events(0, 100)
            .unwrap(),
        events_before
    );
    fs::rename(&unavailable_pending, &pending_work).unwrap();
    // The historical mirror attachment is byte evidence. Ownership stays bound
    // to the original external mirror path; its current ref is observed anew.
    git(
        &mirror,
        &[
            "update-ref",
            "refs/heads/agentscm",
            &first_commit,
            &second_commit,
        ],
    );
    fail(
        &pending_data,
        &[
            "git-export",
            "export",
            "--git",
            text(&git_executable()),
            "--destination",
            text(&mirror),
            text(&request2),
        ],
    );
    assert_eq!(
        String::from_utf8(git(&mirror, &["rev-parse", "refs/heads/agentscm"]))
            .unwrap()
            .trim(),
        first_commit
    );
    assert_eq!(
        String::from_utf8(git(&source, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim(),
        head
    );
    assert_eq!(
        git(
            &source,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        ),
        status
    );
    assert_eq!(fs::read(source.join(".git/index")).unwrap(), index);
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), original_a);
    assert_eq!(fs::read(source.join("gone.txt")).unwrap(), original_gone);
    assert_eq!(
        fs::read_link(source.join("unsupported-link")).unwrap(),
        PathBuf::from("a.txt")
    );
}
