#![cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentscm::git_snapshot::{
    LimitKind, ManifestEntry, PROFILE, PROFILE_V2, Preflight, SnapshotRequest, UnsupportedKind,
    preflight, read_blobs,
};
use tempfile::TempDir;

fn git() -> PathBuf {
    let path = PathBuf::from(
        std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".to_owned()),
    );
    assert!(
        path.exists(),
        "git not found at {}; set AGENTSCM_TEST_GIT to an absolute git path",
        path.display()
    );
    path
}

fn run(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(git())
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
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    run(dir.path(), &["init", "-q"]);
    dir
}

fn commit(dir: &Path, message: &str) -> String {
    run(dir, &["commit", "-q", "-m", message]);
    run(dir, &["rev-parse", "HEAD"])
}

fn request(dir: &Path, revision: &str, exclude: &[&str]) -> SnapshotRequest {
    SnapshotRequest {
        git: git(),
        repository: dir.to_path_buf(),
        revision: revision.to_owned(),
        exclude: exclude.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn blob(dir: &Path, content: &str) -> String {
    write(dir, ".blob-source", content);
    let oid = run(dir, &["hash-object", "-w", ".blob-source"]);
    fs::remove_file(dir.join(".blob-source")).unwrap();
    oid
}

fn kinds(report: &Preflight) -> Vec<UnsupportedKind> {
    report.unsupported.iter().map(|u| u.kind).collect()
}

fn base() -> (TempDir, String) {
    let dir = repo();
    write(dir.path(), "README.md", "hello\n");
    write(dir.path(), "src/a/b.rs", "fn b() {}\n");
    write(dir.path(), "src/c.rs", "fn c() {}\n");
    run(dir.path(), &["add", "."]);
    let head = commit(dir.path(), "one");
    (dir, head)
}

#[test]
fn clean_tree_is_eligible_and_blobs_round_trip() {
    let (dir, head) = base();
    let req = request(dir.path(), &head, &[]);
    let report = preflight(&req).unwrap();
    assert_eq!(report.profile, PROFILE);
    assert!(report.eligible, "{:?}", report.unsupported);
    assert!(report.cleanliness.clean);
    let listed = run(dir.path(), &["ls-tree", "-r", "--name-only", &head]);
    let paths: Vec<&str> = report.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, listed.lines().collect::<Vec<_>>());
    assert_eq!(report.totals.files, 3);
    assert_eq!(report.totals.directories, 2);
    let blobs = read_blobs(&req, &report.entries).unwrap();
    for (entry, bytes) in report.entries.iter().zip(&blobs) {
        assert_eq!(bytes, &fs::read(dir.path().join(&entry.path)).unwrap());
    }
    assert_eq!(
        report.manifest_sha256,
        preflight(&req).unwrap().manifest_sha256
    );
    assert_eq!(report.manifest_sha256.len(), 64);
}

#[test]
fn read_blobs_rejects_wrong_size_and_missing_objects() {
    let (dir, head) = base();
    let req = request(dir.path(), &head, &[]);
    let mut entries = preflight(&req).unwrap().entries;
    let mut wrong = entries.clone();
    wrong[0].size += 1;
    assert!(read_blobs(&req, &wrong).is_err());
    entries[0].oid = "1".repeat(40);
    assert!(read_blobs(&req, &entries).is_err());
}

#[test]
fn symlink_is_captured_unless_declared_excluded() {
    let (dir, _) = base();
    std::os::unix::fs::symlink("README.md", dir.path().join("link")).unwrap();
    run(dir.path(), &["add", "link"]);
    let head = commit(dir.path(), "link");
    let report = preflight(&request(dir.path(), &head, &[])).unwrap();
    assert!(report.eligible, "{:?}", report.unsupported);
    assert_eq!(report.profile, PROFILE_V2);
    let link = report.entries.iter().find(|e| e.path == "link").unwrap();
    assert_eq!((link.mode.as_str(), link.size), ("120000", 9));
    let report = preflight(&request(dir.path(), &head, &["link"])).unwrap();
    assert!(report.eligible);
    assert_eq!(report.profile, PROFILE);
    assert_eq!(report.excluded.len(), 1);
    assert_eq!(report.excluded[0].path, "link");
    assert!(report.entries.iter().all(|e| e.path != "link"));
    let other = preflight(&request(dir.path(), &head, &[])).unwrap();
    // The manifest covers the whole tree, captured or excluded.
    assert_eq!(other.manifest_sha256, report.manifest_sha256);
}

#[test]
fn exclusions_apply_only_to_tracked_symlinks_and_gitlinks() {
    let (dir, head) = base();
    let report = preflight(&request(dir.path(), &head, &["README.md", "missing"])).unwrap();
    assert!(!report.eligible);
    assert_eq!(
        kinds(&report),
        [
            UnsupportedKind::InvalidExclusion,
            UnsupportedKind::InvalidExclusion
        ]
    );
    assert!(report.entries.iter().any(|e| e.path == "README.md"));
}

#[test]
fn index_only_entries_report_their_kinds() {
    let (dir, _) = base();
    let p = dir.path();
    let b = blob(p, "x\n");
    let cases: [(&str, &str, UnsupportedKind); 5] = [
        ("160000", "sub", UnsupportedKind::Gitlink),
        ("100644", "aux.rs", UnsupportedKind::NonPortablePath),
        ("100644", "caf\u{e9}.txt", UnsupportedKind::NonPortablePath),
        ("100644", ".agentscm/x", UnsupportedKind::ReservedNamespace),
        (
            "100644",
            "agentscm-data/y",
            UnsupportedKind::ReservedNamespace,
        ),
    ];
    for (mode, path, kind) in cases {
        let oid = if mode == "160000" {
            run(p, &["rev-parse", "HEAD"])
        } else {
            b.clone()
        };
        run(
            p,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("{mode},{oid},{path}"),
            ],
        );
        let head = commit(p, path);
        let report = preflight(&request(p, &head, &[])).unwrap();
        assert!(!report.eligible, "{path}");
        assert!(
            kinds(&report).contains(&kind),
            "{path}: {:?}",
            report.unsupported
        );
        run(p, &["update-index", "--force-remove", path]);
        commit(p, "undo");
    }
}

#[test]
fn case_alias_and_file_directory_conflict() {
    let (dir, _) = base();
    let p = dir.path();
    let b = blob(p, "x\n");
    for path in ["A.txt", "a.txt", "Dir", "dir/f"] {
        run(
            p,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{b},{path}"),
            ],
        );
    }
    let head = commit(p, "aliases");
    let report = preflight(&request(p, &head, &[])).unwrap();
    assert!(!report.eligible);
    let alias = report
        .unsupported
        .iter()
        .find(|u| u.kind == UnsupportedKind::CaseAlias)
        .unwrap();
    assert_eq!(
        (alias.path.as_str(), alias.detail.as_deref()),
        ("a.txt", Some("A.txt"))
    );
    assert!(kinds(&report).contains(&UnsupportedKind::FileDirectoryConflict));
}

#[test]
fn directory_prefix_aliases_are_ineligible_before_publication() {
    let (dir, _) = base();
    let p = dir.path();
    let b = blob(p, "x\n");
    for path in ["Dir/a", "dir/b", "Outer/Middle/c", "Outer/mIDDLE/d"] {
        run(
            p,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{b},{path}"),
            ],
        );
    }
    let head = commit(p, "directory aliases");
    let req = request(p, &head, &[]);
    let report = preflight(&req).unwrap();
    let aliases: Vec<_> = report
        .unsupported
        .iter()
        .filter(|unsupported| unsupported.kind == UnsupportedKind::CaseAlias)
        .map(|unsupported| (unsupported.path.as_str(), unsupported.detail.as_deref()))
        .collect();
    assert_eq!(
        aliases,
        [
            ("Outer/mIDDLE/d", Some("Outer/Middle")),
            ("dir/b", Some("Dir")),
        ]
    );
    assert!(!report.eligible);

    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let before = object_count(data.path());
    assert!(matches!(
        capture(&engine, "r", &req, report),
        Err(Error::Conflict(message)) if message.contains("CaseAlias")
    ));
    assert_eq!(object_count(data.path()), before);
}

#[test]
fn repeated_directory_spelling_remains_eligible() {
    let (dir, _) = base();
    let p = dir.path();
    for path in ["Dir/a", "Dir/b", "Outer/Middle/c", "Outer/Middle/d"] {
        write(p, path, "x\n");
    }
    run(p, &["add", "Dir", "Outer"]);
    let head = commit(p, "matching directories");
    let report = preflight(&request(p, &head, &[])).unwrap();
    assert!(report.eligible, "{:?}", report.unsupported);
}

#[test]
fn tracked_file_matching_ignore_rules_is_reported() {
    let (dir, _) = base();
    write(dir.path(), ".gitignore", "*.secret\n");
    write(dir.path(), "key.secret", "k\n");
    run(dir.path(), &["add", ".gitignore"]);
    run(dir.path(), &["add", "-f", "key.secret"]);
    let head = commit(dir.path(), "ignored");
    let report = preflight(&request(dir.path(), &head, &[])).unwrap();
    assert!(report.cleanliness.clean);
    assert_eq!(kinds(&report), [UnsupportedKind::IgnoredTracked]);
    assert_eq!(report.unsupported[0].path, "key.secret");
    assert!(!report.eligible);
}

#[test]
fn dirty_staged_and_moved_head_are_not_clean() {
    let (dir, first) = base();
    let p = dir.path();
    write(p, "README.md", "changed\n");
    let report = preflight(&request(p, &first, &[])).unwrap();
    assert!(!report.cleanliness.clean && !report.eligible);
    assert_eq!(report.cleanliness.tracked_change_paths, ["README.md"]);
    run(p, &["add", "README.md"]);
    let report = preflight(&request(p, &first, &[])).unwrap();
    assert_eq!(report.cleanliness.tracked_changes, 1);
    assert!(!report.eligible);
    let second = commit(p, "two");
    let report = preflight(&request(p, &first, &[])).unwrap();
    assert_eq!(report.cleanliness.head, second);
    assert!(!report.cleanliness.clean && !report.eligible);
    assert_eq!(report.entries.len(), 3);
}

#[test]
fn untracked_files_are_counted_not_captured() {
    let (dir, head) = base();
    write(dir.path(), "notes.txt", "scratch\n");
    let report = preflight(&request(dir.path(), &head, &[])).unwrap();
    assert!(report.eligible);
    assert_eq!(report.cleanliness.untracked_count, 1);
    assert!(report.entries.iter().all(|e| e.path != "notes.txt"));
}

#[test]
fn invalid_revisions_and_git_paths_are_errors() {
    let (dir, head) = base();
    assert!(preflight(&request(dir.path(), &head[..12], &[])).is_err());
    assert!(preflight(&request(dir.path(), &head.to_uppercase(), &[])).is_err());
    let tree = run(dir.path(), &["rev-parse", "HEAD^{tree}"]);
    assert!(preflight(&request(dir.path(), &tree, &[])).is_err());
    let mut req = request(dir.path(), &head, &[]);
    req.git = PathBuf::from("git");
    assert!(preflight(&req).is_err());
}

#[test]
fn configured_filter_makes_checkout_ineligible() {
    let (dir, head) = base();
    run(dir.path(), &["config", "filter.x.clean", "cat"]);
    let report = preflight(&request(dir.path(), &head, &[])).unwrap();
    assert_eq!(report.filters, ["filter.x.clean"]);
    assert!(!report.eligible);
}

#[test]
fn manifest_entry_serializes_for_reports() {
    let entry = ManifestEntry {
        path: "a".into(),
        mode: "100644".into(),
        oid: "0".repeat(40),
        size: 1,
    };
    assert!(
        serde_json::to_string(&entry)
            .unwrap()
            .contains("\"size\":1")
    );
}

use agentscm::Error;
use agentscm::git_snapshot::{capture, plan_batches, write_receipt};
use agentscm::store::Engine;

fn entry(path: &str, size: u64) -> ManifestEntry {
    ManifestEntry {
        path: path.to_owned(),
        mode: "100644".into(),
        oid: "0".repeat(40),
        size,
    }
}

fn object_count(data: &Path) -> usize {
    walk(&data.join("objects"))
}

fn walk(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            if p.is_dir() { walk(&p) } else { 1 }
        })
        .sum()
}

#[test]
fn plan_batches_respects_each_limit() {
    let entries: Vec<_> = ["a", "b/c", "b/d/e", "f", "g"]
        .iter()
        .map(|p| entry(p, 10))
        .collect();
    assert!(plan_batches(&[], 2, 2, 20).is_empty());
    assert_eq!(plan_batches(&entries, 100, 100, 1000), vec![0..5]);
    assert_eq!(plan_batches(&entries, 2, 100, 1000), [0..2, 2..4, 4..5]);
    // prefixes per path: 0,1,2,0,0
    assert_eq!(plan_batches(&entries, 100, 2, 1000), [0..2, 2..5]);
    assert_eq!(plan_batches(&entries, 100, 100, 25), [0..2, 2..4, 4..5]);
    // An oversized entry stands alone rather than being dropped.
    assert_eq!(
        plan_batches(&entries, 100, 1, 5),
        [0..1, 1..2, 2..3, 3..4, 4..5]
    );
    let covered: usize = plan_batches(&entries, 3, 3, 30)
        .iter()
        .map(|r| r.len())
        .sum();
    assert_eq!(covered, entries.len());
}

#[test]
fn capture_publishes_manifest_bytes_and_receipt_is_idempotent() {
    let (dir, _) = base();
    std::os::unix::fs::symlink("README.md", dir.path().join("link")).unwrap();
    run(dir.path(), &["add", "link"]);
    let head = commit(dir.path(), "link");
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let req = request(dir.path(), &head, &["link"]);
    let snapshot = capture(&engine, "repo-1", &req, preflight(&req).unwrap()).unwrap();
    let report = preflight(&req).unwrap();
    assert_eq!(snapshot.receipt.git.commit, head);
    assert_eq!(snapshot.receipt.manifest_sha256, report.manifest_sha256);
    let page = engine.list_files(&snapshot.revision, None, 100).unwrap();
    let listed: std::collections::BTreeSet<&str> =
        page.entries.iter().map(|e| e.path.as_str()).collect();
    let wanted: std::collections::BTreeSet<&str> =
        report.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(listed, wanted);
    assert!(!listed.contains("link"));
    for e in &report.entries {
        assert_eq!(
            engine.read_file(&snapshot.revision, &e.path).unwrap(),
            fs::read(dir.path().join(&e.path)).unwrap()
        );
    }
    let json: serde_json::Value =
        serde_json::from_slice(&snapshot.receipt.to_bytes().unwrap()).unwrap();
    assert_eq!(json["receipt"], "agentscm.git-snapshot-receipt/v1");
    assert_eq!(json["native"]["revision"], snapshot.revision);
    assert_eq!(json["git"]["tree"], report.tree);
    assert_eq!(json["content_verification"], "sha1-rehashed");

    // Restart before the receipt exists: the same revision is published again.
    let again = capture(&engine, "repo-1", &req, preflight(&req).unwrap()).unwrap();
    assert_eq!(again.revision, snapshot.revision);
    let out = TempDir::new().unwrap();
    let path = out.path().join("receipt.json");
    assert!(!write_receipt(&path, &snapshot.receipt).unwrap());
    let bytes = fs::read(&path).unwrap();
    assert!(write_receipt(&path, &again.receipt).unwrap());
    assert_eq!(fs::read(&path).unwrap(), bytes);

    // A different request never replaces an existing receipt.
    write(dir.path(), "extra.txt", "x\n");
    run(dir.path(), &["add", "extra.txt"]);
    let next = commit(dir.path(), "extra");
    let next_req = request(dir.path(), &next, &["link"]);
    let other = capture(&engine, "repo-1", &next_req, preflight(&next_req).unwrap()).unwrap();
    assert!(matches!(
        write_receipt(&path, &other.receipt),
        Err(Error::Conflict(_))
    ));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn ineligible_capture_writes_nothing_and_names_the_cause() {
    let (dir, head) = base();
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let before = object_count(data.path());
    write(dir.path(), "README.md", "dirty\n");
    let req = request(dir.path(), &head, &[]);
    let message = capture(&engine, "r", &req, preflight(&req).unwrap())
        .err()
        .unwrap()
        .to_string();
    assert!(
        message.contains("not clean") && message.contains("README.md"),
        "{message}"
    );
    run(dir.path(), &["checkout", "README.md"]);

    let oid = blob(dir.path(), "/etc/passwd");
    run(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{oid},bad-link"),
        ],
    );
    let hostile = commit(dir.path(), "hostile");
    let req = request(dir.path(), &hostile, &[]);
    let message = capture(&engine, "r", &req, preflight(&req).unwrap())
        .err()
        .unwrap()
        .to_string();
    assert!(
        message.contains("SymlinkTarget") && message.contains("bad-link"),
        "{message}"
    );
    assert_eq!(object_count(data.path()), before);
}

#[test]
fn clean_filter_is_never_run_and_cleanliness_is_unverified() {
    let (dir, head) = base();
    let p = dir.path();
    write(p, ".gitattributes", "* filter=x\n");
    run(p, &["add", ".gitattributes"]);
    let head2 = commit(p, "attributes");
    let marker = TempDir::new().unwrap();
    let ran = marker.path().join("RAN");
    run(
        p,
        &[
            "config",
            "filter.x.clean",
            &format!("sh -c 'touch {}; cat'", ran.display()),
        ],
    );
    // Touch a tracked file so a stat-based status would run the clean filter.
    write(p, "README.md", "hello\n");
    let report = preflight(&request(p, &head2, &[])).unwrap();
    assert!(!report.eligible && !report.cleanliness.clean);
    assert_eq!(report.cleanliness.head, head2);
    assert_ne!(head, head2);
    assert!(!ran.exists(), "clean filter ran");
}

#[test]
fn skip_worktree_and_assume_unchanged_edits_are_not_clean() {
    for flag in ["--skip-worktree", "--assume-unchanged"] {
        let (dir, head) = base();
        let p = dir.path();
        run(p, &["update-index", flag, "README.md"]);
        write(p, "README.md", "edited\n");
        let report = preflight(&request(p, &head, &[])).unwrap();
        assert!(!report.cleanliness.clean && !report.eligible, "{flag}");
        assert_eq!(report.cleanliness.tracked_change_paths, ["README.md"]);
    }
}

#[test]
fn staged_rename_is_not_clean_and_names_the_new_path() {
    let dir = repo();
    write(dir.path(), "a b.txt", "x\n");
    run(dir.path(), &["add", "."]);
    let head = commit(dir.path(), "one");
    run(dir.path(), &["mv", "a b.txt", "c d.txt"]);
    let report = preflight(&request(dir.path(), &head, &[])).unwrap();
    assert!(!report.cleanliness.clean && !report.eligible);
    assert_eq!(report.cleanliness.tracked_changes, 1);
    assert_eq!(report.cleanliness.tracked_change_paths, ["c d.txt"]);
}

#[test]
fn over_deep_path_is_a_depth_violation() {
    let (dir, _) = base();
    let p = dir.path();
    let b = blob(p, "x\n");
    let path = format!("{}f", "d/".repeat(129));
    run(
        p,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{b},{path}"),
        ],
    );
    let head = commit(p, "deep");
    let report = preflight(&request(p, &head, &[])).unwrap();
    assert!(!report.eligible);
    assert!(
        report
            .limit_violations
            .iter()
            .any(|v| v.limit == LimitKind::Depth && v.observed == 129 && v.maximum == 128),
        "{:?}",
        report.limit_violations
    );
}
