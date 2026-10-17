#![cfg(unix)]
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use agentscm::context::ChangeId;
use agentscm::git_snapshot::{
    PROFILE, PROFILE_V2, SnapshotRequest, UnsupportedKind, capture, preflight, write_receipt,
};
use agentscm::model::{EntryKind, FileChange};
use agentscm::store::Engine;
use agentscm::sync::{SOURCE_PROFILE_V3, import_bundle, write_bundle, write_offer, write_request};
use agentscm::workspace::{OperationOutcome, Workspace, WorkspaceChange};
use tempfile::TempDir;

fn git_exe() -> PathBuf {
    PathBuf::from(std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".to_owned()))
}

fn run(dir: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    let mut child = Command::new(git_exe())
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
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.unwrap_or_default())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn request(dir: &Path, head: &str) -> SnapshotRequest {
    SnapshotRequest {
        git: git_exe(),
        repository: dir.to_path_buf(),
        revision: head.to_owned(),
        exclude: Vec::new(),
    }
}

fn commit_all(dir: &Path) -> String {
    run(dir, &["add", "."], None);
    run(dir, &["commit", "-q", "-m", "snapshot"], None);
    run(dir, &["rev-parse", "HEAD"], None)
}

fn kinds_repo() -> (TempDir, String) {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    run(p, &["init", "-q"], None);
    fs::create_dir_all(p.join("dir")).unwrap();
    fs::write(p.join("dir/file"), "data\n").unwrap();
    fs::write(p.join("run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(p.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("dir/file", p.join("link")).unwrap();
    symlink("dir", p.join("dirlink")).unwrap();
    let head = commit_all(p);
    (dir, head)
}

#[test]
fn kinds_round_trip_through_capture_workspace_checkpoint_and_sync() {
    let (repo, head) = kinds_repo();
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path().join("store")).unwrap();
    let req = request(repo.path(), &head);
    let report = preflight(&req).unwrap();
    assert!(report.eligible, "{:?}", report.unsupported);
    assert_eq!(report.profile, PROFILE_V2);
    let snapshot = capture(&engine, "repo-1", &req, report).unwrap();
    assert_eq!(snapshot.receipt.profile, PROFILE_V2);

    let receipt_path = data.path().join("receipt.json");
    write_receipt(&receipt_path, &snapshot.receipt).unwrap();
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(receipt["profile"], "agentscm-git-snapshot/v2");
    let modes: Vec<(&str, &str)> = receipt["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["path"].as_str().unwrap(), e["mode"].as_str().unwrap()))
        .collect();
    assert_eq!(
        modes,
        [
            ("dir/file", "100644"),
            ("dirlink", "120000"),
            ("link", "120000"),
            ("run.sh", "100755")
        ]
    );

    let revision = snapshot.revision;
    let expect = [
        ("dir/file", EntryKind::Regular, "data\n"),
        ("dirlink", EntryKind::Symlink, "dir"),
        ("link", EntryKind::Symlink, "dir/file"),
        ("run.sh", EntryKind::Executable, "#!/bin/sh\n"),
    ];
    for (path, kind, content) in expect {
        assert_eq!(
            engine.read_entry(&revision, path).unwrap(),
            (kind, content.as_bytes().to_vec()),
            "{path}"
        );
    }

    // Materialize: exec bit and symlinks are present and the status is clean.
    let root = data.path().join("workspace");
    let change = ChangeId {
        repository: "repo-1".to_owned(),
        id: "change-1".to_owned(),
    };
    let mut workspace = Workspace::create(&engine, &root, change, &revision).unwrap();
    assert_eq!(
        fs::symlink_metadata(root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100,
        0o100
    );
    assert_eq!(
        fs::read_link(root.join("link")).unwrap(),
        Path::new("dir/file")
    );
    assert_eq!(
        fs::read_link(root.join("dirlink")).unwrap(),
        Path::new("dir")
    );
    assert!(workspace.status(&engine, 100).unwrap().changes.is_empty());

    // Drop the exec bit and retarget a link.
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(root.join("link")).unwrap();
    symlink("dirlink", root.join("link")).unwrap();
    let changes = workspace.status(&engine, 100).unwrap().changes;
    assert!(changes.contains(&WorkspaceChange::KindChanged {
        path: "run.sh".into(),
        before: EntryKind::Executable,
        after: EntryKind::Regular,
    }));
    let OperationOutcome::Checkpointed {
        revision: edited, ..
    } = workspace.checkpoint(&engine, "op-1", 0).unwrap()
    else {
        panic!("checkpoint did not publish");
    };
    let expect = [
        ("dir/file", EntryKind::Regular, "data\n"),
        ("dirlink", EntryKind::Symlink, "dir"),
        ("link", EntryKind::Symlink, "dirlink"),
        ("run.sh", EntryKind::Regular, "#!/bin/sh\n"),
    ];
    for (path, kind, content) in expect {
        assert_eq!(
            engine.read_entry(&edited, path).unwrap(),
            (kind, content.as_bytes().to_vec()),
            "{path}"
        );
    }
    assert!(workspace.status(&engine, 100).unwrap().changes.is_empty());

    // Sync into a second store: v3 profile, identical kinds.
    let receiver = Engine::open(data.path().join("receiver")).unwrap();
    let files = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let offer_path = files.path().join("offer.json");
    let request_path = files.path().join("request.json");
    let bundle_path = files.path().join("bundle");
    let offer = write_offer(
        &engine,
        "source",
        std::slice::from_ref(&edited),
        &offer_path,
    )
    .unwrap();
    assert_eq!(offer.profile, SOURCE_PROFILE_V3);
    write_request(&receiver, &offer_path, &request_path).unwrap();
    write_bundle(&engine, &offer_path, &request_path, &bundle_path).unwrap();
    import_bundle(&receiver, &bundle_path, state.path()).unwrap();
    for (path, _, _) in expect {
        assert_eq!(
            receiver.read_entry(&edited, path).unwrap(),
            engine.read_entry(&edited, path).unwrap(),
            "{path}"
        );
    }
}

#[test]
fn hostile_git_symlink_targets_are_reported_and_never_captured() {
    for (name, target) in [
        ("absolute", "/etc/passwd"),
        ("escape", "../../outside"),
        ("git-dir", ".git/config"),
        ("nested-git", "sub/.git/hooks"),
        ("backslash", "a\\b"),
        ("empty", ""),
    ] {
        let (repo, _) = kinds_repo();
        let p = repo.path();
        let oid = run(
            p,
            &["hash-object", "-w", "--stdin"],
            Some(target.as_bytes()),
        );
        run(
            p,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("120000,{oid},bad/link"),
            ],
            None,
        );
        run(p, &["commit", "-q", "-m", "hostile"], None);
        let head = run(p, &["rev-parse", "HEAD"], None);
        let report = preflight(&request(p, &head)).unwrap();
        assert!(!report.eligible, "{name}");
        assert!(
            report
                .unsupported
                .iter()
                .any(|u| u.kind == UnsupportedKind::SymlinkTarget && u.path == "bad/link"),
            "{name}: {:?}",
            report.unsupported
        );
        assert!(
            report.entries.iter().all(|e| e.path != "bad/link"),
            "{name}"
        );
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        assert!(capture(&engine, "repo-1", &request(p, &head), report).is_err());
    }
}

#[test]
fn gitlinks_stay_unsupported_and_regular_trees_keep_profile_v1_and_identity() {
    let (repo, _) = kinds_repo();
    let p = repo.path();
    let head_oid = run(p, &["rev-parse", "HEAD"], None);
    run(
        p,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head_oid},sub"),
        ],
        None,
    );
    run(p, &["commit", "-q", "-m", "gitlink"], None);
    let head = run(p, &["rev-parse", "HEAD"], None);
    let report = preflight(&request(p, &head)).unwrap();
    assert!(!report.eligible);
    assert!(
        report
            .unsupported
            .iter()
            .any(|u| u.kind == UnsupportedKind::Gitlink)
    );

    let regular = TempDir::new().unwrap();
    let r = regular.path();
    run(r, &["init", "-q"], None);
    fs::create_dir_all(r.join("src")).unwrap();
    fs::write(r.join("README.md"), "hello\n").unwrap();
    fs::write(r.join("src/lib.rs"), "fn lib() {}\n").unwrap();
    let head = commit_all(r);
    let req = request(r, &head);
    let report = preflight(&req).unwrap();
    assert_eq!(report.profile, PROFILE);
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let snapshot = capture(&engine, "repo-1", &req, report).unwrap();
    assert_eq!(snapshot.receipt.profile, PROFILE);
    let plain_dir = TempDir::new().unwrap();
    let plain = Engine::open(plain_dir.path())
        .unwrap()
        .checkpoint(
            None,
            &[
                FileChange {
                    path: "README.md".into(),
                    content: Some(b"hello\n".to_vec()),
                },
                FileChange {
                    path: "src/lib.rs".into(),
                    content: Some(b"fn lib() {}\n".to_vec()),
                },
            ],
        )
        .unwrap();
    assert_eq!(snapshot.revision, plain);
    let bytes = String::from_utf8(snapshot.receipt.to_bytes().unwrap()).unwrap();
    assert!(!bytes.contains("agentscm-git-snapshot/v2"));
}
