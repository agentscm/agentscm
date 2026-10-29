#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentscm::attached_worktree::{AttachRequest, AttachmentJournal, SourceMode};
use agentscm::store::Engine;
use tempfile::TempDir;

fn git_path() -> PathBuf {
    PathBuf::from(std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into()))
}
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(git_path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
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
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn isolated_git(repo: &Path) -> PathBuf {
    let common = PathBuf::from(git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    let wrapper = common.join("synthetic-git");
    let executable = git_path().to_string_lossy().replace('\'', "'\\''");
    fs::write(&wrapper,format!("#!/bin/sh\nexport GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1\nexec '{executable}' \"$@\"\n")).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    wrapper
}
fn write(root: &Path, path: &str, bytes: &[u8]) {
    let full = root.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, bytes).unwrap()
}
fn fixture() -> (TempDir, TempDir, Engine, AttachmentJournal) {
    let repo = TempDir::new().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "core.excludesFile", ""]);
    write(repo.path(), "a.txt", b"a0\n");
    write(repo.path(), "b.txt", b"b0\n");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "base"]);
    git(
        repo.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/repository.git",
        ],
    );
    git(
        repo.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let journal = AttachmentJournal::open(data.path(), "repository-1").unwrap();
    (repo, data, engine, journal)
}
fn request(repo: &Path, actor: &str, source: SourceMode, paths: &[&str]) -> AttachRequest {
    AttachRequest {
        git: isolated_git(repo),
        worktree: repo.into(),
        actor: actor.into(),
        remote: "origin".into(),
        target: "main".into(),
        output: "agents/alice".into(),
        source,
        paths: if source == SourceMode::Committed && paths.is_empty() {
            vec![".".into()]
        } else {
            paths.iter().map(|p| (*p).into()).collect()
        },
    }
}

#[test]
fn no_op_capture_is_stable_and_durable() {
    let (repo, _data, engine, journal) = fixture();
    let state = journal
        .attach(
            &engine,
            "attach-1",
            &request(repo.path(), "alice", SourceMode::Committed, &[]),
        )
        .unwrap();
    let first = journal.capture(&engine, "capture-1", &state.id, 0).unwrap();
    assert!(first.no_op);
    assert_eq!(first.attachment.generation, 0);
    let retry = journal.capture(&engine, "capture-1", &state.id, 0).unwrap();
    assert_eq!(retry.revision, first.revision);
    assert_eq!(retry.attachment.generation, 0);
}

#[test]
fn worktree_capture_selects_paths_and_preserves_index() {
    let (repo, _data, engine, journal) = fixture();
    let state = journal
        .attach(
            &engine,
            "attach-1",
            &request(repo.path(), "alice", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    write(repo.path(), "a.txt", b"a1\n");
    git(repo.path(), &["add", "a.txt"]);
    write(repo.path(), "a.txt", b"a2\n");
    write(repo.path(), "b.txt", b"b1\n");
    let index = PathBuf::from(git(
        repo.path(),
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    ));
    let before = fs::read(&index).unwrap();
    let out = journal.capture(&engine, "capture-1", &state.id, 0).unwrap();
    assert_eq!(out.changed_paths, ["a.txt"]);
    assert_eq!(fs::read(index).unwrap(), before);
    assert_eq!(engine.read_file(&out.revision, "a.txt").unwrap(), b"a2\n");
    assert_eq!(engine.read_file(&out.revision, "b.txt").unwrap(), b"b0\n");
}

#[test]
fn index_mode_is_refused_with_recovery_guidance() {
    let (repo, _data, engine, journal) = fixture();
    let error = journal
        .attach(
            &engine,
            "attach-1",
            &request(repo.path(), "alice", SourceMode::Index, &[]),
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("commit") && error.contains("worktree"),
        "{error}"
    );
}

#[test]
fn linked_worktrees_have_distinct_identity_and_target_rewrites_block() {
    let (repo, _data, engine, journal) = fixture();
    let linked = TempDir::new().unwrap();
    let linked_path = linked.path().join("tree");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            linked_path.to_str().unwrap(),
        ],
    );
    let one = journal
        .attach(
            &engine,
            "attach-1",
            &request(repo.path(), "alice", SourceMode::Committed, &[]),
        )
        .unwrap();
    let two = journal
        .attach(
            &engine,
            "attach-2",
            &request(&linked_path, "alice", SourceMode::Committed, &[]),
        )
        .unwrap();
    assert_ne!(one.id, two.id);
    write(repo.path(), "new.txt", b"new\n");
    git(repo.path(), &["add", "new.txt"]);
    git(repo.path(), &["commit", "-q", "-m", "next"]);
    git(
        repo.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    journal.refresh(&engine, "refresh-1", &one.id, 0).unwrap();
    let old = git(repo.path(), &["rev-parse", "HEAD^"]);
    git(
        repo.path(),
        &["update-ref", "refs/remotes/origin/main", &old],
    );
    let error = journal
        .refresh(&engine, "refresh-2", &one.id, 1)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("rewritten") || error.contains("diverged"),
        "{error}"
    );
}

fn commit(root: &Path, path: &str, bytes: &[u8]) -> String {
    write(root, path, bytes);
    git(root, &["add", path]);
    git(root, &["commit", "-qm", "synthetic"]);
    git(root, &["rev-parse", "HEAD"])
}
fn incoming(root: &Path, parent: &str, path: &str, bytes: &[u8]) -> String {
    let temp = TempDir::new().unwrap();
    let work = temp.path().join("incoming");
    git(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            "-q",
            work.to_str().unwrap(),
            parent,
        ],
    );
    let oid = commit(&work, path, bytes);
    git(root, &["worktree", "remove", work.to_str().unwrap()]);
    git(root, &["update-ref", "refs/remotes/origin/main", &oid]);
    oid
}

#[test]
fn advancing_retries_return_the_entire_original_outcome_after_further_edits() {
    let (repo, data, engine, journal) = fixture();
    let req = request(repo.path(), "alice", SourceMode::Worktree, &["a.txt"]);
    let state = journal.attach(&engine, "attach", &req).unwrap();
    write(repo.path(), "a.txt", b"first");
    let first = journal.capture(&engine, "capture", &state.id, 0).unwrap();
    assert!(!first.no_op);
    write(repo.path(), "a.txt", b"second");
    journal.capture(&engine, "capture2", &state.id, 1).unwrap();
    assert_eq!(
        first,
        journal.capture(&engine, "capture", &state.id, 0).unwrap()
    );
    assert_eq!(state, journal.attach(&engine, "attach", &req).unwrap());
    assert!(journal.capture(&engine, "capture", &state.id, 2).is_err());
    let mut other = req.clone();
    other.paths = vec!["b.txt".into()];
    assert!(journal.attach(&engine, "attach", &other).is_err());
    drop(journal);
    let reopened = AttachmentJournal::open(data.path(), "repository-1").unwrap();
    assert_eq!(
        first,
        reopened.capture(&engine, "capture", &state.id, 0).unwrap()
    );
}

#[test]
fn branch_ahead_refresh_preserves_commits_and_selected_edits_with_stable_noop() {
    let (repo, _data, engine, journal) = fixture();
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    commit(repo.path(), "feature.txt", b"branch work");
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "a", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    assert_ne!(state.baseline_revision, state.candidate_revision);
    write(repo.path(), "a.txt", b"selected");
    let captured = journal.capture(&engine, "capture", &state.id, 0).unwrap();
    incoming(repo.path(), &base, "incoming.txt", b"external");
    assert!(
        journal
            .capture(&engine, "premature", &state.id, 1)
            .unwrap_err()
            .to_string()
            .contains("refresh")
    );
    let refreshed = journal.refresh(&engine, "refresh", &state.id, 1).unwrap();
    for (path, expected) in [
        ("feature.txt", b"branch work".as_slice()),
        ("a.txt", b"selected"),
        ("incoming.txt", b"external"),
    ] {
        assert_eq!(
            engine
                .read_file(&refreshed.candidate_revision, path)
                .unwrap(),
            expected
        );
    }
    let no_op = journal.capture(&engine, "noop", &state.id, 2).unwrap();
    assert!(no_op.no_op);
    assert_eq!(no_op.attachment, refreshed);
    assert_eq!(
        journal.refresh(&engine, "refresh", &state.id, 1).unwrap(),
        refreshed
    );
    assert_eq!(
        journal.capture(&engine, "capture", &state.id, 0).unwrap(),
        captured
    );
    assert!(journal.capture(&engine, "stale", &state.id, 0).is_err());
    assert!(journal.validate_current(&engine, &state.id, 2).is_ok());
}

#[test]
fn refresh_adopts_clean_git_merge_resolution_for_same_file() {
    let (repo, _data, engine, journal) = fixture();
    write(
        repo.path(),
        "a.txt",
        b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
    );
    git(repo.path(), &["add", "a.txt"]);
    git(repo.path(), &["commit", "-qm", "lines"]);
    git(
        repo.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    commit(
        repo.path(),
        "a.txt",
        b"ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
    );
    let state = journal
        .attach(
            &engine,
            "attach-merge",
            &request(repo.path(), "merge", SourceMode::Committed, &[]),
        )
        .unwrap();
    let target = incoming(
        repo.path(),
        &base,
        "a.txt",
        b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nNINE\n",
    );
    git(repo.path(), &["merge", "--no-edit", &target]);
    let refreshed = journal
        .refresh(&engine, "refresh-merge", &state.id, 0)
        .unwrap();
    assert_eq!(
        refreshed.head_commit,
        git(repo.path(), &["rev-parse", "HEAD"])
    );
    assert_eq!(
        engine
            .read_file(&refreshed.candidate_revision, "a.txt")
            .unwrap(),
        b"ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nNINE\n"
    );
}

#[test]
fn refresh_adopts_explicit_git_conflict_resolution() {
    let (repo, _data, engine, journal) = fixture();
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    commit(repo.path(), "a.txt", b"branch\n");
    let state = journal
        .attach(
            &engine,
            "attach-resolution",
            &request(repo.path(), "resolution", SourceMode::Committed, &[]),
        )
        .unwrap();
    let target = incoming(repo.path(), &base, "a.txt", b"target\n");
    let status = Command::new(git_path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(repo.path())
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(["merge", "--no-edit", &target])
        .status()
        .unwrap();
    assert!(!status.success());
    assert_eq!(
        git(repo.path(), &["rev-parse", "--verify", "MERGE_HEAD"]),
        target
    );
    write(repo.path(), "a.txt", b"resolved by git user\n");
    git(repo.path(), &["add", "a.txt"]);
    git(repo.path(), &["commit", "-qm", "resolve"]);
    let refreshed = journal
        .refresh(&engine, "refresh-resolution", &state.id, 0)
        .unwrap();
    assert_eq!(
        engine
            .read_file(&refreshed.candidate_revision, "a.txt")
            .unwrap(),
        b"resolved by git user\n"
    );
}

#[test]
fn git_target_adoption_preserves_prior_native_refresh_content() {
    let (repo, _data, engine, journal) = fixture();
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    commit(repo.path(), "feature.txt", b"branch\n");
    let state = journal
        .attach(
            &engine,
            "attach-prior",
            &request(repo.path(), "prior", SourceMode::Committed, &[]),
        )
        .unwrap();
    let first_target = incoming(repo.path(), &base, "incoming.txt", b"first\n");
    let first = journal
        .refresh(&engine, "refresh-prior-1", &state.id, 0)
        .unwrap();
    assert_eq!(
        engine
            .read_file(&first.candidate_revision, "incoming.txt")
            .unwrap(),
        b"first\n"
    );
    git(repo.path(), &["merge", "--no-edit", &first_target]);
    let second = journal
        .refresh(&engine, "refresh-prior-2", &state.id, 1)
        .unwrap();
    assert_eq!(
        engine
            .read_file(&second.candidate_revision, "incoming.txt")
            .unwrap(),
        b"first\n"
    );
    assert_eq!(
        engine
            .read_file(&second.candidate_revision, "feature.txt")
            .unwrap(),
        b"branch\n"
    );
}

#[test]
fn conflicts_and_head_rewrites_leave_state_unchanged() {
    let (repo, _data, engine, journal) = fixture();
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    commit(repo.path(), "feature", b"keep");
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "a", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    write(repo.path(), "a.txt", b"ours");
    let captured = journal
        .capture(&engine, "capture", &state.id, 0)
        .unwrap()
        .attachment;
    incoming(repo.path(), &base, "a.txt", b"theirs");
    assert!(
        journal
            .refresh(&engine, "conflict", &state.id, 1)
            .unwrap_err()
            .to_string()
            .contains("conflict")
    );
    assert_eq!(journal.state(&state.id).unwrap(), captured);
    git(repo.path(), &["reset", "--hard", &base]);
    for refresh in [true, false] {
        let err = if refresh {
            journal
                .refresh(&engine, "rewrite", &state.id, 1)
                .unwrap_err()
        } else {
            journal
                .capture(&engine, "rewrite", &state.id, 1)
                .unwrap_err()
        };
        assert!(err.to_string().contains("rewritten"));
    }
    assert_eq!(journal.state(&state.id).unwrap(), captured);
}

#[test]
fn identical_trees_retain_every_commit_mapping() {
    let (repo, _data, engine, journal) = fixture();
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "a", SourceMode::Committed, &[]),
        )
        .unwrap();
    git(repo.path(), &["commit", "--allow-empty", "-qm", "empty"]);
    let out = journal.capture(&engine, "empty", &state.id, 0).unwrap();
    let mappings = journal.mappings(&state.id, None, 256).unwrap();
    assert_eq!(mappings.len(), 2);
    assert_eq!(mappings[0].revision, mappings[1].revision);
    assert_eq!(
        out.attachment.head_commit,
        git(repo.path(), &["rev-parse", "HEAD"])
    );
    let page = journal.mappings(&state.id, None, 1).unwrap();
    assert_eq!(
        journal
            .mappings(&state.id, Some(&page[0].commit), 1)
            .unwrap()
            .len(),
        1
    );
    assert!(journal.mappings(&state.id, None, 257).is_err());
}

#[test]
fn git_inventory_kinds_absence_and_selected_only_scan() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (repo, _data, engine, journal) = fixture();
    commit(repo.path(), "tracked.log", b"base");
    commit(repo.path(), ".gitignore", b"*.log\n.env\n");
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(
                repo.path(),
                "a",
                SourceMode::Worktree,
                &["tracked.log", "staged.log", "a.txt", "b.txt", "link"],
            ),
        )
        .unwrap();
    write(repo.path(), "tracked.log", b"tracked ignored edit");
    write(repo.path(), "staged.log", b"staged ignored add");
    git(repo.path(), &["add", "-f", "staged.log"]);
    write(repo.path(), "unselected:bad", b"never read");
    write(repo.path(), ".env", b"synthetic private");
    fs::remove_file(repo.path().join("a.txt")).unwrap();
    fs::set_permissions(repo.path().join("b.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("b.txt", repo.path().join("link")).unwrap();
    let index = fs::read(repo.path().join(".git/index")).unwrap();
    let out = journal.capture(&engine, "capture", &state.id, 0).unwrap();
    assert_eq!(
        engine.read_file(&out.revision, "tracked.log").unwrap(),
        b"tracked ignored edit"
    );
    assert_eq!(
        engine.read_file(&out.revision, "staged.log").unwrap(),
        b"staged ignored add"
    );
    assert!(engine.stat_file(&out.revision, "a.txt").unwrap().is_none());
    assert!(
        engine
            .list_files(&out.revision, None, 100)
            .unwrap()
            .entries
            .iter()
            .all(|f| f.path != "unselected:bad")
    );
    assert!(engine.stat_file(&out.revision, ".env").unwrap().is_none());
    assert_eq!(
        engine.read_entry(&out.revision, "link").unwrap().0,
        agentscm::model::EntryKind::Symlink
    );
    assert_eq!(
        engine.read_entry(&out.revision, "b.txt").unwrap().0,
        agentscm::model::EntryKind::Executable
    );
    assert_eq!(index, fs::read(repo.path().join(".git/index")).unwrap());
}

#[test]
fn fingerprints_include_exclusion_inputs_and_absence() {
    let (repo, _data, engine, journal) = fixture();
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "a", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    assert!(journal.validate_current(&engine, &state.id, 0).is_ok());
    write(
        repo.path(),
        ".gitignore",
        b"# even a selection-neutral rule edit\n",
    );
    assert!(journal.validate_current(&engine, &state.id, 0).is_err());
    let out = journal.capture(&engine, "rules", &state.id, 0).unwrap();
    assert!(!out.no_op);
    fs::remove_file(repo.path().join("a.txt")).unwrap();
    assert!(journal.validate_current(&engine, &state.id, 1).is_err());
    assert!(out.historical);
}

#[test]
fn invalid_requests_and_missing_exact_remote_are_rejected_before_attachment() {
    let (repo, _data, engine, journal) = fixture();
    for paths in [
        vec!["../x"],
        vec!["/abs"],
        vec![".git/config"],
        vec!["a//b"],
        vec![".", "a.txt"],
        vec![],
    ] {
        assert!(
            journal
                .attach(
                    &engine,
                    "invalid",
                    &request(repo.path(), "a", SourceMode::Worktree, &paths)
                )
                .is_err(),
            "{paths:?}"
        );
    }
    let mut req = request(repo.path(), "a", SourceMode::Committed, &[]);
    req.remote = "-bad".into();
    assert!(journal.attach(&engine, "badremote", &req).is_err());
    git(repo.path(), &["update-ref", "refs/heads/main", "HEAD"]);
    git(
        repo.path(),
        &["update-ref", "-d", "refs/remotes/origin/main"],
    );
    req.remote = "origin".into();
    assert!(
        journal
            .attach(&engine, "missing", &req)
            .unwrap_err()
            .to_string()
            .contains("exact remote")
    );
}

#[test]
fn credentialed_remote_is_neither_echoed_nor_persisted() {
    let (repo, data, engine, journal) = fixture();
    git(
        repo.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://user:synthetic-password@example.invalid/repo",
        ],
    );
    let error = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "a", SourceMode::Committed, &[]),
        )
        .unwrap_err()
        .to_string();
    assert!(!error.contains("synthetic-password"));
    assert!(
        !fs::read(data.path().join("attachments/state.sqlite"))
            .unwrap()
            .windows(18)
            .any(|s| s == b"synthetic-password")
    );
}

#[test]
fn journal_permissions_identity_corruption_and_recovery_lease() {
    use std::os::unix::fs::PermissionsExt;
    let (repo, data, engine, journal) = fixture();
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "a", SourceMode::Committed, &[]),
        )
        .unwrap();
    let db_path = data.path().join("attachments/state.sqlite");
    assert_eq!(
        fs::metadata(&db_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(agentscm::local_recovery::AccessGuard::exclusive(data.path()).is_err());
    assert!(AttachmentJournal::open(data.path(), "foreign").is_err());
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("UPDATE attachments SET generation=7", [])
        .unwrap();
    assert!(
        journal
            .state(&state.id)
            .unwrap_err()
            .to_string()
            .contains("mismatch")
    );
    db.pragma_update(None, "application_id", 1).unwrap();
    assert!(journal.state(&state.id).is_err());
    drop(db);
    drop(journal);
    drop(engine);
    assert!(AttachmentJournal::open(data.path(), "repository-1").is_err());
}

#[test]
fn concurrent_identical_requests_replay_and_stale_noop_fails() {
    let (repo, data, engine, journal) = fixture();
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "a", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    write(repo.path(), "a.txt", b"advance");
    let result = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            let j = AttachmentJournal::open(data.path(), "repository-1").unwrap();
            let e = Engine::open(data.path()).unwrap();
            j.capture(&e, "same", &state.id, 0).unwrap()
        });
        let b = scope.spawn(|| {
            let j = AttachmentJournal::open(data.path(), "repository-1").unwrap();
            let e = Engine::open(data.path()).unwrap();
            j.capture(&e, "same", &state.id, 0).unwrap()
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(result.0, result.1);
    assert!(
        journal
            .capture(&engine, "stale-noop", &state.id, 0)
            .is_err()
    );
    assert!(
        journal
            .refresh(&engine, "stale-refresh", &state.id, 0)
            .is_err()
    );
}

#[test]
fn raw_worktree_conversion_semantics_are_explicit_bytes() {
    let (repo, _data, engine, journal) = fixture();
    commit(repo.path(), ".gitattributes", b"*.txt text eol=crlf\n");
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "a", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    write(repo.path(), "a.txt", b"a0\r\n");
    let out = journal.capture(&engine, "raw", &state.id, 0).unwrap();
    assert_eq!(engine.read_file(&out.revision, "a.txt").unwrap(), b"a0\r\n");
    assert!(out.historical);
}

#[test]
fn actual_authority_binding_runs_exact_checks_without_native_landing_and_invalidates() {
    use agentscm::authority::Authority;
    use agentscm::checks::{
        CheckLimits, CheckState, SOURCE_PATHS_CAPABILITY, SOURCE_PATHS_EVIDENCE,
        SourcePathExpectation, SourcePathsRecipe, SourcePathsRequest,
    };
    use agentscm::context::{BaseState, ChangeId, InputIdentity, RequiredEvidence, ReviewContext};
    use agentscm::store::digest;
    let (repo, _unused_data, _unused_engine, _unused_journal) = fixture();
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let mut authority = Authority::open(data.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    let journal = AttachmentJournal::open(data.path(), &repository).unwrap();
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "actor", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    let recipe = SourcePathsRecipe {
        version: 1,
        expectations: vec![SourcePathExpectation {
            path: "a.txt".into(),
            sha256: digest(b"a0\n"),
        }],
        limits: CheckLimits {
            max_paths: 8,
            max_total_bytes: 1024,
            max_output_bytes: 4096,
            timeout_ms: 30000,
        },
    };
    let identity = InputIdentity::Digest {
        sha256: recipe.digest().unwrap(),
    };
    let mut context = ReviewContext {
        version: 1,
        change: ChangeId {
            repository: repository.clone(),
            id: "actual-change".into(),
        },
        change_version: 1,
        revision: state.candidate_revision.clone(),
        base: None,
        target: "main".into(),
        policy_epoch: 0,
        intent: "synthetic exact source qualification".into(),
        acceptance_criteria: vec!["matches expected source".into()],
        scope: vec!["a.txt".into()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: identity.clone(),
        toolchain: InputIdentity::Digest {
            sha256: digest(b"synthetic-toolchain"),
        },
        environment: InputIdentity::Digest {
            sha256: digest(b"synthetic-environment"),
        },
        required_evidence: vec![RequiredEvidence {
            identity: SOURCE_PATHS_EVIDENCE.into(),
            digest: identity,
        }],
        required_capabilities: vec![
            "agentscm.review-context/v1".into(),
            SOURCE_PATHS_CAPABILITY.into(),
        ],
    };
    authority.start_change(&context).unwrap();
    let bound = journal
        .bind_context(&authority, "bind", &state.id, 0, &context.change)
        .unwrap();
    assert_eq!(bound.change_id, Some(context.change.clone()));
    let attempt = SourcePathsRequest {
        attempt_id: "exact".into(),
        change: context.change.clone(),
        change_version: 1,
        context_digest: context.digest().unwrap(),
        revision: context.revision.clone(),
        recipe,
    };
    authority.create_source_paths_attempt(&attempt).unwrap();
    assert_eq!(
        authority
            .run_source_paths_attempt(&engine, "exact", 0)
            .unwrap()
            .state,
        CheckState::Success
    );
    assert_eq!(
        authority
            .qualify_draft_candidate(&context.change)
            .unwrap()
            .digest()
            .unwrap(),
        context.digest().unwrap()
    );
    assert!(authority.target("main").unwrap().is_none());
    write(repo.path(), "a.txt", b"new source");
    let advanced = journal
        .capture(&engine, "advance", &state.id, 1)
        .unwrap()
        .attachment;
    assert_eq!(advanced.change_id, bound.change_id);
    assert!(advanced.context.is_none());
    assert!(
        journal
            .bind_context(&authority, "rebind", &state.id, 2, &context.change)
            .is_err()
    );
    context.change_version = 2;
    context.revision = advanced.candidate_revision.clone();
    authority.update_change_context(1, &context).unwrap();
    assert!(authority.qualify_draft_candidate(&context.change).is_err());
    let rebound = journal
        .bind_context(&authority, "rebind", &state.id, 2, &context.change)
        .unwrap();

    let second = journal
        .attach(
            &engine,
            "attach-second-binding",
            &request(repo.path(), "actor-2", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    assert_eq!(second.candidate_revision, advanced.candidate_revision);
    assert!(
        journal
            .bind_context(
                &authority,
                "bound-elsewhere",
                &second.id,
                0,
                &context.change,
            )
            .unwrap_err()
            .to_string()
            .contains("another attachment")
    );

    let mut different = context.clone();
    different.change = ChangeId {
        repository: repository.clone(),
        id: "different-change".into(),
    };
    different.change_version = 1;
    authority.start_change(&different).unwrap();
    assert!(
        journal
            .bind_context(
                &authority,
                "different-change",
                &state.id,
                3,
                &different.change,
            )
            .is_err()
    );

    let third = journal
        .attach(
            &engine,
            "attach-base-binding",
            &request(repo.path(), "actor-3", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    let mut based = context.clone();
    based.change = ChangeId {
        repository: repository.clone(),
        id: "based-change".into(),
    };
    based.change_version = 1;
    based.revision = third.candidate_revision.clone();
    based.base = Some(BaseState {
        revision: third.baseline_revision.clone(),
        target_version: 1,
    });
    let fourth = journal
        .attach(
            &engine,
            "attach-target-binding",
            &request(repo.path(), "actor-4", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    let mut occupied = context.clone();
    occupied.change = ChangeId {
        repository: repository.clone(),
        id: "occupied-change".into(),
    };
    occupied.change_version = 1;
    occupied.revision = fourth.candidate_revision.clone();
    authority.start_change(&occupied).unwrap();
    let authority_db = rusqlite::Connection::open(data.path().join("state.sqlite")).unwrap();
    authority_db
        .execute(
            "INSERT INTO targets(name,revision,version) VALUES('main',?1,1)",
            [&fourth.baseline_revision],
        )
        .unwrap();
    drop(authority_db);
    authority.start_change(&based).unwrap();
    assert!(
        journal
            .bind_context(&authority, "based-change", &third.id, 0, &based.change)
            .is_err()
    );
    assert!(
        journal
            .bind_context(
                &authority,
                "occupied-target",
                &fourth.id,
                0,
                &occupied.change,
            )
            .is_err()
    );

    let foreign_data = TempDir::new().unwrap();
    let foreign_authority = Authority::open(foreign_data.path()).unwrap();
    assert!(
        journal
            .bind_context(
                &foreign_authority,
                "foreign-authority",
                &fourth.id,
                0,
                &occupied.change,
            )
            .unwrap_err()
            .to_string()
            .contains("different data")
    );
    // Historical replay survives source/context advancement and a native target
    // created after the original binding, but never a different Authority/root.
    assert_eq!(
        journal
            .bind_context(&authority, "bind", &state.id, 0, &context.change)
            .unwrap(),
        bound
    );
    assert!(
        journal
            .bind_context(&foreign_authority, "bind", &state.id, 0, &context.change)
            .is_err()
    );
    let db = rusqlite::Connection::open(data.path().join("attachments/state.sqlite")).unwrap();
    // Point a retained bind at another otherwise consistent state carrying the
    // same Change/context. The caller ID must reject this independently.
    let mut swapped = second.clone();
    swapped.generation = rebound.generation;
    swapped.change_id = rebound.change_id.clone();
    swapped.context = rebound.context.clone();
    db.execute(
        "UPDATE bindings SET attachment=?1 WHERE change_id=?2",
        rusqlite::params![second.id, context.change.id],
    )
    .unwrap();
    db.execute(
        "UPDATE attachments SET generation=?1,state_json=?2 WHERE id=?3",
        rusqlite::params![
            swapped.generation,
            serde_json::to_vec(&swapped).unwrap(),
            second.id
        ],
    )
    .unwrap();
    db.execute(
        "UPDATE operations SET outcome_json=?1 WHERE operation='rebind'",
        [serde_json::to_vec(&swapped).unwrap()],
    )
    .unwrap();
    assert!(
        journal
            .bind_context(&authority, "rebind", &state.id, 2, &context.change)
            .is_err()
    );
    db.execute(
        "UPDATE operations SET outcome_json=?1 WHERE operation='rebind'",
        [serde_json::to_vec(&rebound).unwrap()],
    )
    .unwrap();
    db.execute(
        "UPDATE attachments SET generation=?1,state_json=?2 WHERE id=?3",
        rusqlite::params![
            second.generation,
            serde_json::to_vec(&second).unwrap(),
            second.id
        ],
    )
    .unwrap();
    db.execute(
        "UPDATE bindings SET attachment=?1 WHERE change_id=?2",
        rusqlite::params![state.id, context.change.id],
    )
    .unwrap();
    let original = serde_json::to_vec(&bound).unwrap();
    let mut corruptions = Vec::new();
    let mut version = bound.clone();
    version.context.as_mut().unwrap().version += 1;
    corruptions.push(version);
    let mut digest = bound.clone();
    digest.context.as_mut().unwrap().context_digest = "0".repeat(64);
    corruptions.push(digest);
    let mut later_context = bound.clone();
    later_context.context = rebound.context.clone();
    corruptions.push(later_context);
    let mut future = rebound.clone();
    future.generation += 1;
    corruptions.push(future);
    let mut wrong_candidate = bound.clone();
    wrong_candidate.candidate_revision = advanced.candidate_revision.clone();
    corruptions.push(wrong_candidate);
    // Even a structurally valid other attachment outcome cannot answer this ID.
    corruptions.push(second.clone());
    for corrupted in corruptions {
        db.execute(
            "UPDATE operations SET outcome_json=?1 WHERE operation='bind'",
            [serde_json::to_vec(&corrupted).unwrap()],
        )
        .unwrap();
        assert!(
            journal
                .bind_context(&authority, "bind", &state.id, 0, &context.change)
                .is_err()
        );
    }
    db.execute(
        "UPDATE operations SET outcome_json=?1 WHERE operation='bind'",
        [&original],
    )
    .unwrap();
    db.execute("UPDATE identity SET repository='foreign-repository'", [])
        .unwrap();
    assert!(
        journal
            .bind_context(&authority, "bind", &state.id, 0, &context.change)
            .is_err()
    );
    db.execute("UPDATE identity SET repository=?1", [&repository])
        .unwrap();
    db.execute("DELETE FROM bindings WHERE attachment=?1", [&state.id])
        .unwrap();
    assert!(
        journal
            .bind_context(&authority, "bind", &state.id, 0, &context.change)
            .is_err()
    );
    db.execute(
        "INSERT INTO bindings VALUES(?1,?2)",
        rusqlite::params![context.change.id, state.id],
    )
    .unwrap();
    let mappings = journal.mappings(&state.id, None, 256).unwrap();
    db.execute("DELETE FROM mappings WHERE id=?1", [&state.id])
        .unwrap();
    assert!(
        journal
            .bind_context(&authority, "bind", &state.id, 0, &context.change)
            .is_err()
    );
    for mapping in mappings {
        db.execute(
            "INSERT INTO mappings VALUES(?1,?2,?3,?4)",
            rusqlite::params![state.id, mapping.commit, mapping.revision, mapping.tree],
        )
        .unwrap();
    }
    assert_eq!(
        journal
            .bind_context(&authority, "bind", &state.id, 0, &context.change)
            .unwrap(),
        bound
    );
    drop(db);
    drop(journal);
    drop(authority);
    let reopened = AttachmentJournal::open(data.path(), &repository).unwrap();
    let authority = Authority::open(data.path()).unwrap();
    assert_eq!(reopened.state(&state.id).unwrap(), rebound);
    assert_eq!(
        rebound.context.clone().unwrap(),
        authority.change(&context.change).unwrap()
    );
    incoming(repo.path(), &state.target_commit, "b.txt", b"later target");
    let refreshed = reopened
        .refresh(&engine, "later-target", &state.id, rebound.generation)
        .unwrap();
    assert!(refreshed.context.is_none());
    assert_eq!(
        reopened
            .bind_context(&authority, "bind", &state.id, 0, &context.change)
            .unwrap(),
        bound
    );
    drop(authority);
    let db = rusqlite::Connection::open(data.path().join("attachments/state.sqlite")).unwrap();
    let mut damaged = refreshed;
    damaged.change_id = None;
    damaged.context = None;
    db.execute(
        "UPDATE attachments SET state_json=?1 WHERE id=?2",
        rusqlite::params![serde_json::to_vec(&damaged).unwrap(), state.id],
    )
    .unwrap();
    assert!(reopened.state(&state.id).is_err());
    let authority = Authority::open(data.path()).unwrap();
    assert!(
        reopened
            .bind_context(&authority, "bind", &state.id, 0, &context.change)
            .is_err()
    );
}

#[test]
fn attached_backup_is_explicitly_unqualified_even_when_closed() {
    let (repo, data, engine, journal) = fixture();
    journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "actor", SourceMode::Committed, &[]),
        )
        .unwrap();
    drop(journal);
    drop(engine);
    let destination = TempDir::new().unwrap();
    let error = agentscm::local_recovery::backup(
        data.path(),
        &destination.path().join("backup"),
        &[],
        &[],
        true,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("attached stores"), "{error}");
}

#[test]
fn failed_inspection_and_attach_do_not_disable_backup() {
    let data = TempDir::new().unwrap();
    drop(agentscm::authority::Authority::open(data.path()).unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data.path())
        .args(["attached-worktree", "state", "--id", &"0".repeat(64)])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!data.path().join("attachments").exists());
    let backup_root = TempDir::new().unwrap();
    agentscm::local_recovery::backup(
        data.path(),
        &backup_root.path().join("after-state"),
        &[],
        &[],
        true,
    )
    .unwrap();

    let (repo, _unused_data, _unused_engine, _unused_journal) = fixture();
    let data = TempDir::new().unwrap();
    let authority = agentscm::authority::Authority::open(data.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    drop(authority);
    let engine = Engine::open(data.path()).unwrap();
    let journal = AttachmentJournal::open(data.path(), &repository).unwrap();
    assert!(
        journal
            .attach(
                &engine,
                "failing",
                &request(repo.path(), "actor", SourceMode::Index, &[]),
            )
            .is_err()
    );
    drop(journal);
    drop(engine);
    agentscm::local_recovery::backup(
        data.path(),
        &backup_root.path().join("after-attach"),
        &[],
        &[],
        true,
    )
    .unwrap();

    let malformed = TempDir::new().unwrap();
    drop(agentscm::authority::Authority::open(malformed.path()).unwrap());
    let attachments = malformed.path().join("attachments");
    fs::create_dir(&attachments).unwrap();
    fs::set_permissions(&attachments, fs::Permissions::from_mode(0o700)).unwrap();
    let db = rusqlite::Connection::open(attachments.join("state.sqlite")).unwrap();
    db.execute("CREATE TABLE unknown(value TEXT)", []).unwrap();
    drop(db);
    fs::set_permissions(
        attachments.join("state.sqlite"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(
        agentscm::local_recovery::backup(
            malformed.path(),
            &backup_root.path().join("malformed"),
            &[],
            &[],
            true,
        )
        .is_err()
    );
}

#[test]
fn journal_retained_operation_cap_and_strict_decode_fail_closed() {
    let (repo, data, engine, journal) = fixture();
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "actor", SourceMode::Committed, &[]),
        )
        .unwrap();
    let other = journal
        .attach(
            &engine,
            "attach-other",
            &request(repo.path(), "other", SourceMode::Committed, &[]),
        )
        .unwrap();
    let db = rusqlite::Connection::open(data.path().join("attachments/state.sqlite")).unwrap();
    let mut corrupt = serde_json::to_value(&state).unwrap();
    corrupt["unknown"] = true.into();
    db.execute(
        "UPDATE attachments SET state_json=?1 WHERE id=?2",
        rusqlite::params![serde_json::to_vec(&corrupt).unwrap(), state.id],
    )
    .unwrap();
    assert!(journal.state(&state.id).is_err());
    db.execute(
        "UPDATE attachments SET state_json=?1 WHERE id=?2",
        rusqlite::params![serde_json::to_vec(&state).unwrap(), state.id],
    )
    .unwrap();
    db.execute(
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<4095) INSERT INTO operations SELECT 'cap-'||x,'capture',?1,0,printf('%064d',0),x'7b7d' FROM n",
        [&state.id],
    )
    .unwrap();
    assert!(
        journal
            .capture(&engine, "full", &state.id, 0)
            .unwrap_err()
            .to_string()
            .contains("capacity")
    );
    assert_eq!(journal.state(&state.id).unwrap(), state);
    assert_eq!(
        journal
            .attach(
                &engine,
                "attach",
                &request(repo.path(), "actor", SourceMode::Committed, &[]),
            )
            .unwrap(),
        state
    );
    journal
        .capture(&engine, "other-noop", &other.id, 0)
        .unwrap();
}

#[test]
fn retained_capture_corruption_fails_closed() {
    let (repo, data, engine, journal) = fixture();
    let first = journal
        .attach(
            &engine,
            "attach-first",
            &request(repo.path(), "first", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    let second = journal
        .attach(
            &engine,
            "attach-second",
            &request(repo.path(), "second", SourceMode::Committed, &[]),
        )
        .unwrap();
    write(repo.path(), "a.txt", b"changed\n");
    let outcome = journal.capture(&engine, "captured", &first.id, 0).unwrap();
    let db = rusqlite::Connection::open(data.path().join("attachments/state.sqlite")).unwrap();
    let original = serde_json::to_vec(&outcome).unwrap();
    let mut corruptions = Vec::new();
    let mut swapped = outcome.clone();
    swapped.attachment = second;
    swapped.revision = swapped.attachment.candidate_revision.clone();
    corruptions.push(serde_json::to_vec(&swapped).unwrap());
    let mut revision = serde_json::to_value(&outcome).unwrap();
    revision["revision"] = "0".repeat(64).into();
    corruptions.push(serde_json::to_vec(&revision).unwrap());
    let mut historical = serde_json::to_value(&outcome).unwrap();
    historical["historical"] = false.into();
    corruptions.push(serde_json::to_vec(&historical).unwrap());
    let mut path = serde_json::to_value(&outcome).unwrap();
    path["changed_paths"] = serde_json::json!(["../escape"]);
    corruptions.push(serde_json::to_vec(&path).unwrap());
    for corruption in corruptions {
        db.execute(
            "UPDATE operations SET outcome_json=?1 WHERE operation='captured'",
            [corruption],
        )
        .unwrap();
        assert!(journal.capture(&engine, "captured", &first.id, 0).is_err());
    }
    db.execute(
        "UPDATE operations SET outcome_json=?1 WHERE operation='captured'",
        [&original],
    )
    .unwrap();
    db.execute(
        "DELETE FROM mappings WHERE id=?1 AND git_commit=?2",
        rusqlite::params![first.id, outcome.attachment.head_commit],
    )
    .unwrap();
    assert!(journal.capture(&engine, "captured", &first.id, 0).is_err());
}

#[test]
fn journal_initialization_recovers_empty_file_and_serializes_first_open() {
    let empty = TempDir::new().unwrap();
    drop(Engine::open(empty.path()).unwrap());
    let attachments = empty.path().join("attachments");
    fs::create_dir(&attachments).unwrap();
    fs::set_permissions(&attachments, fs::Permissions::from_mode(0o700)).unwrap();
    let path = attachments.join("state.sqlite");
    fs::File::create(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&attachments, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(AttachmentJournal::open_existing(empty.path(), "repository-1").is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(
        fs::metadata(&attachments).unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert!(AttachmentJournal::open(empty.path(), "repository-1").is_ok());

    let concurrent = TempDir::new().unwrap();
    drop(Engine::open(concurrent.path()).unwrap());
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| AttachmentJournal::open(concurrent.path(), "repository-1")))
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    });

    let unknown = TempDir::new().unwrap();
    drop(Engine::open(unknown.path()).unwrap());
    let attachments = unknown.path().join("attachments");
    fs::create_dir(&attachments).unwrap();
    fs::set_permissions(&attachments, fs::Permissions::from_mode(0o700)).unwrap();
    let db = rusqlite::Connection::open(attachments.join("state.sqlite")).unwrap();
    db.execute("CREATE TABLE foreign_data(value TEXT)", [])
        .unwrap();
    drop(db);
    fs::set_permissions(
        attachments.join("state.sqlite"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(AttachmentJournal::open(unknown.path(), "repository-1").is_err());
}

#[test]
fn untracked_exclusions_and_unselected_special_files_are_preserved() {
    use std::os::unix::fs::symlink;
    let (repo, _data, engine, journal) = fixture();
    write(repo.path(), ".gitignore", b"*.secret\n");
    write(repo.path(), "untracked.secret", b"synthetic private");
    write(repo.path(), "untracked.txt", b"explicit raw source");
    symlink("/outside", repo.path().join("unselected-link")).unwrap();
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(
                repo.path(),
                "actor",
                SourceMode::Worktree,
                &["a.txt", "untracked.txt", "untracked.secret"],
            ),
        )
        .unwrap();
    assert_eq!(
        engine
            .read_file(&state.candidate_revision, "untracked.txt")
            .unwrap(),
        b"explicit raw source"
    );
    assert!(
        engine
            .stat_file(&state.candidate_revision, "untracked.secret")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fs::read(repo.path().join("untracked.secret")).unwrap(),
        b"synthetic private"
    );
}

#[test]
fn source_data_overlap_and_old_selected_git_are_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let (repo, _data, _engine, _journal) = fixture();
    let data = repo.path().join("native-data");
    fs::create_dir(&data).unwrap();
    let engine = Engine::open(&data).unwrap();
    let journal = AttachmentJournal::open(&data, "repo").unwrap();
    assert!(
        journal
            .attach(
                &engine,
                "overlap",
                &request(repo.path(), "actor", SourceMode::Committed, &[])
            )
            .unwrap_err()
            .to_string()
            .contains("separate")
    );
    let separate = TempDir::new().unwrap();
    let engine = Engine::open(separate.path()).unwrap();
    let journal = AttachmentJournal::open(separate.path(), "repo").unwrap();
    let script = separate.path().join("old-git");
    let marker = separate.path().join("repository-command");
    fs::write(&script,format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'git version 2.44.0'; else touch '{}'; exit 1; fi\n",marker.display())).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let mut req = request(repo.path(), "actor", SourceMode::Committed, &[]);
    req.git = script;
    assert!(journal.attach(&engine, "oldgit", &req).is_err());
    assert!(!marker.exists());
}

#[test]
fn capture_and_refresh_compete_on_the_same_generation() {
    let (repo, data, engine, journal) = fixture();
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "a", SourceMode::Worktree, &["a.txt"]),
        )
        .unwrap();
    write(repo.path(), "a.txt", b"new");
    let results = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            let j = AttachmentJournal::open(data.path(), "repository-1").unwrap();
            let e = Engine::open(data.path()).unwrap();
            j.capture(&e, "capture", &state.id, 0).map(|o| o.attachment)
        });
        let b = scope.spawn(|| {
            let j = AttachmentJournal::open(data.path(), "repository-1").unwrap();
            let e = Engine::open(data.path()).unwrap();
            j.refresh(&e, "refresh", &state.id, 0)
        });
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(journal.state(&state.id).unwrap().generation, 1);
}

#[test]
fn configured_filters_and_hidden_index_entries_are_refused() {
    for flag in ["--skip-worktree", "--assume-unchanged"] {
        let (repo, _data, engine, journal) = fixture();
        git(repo.path(), &["update-index", flag, "a.txt"]);
        assert!(
            journal
                .attach(
                    &engine,
                    "attach",
                    &request(repo.path(), "a", SourceMode::Worktree, &["a.txt"])
                )
                .unwrap_err()
                .to_string()
                .contains("hide")
        );
    }
    let (repo, _data, engine, journal) = fixture();
    git(repo.path(), &["config", "filter.synthetic.clean", "false"]);
    assert!(
        journal
            .attach(
                &engine,
                "attach",
                &request(repo.path(), "a", SourceMode::Worktree, &["a.txt"])
            )
            .unwrap_err()
            .to_string()
            .contains("filters")
    );
}

#[test]
fn cli_reports_historical_capture_and_requires_generation() {
    let (repo, _data, _engine, _journal) = fixture();
    let data = TempDir::new().unwrap();
    drop(agentscm::authority::Authority::open(data.path()).unwrap());
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--data-dir")
            .arg(data.path())
            .arg("attached-worktree")
            .args(args)
            .output()
            .unwrap()
    };
    let attached = cli(&[
        "attach",
        "--git",
        isolated_git(repo.path()).to_str().unwrap(),
        "--repository",
        repo.path().to_str().unwrap(),
        "--actor",
        "a",
        "--target",
        "main",
        "--output",
        "feature",
        "--source",
        "worktree",
        "--path",
        "a.txt",
        "--operation-id",
        "attach",
    ]);
    assert!(
        attached.status.success(),
        "{}",
        String::from_utf8_lossy(&attached.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&attached.stdout).unwrap();
    let id = json["attachment"]["id"].as_str().unwrap();
    assert_eq!(json["attachment"]["source"], "worktree_raw");
    assert!(
        !cli(&["capture", "--id", id, "--operation-id", "capture"])
            .status
            .success()
    );
    let captured = cli(&[
        "capture",
        "--id",
        id,
        "--expected-generation",
        "0",
        "--operation-id",
        "capture",
    ]);
    assert!(
        captured.status.success(),
        "{}",
        String::from_utf8_lossy(&captured.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&captured.stdout).unwrap();
    assert_eq!(value["capture"]["historical"], true);
    assert_eq!(value["capture"]["no_op"], true);
}

#[test]
fn retained_attach_envelope_and_historical_generation_fail_closed() {
    let (repo, data, engine, journal) = fixture();
    let request = request(repo.path(), "first", SourceMode::Worktree, &["a.txt"]);
    let first = journal.attach(&engine, "first", &request).unwrap();
    let mut other_request = request.clone();
    other_request.actor = "second".into();
    let second = journal.attach(&engine, "second", &other_request).unwrap();
    let db = rusqlite::Connection::open(data.path().join("attachments/state.sqlite")).unwrap();
    db.execute(
        "UPDATE operations SET attachment=?1 WHERE operation='first'",
        [&second.id],
    )
    .unwrap();
    assert!(journal.attach(&engine, "first", &request).is_err());
    db.execute(
        "UPDATE operations SET attachment=?1 WHERE operation='first'",
        [&first.id],
    )
    .unwrap();
    write(repo.path(), "a.txt", b"advanced");
    let captured = journal.capture(&engine, "capture", &first.id, 0).unwrap();
    write(repo.path(), "a.txt", b"advanced again");
    journal.capture(&engine, "later", &first.id, 1).unwrap();
    assert_eq!(journal.attach(&engine, "first", &request).unwrap(), first);
    assert_eq!(
        journal.capture(&engine, "capture", &first.id, 0).unwrap(),
        captured
    );
    // Keep the retained capture's generation shape intact, but rewind current state.
    db.execute(
        "UPDATE attachments SET generation=0,state_json=?1 WHERE id=?2",
        rusqlite::params![serde_json::to_vec(&first).unwrap(), first.id],
    )
    .unwrap();
    assert!(journal.capture(&engine, "capture", &first.id, 0).is_err());
}

#[test]
fn existing_journal_inspection_does_not_write_or_wait_for_reserved_writer() {
    let (repo, data, engine, journal) = fixture();
    let state = journal
        .attach(
            &engine,
            "attach",
            &request(repo.path(), "actor", SourceMode::Committed, &[]),
        )
        .unwrap();
    let directory = data.path().join("attachments");
    let path = directory.join("state.sqlite");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
    let before = fs::read(&path).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let opened = AttachmentJournal::open_existing(data.path(), "repository-1").unwrap();
    assert_eq!(opened.state(&state.id).unwrap(), state);
    assert!(opened.state(&"0".repeat(64)).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o750
    );
    db.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn local_remote_attachment_retains_transport_canonical_identity() {
    let (repo, _data, engine, journal) = fixture();
    let remote = TempDir::new().unwrap();
    git(remote.path(), &["init", "--bare", "--template="]);
    git(
        repo.path(),
        &[
            "remote",
            "set-url",
            "origin",
            remote.path().to_str().unwrap(),
        ],
    );
    let state = journal
        .attach(
            &engine,
            "local-identity",
            &request(repo.path(), "alice", SourceMode::Committed, &[]),
        )
        .unwrap();
    assert_eq!(
        PathBuf::from(&state.remote_url),
        remote.path().canonicalize().unwrap()
    );
    assert!(
        journal
            .validate_current(&engine, &state.id, state.generation)
            .is_ok()
    );
}
