use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

use agentscm::continuity::{ForgeIdentity, ForgeProvider, Journal};
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env_remove("GIT_NO_LAZY_FETCH")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn fixture() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/fixture/project.git",
        ],
    );
    fs::write(repo.join("tracked.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    git(&repo, &["checkout", "-b", "feature"]);
    let state = root.join("state");
    (temp, repo, state)
}
fn success_argv() -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), "exit 0".into()]
}

fn forge() -> ForgeIdentity {
    ForgeIdentity {
        provider: ForgeProvider::Github,
        host: "github.com".into(),
        repository: "fixture/project".into(),
    }
}
fn start(journal: &Journal) {
    journal
        .start("task", "main", "Retained synthetic task context", forge())
        .unwrap();
}

#[test]
fn restart_retains_context_and_exact_draft_commit_after_head_advances() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    let a = git(&repo, &["rev-parse", "HEAD"]);
    journal
        .draft(
            "task",
            "draft-a",
            "feature",
            "main",
            "Synthetic change",
            "Synthetic body",
        )
        .unwrap();
    drop(journal);
    fs::write(repo.join("tracked.txt"), "next\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "next"]);
    let journal = Journal::open(&state, &repo).unwrap();
    let status = journal.status("task").unwrap();
    assert_eq!(status.task.context, "Retained synthetic task context");
    assert_eq!(status.drafts[0].commit, a);
    assert_eq!(status.drafts[0].publication_kind, "pull_request");
    assert!(
        journal
            .revalidate("task")
            .unwrap()
            .local_blockers
            .iter()
            .any(|s| s.contains("earlier commit"))
    );
    assert!(
        journal
            .draft("task", "draft-a", "feature", "main", "Replacement", "")
            .is_err()
    );
    journal.cancel_draft("task", "draft-a").unwrap();
    assert!(journal.status("task").unwrap().drafts[0].cancelled);
}

#[test]
fn check_is_once_only_and_dirty_source_changes_make_evidence_stale() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    fs::write(repo.join("tracked.txt"), "dirty A\n").unwrap();
    let argv = vec!["/bin/sh".into(), "-c".into(), "printf observed".into()];
    let check = journal.check("task", "check-a", argv.clone(), 10).unwrap();
    assert_eq!(check.state, "finished");
    assert_eq!(check.exit_code, Some(0));
    assert_eq!(
        journal.revalidate("task").unwrap().checks[0].applicability,
        "current"
    );
    fs::write(repo.join("tracked.txt"), "dirty B\n").unwrap();
    assert_eq!(
        journal.revalidate("task").unwrap().checks[0].applicability,
        "stale"
    );
    assert!(
        journal
            .draft("task", "dirty", "feature", "main", "Dirty", "")
            .is_err()
    );
    let replay = journal.check("task", "check-a", argv, 10).unwrap();
    assert_eq!(replay.created_at, check.created_at);
    assert!(
        journal
            .check(
                "task",
                "check-a",
                vec!["/bin/sh".into(), "-c".into(), "exit 1".into()],
                10
            )
            .is_err()
    );
    assert!(!journal.revalidate("task").unwrap().publication_authorized);
    assert_eq!(
        journal.revalidate("task").unwrap().remote_freshness,
        "unknown"
    );
}

#[test]
fn failed_checks_and_timeouts_remain_explicit_and_not_rerun() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    let failed = journal
        .check(
            "task",
            "failed",
            vec!["/bin/sh".into(), "-c".into(), "exit 1".into()],
            10,
        )
        .unwrap();
    assert_eq!(failed.exit_code, Some(1));
    let timeout = journal
        .check(
            "task",
            "timed",
            vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            1,
        )
        .unwrap();
    assert_eq!(timeout.stop.as_deref(), Some("TimedOut"));
    drop(journal);
    let journal = Journal::open(&state, &repo).unwrap();
    assert_eq!(journal.status("task").unwrap().checks.len(), 2);
}

#[test]
fn unknown_reserved_attempt_survives_restart_and_cannot_be_deleted_or_retried() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    // Simulate the durable prelaunch record left by an interrupted process.
    let check = journal
        .check("task", "interrupted", success_argv(), 10)
        .unwrap();
    assert_eq!(check.exit_code, Some(0));
    drop(journal);
    let mut value = serde_json::to_value(check).unwrap();
    value["state"] = "unknown".into();
    value["finish"] = serde_json::Value::Null;
    let db = rusqlite::Connection::open(state.join("journal.sqlite3")).unwrap();
    db.execute(
        "UPDATE checks SET body=?1 WHERE id='interrupted'",
        [serde_json::to_string(&value).unwrap()],
    )
    .unwrap();
    drop(db);
    let mut journal = Journal::open(&state, &repo).unwrap();
    assert_eq!(
        journal
            .check("task", "interrupted", success_argv(), 10)
            .unwrap()
            .state,
        "unknown"
    );
    assert_eq!(
        journal.revalidate("task").unwrap().checks[0].applicability,
        "unknown"
    );
    assert!(journal.delete("task").is_err());
    journal.delete_discarding_unknown("task").unwrap();
    assert!(journal.status("task").is_err());
}

#[test]
fn journal_lock_rejects_concurrent_reservation_owner() {
    let (_temp, repo, state) = fixture();
    let first = Journal::open(&state, &repo).unwrap();
    start(&first);
    assert!(Journal::open(&state, &repo).is_err());
    drop(first);
    assert!(Journal::open(&state, &repo).is_ok());
}

#[test]
fn target_drift_and_origin_drift_are_blockers_without_network_activity() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    journal
        .draft(
            "task",
            "draft-before-drift",
            "feature",
            "main",
            "Synthetic draft",
            "",
        )
        .unwrap();
    fs::write(repo.join("tracked.txt"), "next").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "next"]);
    git(&repo, &["branch", "-f", "main", "HEAD"]);
    git(
        &repo,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/fixture/changed.git",
        ],
    );
    let result = journal.revalidate("task").unwrap();
    assert!(
        result
            .local_blockers
            .iter()
            .any(|s| s.contains("base ref moved"))
    );
    assert!(
        result
            .local_blockers
            .iter()
            .any(|s| s.contains("origin identity changed"))
    );
    assert_eq!(result.remote_freshness, "unknown");
}

#[test]
fn rejects_inside_checkout_paths_symlinks_unsafe_ids_and_large_text() {
    let (_temp, repo, state) = fixture();
    let inside = repo.join("new-state");
    assert!(Journal::open(&inside, &repo).is_err());
    assert!(!inside.exists());
    let parent = state.parent().unwrap();
    symlink(parent, parent.join("alias")).unwrap();
    assert!(Journal::open(&parent.join("alias/state"), &repo).is_err());
    let journal = Journal::open(&state, &repo).unwrap();
    assert!(
        journal
            .start("../escape", "main", "context", forge())
            .is_err()
    );
    assert!(
        journal
            .start("large", "main", &"x".repeat(32 * 1024 + 1), forge())
            .is_err()
    );
    assert!(
        journal
            .start("badref", "-danger", "context", forge())
            .is_err()
    );
    assert!(
        journal
            .start(
                "mismatch",
                "main",
                "context",
                ForgeIdentity {
                    repository: "fixture/wrong".into(),
                    ..forge()
                }
            )
            .is_err()
    );
}

#[test]
fn refuses_symlink_database_and_nonprivate_state_directory() {
    let (_temp, repo, state) = fixture();
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Journal::open(&state, &repo).is_err());
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let target = state.parent().unwrap().join("unrelated");
    fs::write(&target, "preserve").unwrap();
    symlink(&target, state.join("journal.sqlite3")).unwrap();
    assert!(Journal::open(&state, &repo).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "preserve");
}

#[test]
fn deletion_removes_only_selected_completed_task_and_private_spool() {
    let (_temp, repo, state) = fixture();
    let mut journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    journal
        .start("other", "main", "Retain this", forge())
        .unwrap();
    assert_eq!(
        journal
            .check("task", "check", success_argv(), 10)
            .unwrap()
            .exit_code,
        Some(0)
    );
    journal.delete("task").unwrap();
    assert!(journal.status("task").is_err());
    assert!(journal.status("other").is_ok());
    assert!(!state.join("spool-task").exists());
}

#[test]
fn explicit_gitlab_nested_identity_and_mr_draft_are_supported() {
    for origin in [
        "git@gitlab.example:group/subgroup/project.git",
        "ssh://git@gitlab.example:2222/group/subgroup/project.git",
        "https://gitlab.example:8443/group/subgroup/project.git",
    ] {
        let (_temp, repo, state) = fixture();
        git(&repo, &["remote", "set-url", "origin", origin]);
        let journal = Journal::open(&state, &repo).unwrap();
        journal
            .start(
                "task",
                "main",
                "GitLab task",
                ForgeIdentity {
                    provider: ForgeProvider::Gitlab,
                    host: "gitlab.example".into(),
                    repository: "group/subgroup/project".into(),
                },
            )
            .unwrap();
        assert_eq!(
            journal
                .draft("task", "mr", "feature", "main", "Synthetic MR", "")
                .unwrap()
                .publication_kind,
            "merge_request"
        );
    }
}

#[test]
fn cli_reads_bounded_context_file_and_idempotent_check_does_not_execute_twice() {
    let (_temp, repo, state) = fixture();
    let context = state.parent().unwrap().join("context.txt");
    fs::write(&context, "Saved context from UTF-8 file").unwrap();
    let marker = state.parent().unwrap().join("marker");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args([
                "continuity",
                "--repo",
                repo.to_str().unwrap(),
                "--state-dir",
                state.to_str().unwrap(),
            ])
            .args(args)
            .output()
            .unwrap()
    };
    let output = run(&[
        "start",
        "--id",
        "task",
        "--base",
        "main",
        "--context-file",
        context.to_str().unwrap(),
        "--provider",
        "github",
        "--repository",
        "fixture/project",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for _ in 0..2 {
        let output = run(&[
            "check",
            "--id",
            "task",
            "--operation",
            "once",
            "--",
            "/bin/sh",
            "-c",
            "printf x >> \"$1\"",
            "fixture",
            marker.to_str().unwrap(),
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["exit_code"], 0);
    }
    assert_eq!(fs::read_to_string(marker).unwrap(), "x");
    let failed = run(&[
        "check",
        "--id",
        "task",
        "--operation",
        "failed-cli",
        "--",
        "/bin/sh",
        "-c",
        "exit 1",
    ]);
    assert!(!failed.status.success());
    let failed_record: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(failed_record["exit_code"], 1);

    let status = run(&["status", "--id", "task"]);
    let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(value["task"]["context"], "Saved context from UTF-8 file");
    let refs = git(&repo, &["show-ref"]);
    assert!(!refs.contains("refs/agentscm"));
    fs::write(&context, vec![b'x'; 32 * 1024 + 1]).unwrap();
    let oversized = run(&[
        "start",
        "--id",
        "oversized",
        "--base",
        "main",
        "--context-file",
        context.to_str().unwrap(),
        "--provider",
        "github",
        "--repository",
        "fixture/project",
    ]);
    assert!(!oversized.status.success());
}

#[test]
fn unrelated_or_future_sqlite_state_is_rejected_without_mutating_it() {
    let (_temp, repo, state) = fixture();
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let path = state.join("journal.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES('preserve');",
    )
    .unwrap();
    drop(db);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&path).unwrap();
    assert!(Journal::open(&state, &repo).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::remove_file(&path).unwrap();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=2;").unwrap();
    drop(db);
    let before = fs::read(&path).unwrap();
    assert!(Journal::open(&state, &repo).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn killed_cli_leaves_durable_unknown_attempt_and_replay_does_not_launch() {
    use std::time::{Duration, Instant};
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    drop(journal);
    let marker = state.parent().unwrap().join("process-pid");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args([
            "continuity",
            "--repo",
            repo.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
            "check",
            "--id",
            "task",
            "--operation",
            "killed",
            "--",
            "/bin/sh",
            "-c",
            "echo $$ > \"$1\"; sleep 30",
            "fixture",
            marker.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let started = marker.exists();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(started, "explicit command did not reach fixture marker");
    let pid: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    // SAFETY: the PID came from this fixture's owned subprocess, whose process group
    // the bounded runner creates. Kill that group so the test leaves no worker behind.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let journal = Journal::open(&state, &repo).unwrap();
    let status = journal.status("task").unwrap();
    assert_eq!(status.checks[0].state, "unknown");
    fs::remove_file(&marker).unwrap();
    let prior = journal
        .check(
            "task",
            "killed",
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo $$ > \"$1\"; sleep 30".into(),
                "fixture".into(),
                marker.to_str().unwrap().into(),
            ],
            300,
        )
        .unwrap();
    assert_eq!(prior.state, "unknown");
    assert!(!marker.exists());
}

fn stage_interrupted_deletion(state: &Path) {
    // Retain exactly the durable marker an admitted deletion writes before cleanup.
    let db = rusqlite::Connection::open(state.join("journal.sqlite3")).unwrap();
    let body: String = db
        .query_row("SELECT body FROM tasks WHERE id='task'", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&body).unwrap();
    value["deletion_pending"] =
        serde_json::json!({"started_at": 1, "spool_identity": value["spool_identity"]});
    db.execute(
        "UPDATE tasks SET body=?1 WHERE id='task'",
        [serde_json::to_string(&value).unwrap()],
    )
    .unwrap();
}

#[test]
fn deletion_retries_after_spool_removed_and_sql_failure_without_losing_rows() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    assert_eq!(
        journal
            .check("task", "check", success_argv(), 10)
            .unwrap()
            .exit_code,
        Some(0)
    );
    drop(journal);
    let db = rusqlite::Connection::open(state.join("journal.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_delete BEFORE DELETE ON tasks BEGIN SELECT RAISE(ABORT,'fixture interruption after spool cleanup'); END;").unwrap();
    drop(db);
    let mut journal = Journal::open(&state, &repo).unwrap();
    assert!(journal.delete("task").is_err());
    assert!(!state.join("spool-task").exists());
    let status = journal.status("task").unwrap();
    assert!(status.task.deletion_pending.is_some());
    assert_eq!(status.checks.len(), 1);
    assert!(journal.check("task", "check", success_argv(), 10).is_err());
    assert!(
        journal
            .draft("task", "blocked", "feature", "main", "Synthetic", "")
            .is_err()
    );
    drop(journal);
    let db = rusqlite::Connection::open(state.join("journal.sqlite3")).unwrap();
    db.execute_batch("DROP TRIGGER fail_delete;").unwrap();
    drop(db);
    let mut journal = Journal::open(&state, &repo).unwrap();
    journal.delete("task").unwrap();
    assert!(journal.status("task").is_err());
}

#[test]
fn marked_partial_cleanup_resumes_without_identity_files_and_preserves_replacement() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    drop(journal);
    stage_interrupted_deletion(&state);
    let spool = state.join("spool-task");
    fs::remove_file(spool.join("capture-spool-v4.json")).unwrap();
    fs::remove_file(spool.join("spool.lock")).unwrap();
    let mut journal = Journal::open(&state, &repo).unwrap();
    assert!(
        journal
            .status("task")
            .unwrap()
            .task
            .deletion_pending
            .is_some()
    );
    journal.delete("task").unwrap();
    assert!(!spool.exists());
    start(&journal);
    drop(journal);
    stage_interrupted_deletion(&state);
    fs::rename(&spool, state.join("retained-original-spool")).unwrap();
    let replacement = agentscm::change_capture::LocalSpool::open(&spool).unwrap();
    fs::write(spool.join("preserve.txt"), "replacement source").unwrap();
    drop(replacement);
    let mut journal = Journal::open(&state, &repo).unwrap();
    assert!(journal.delete("task").is_err());
    assert_eq!(
        fs::read_to_string(spool.join("preserve.txt")).unwrap(),
        "replacement source"
    );
    assert!(
        journal
            .status("task")
            .unwrap()
            .task
            .deletion_pending
            .is_some()
    );
}

#[test]
fn unmarked_missing_spool_is_not_accepted_as_completed_cleanup() {
    let (_temp, repo, state) = fixture();
    let mut journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    fs::remove_dir_all(state.join("spool-task")).unwrap();
    assert!(journal.delete("task").is_err());
    assert!(!state.join("spool-task").exists());
    assert!(
        journal
            .status("task")
            .unwrap()
            .task
            .deletion_pending
            .is_none()
    );
}

#[test]
fn invalid_runner_requests_leave_no_attempt_or_launch() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    let marker = state.parent().unwrap().join("not-launched");
    let argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "touch \"$1\"".into(),
        "fixture".into(),
        marker.to_str().unwrap().into(),
    ];
    assert!(journal.check("task", "too-long", argv, 301).is_err());
    assert!(
        journal
            .check("task", "relative", vec!["true".into()], 10)
            .is_err()
    );
    assert!(journal.status("task").unwrap().checks.is_empty());
    assert!(!marker.exists());
}

#[test]
fn unpublished_initialization_residue_does_not_poison_journal_creation() {
    let (_temp, repo, state) = fixture();
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    // An incomplete temporary database is not a published journal. This fixture
    // tests residue handling, not interruption inside the initialization syscall.
    let interrupted = state.join(".continuity-init-interrupted");
    fs::write(&interrupted, "SQLite format 3\0partial").unwrap();
    fs::set_permissions(&interrupted, fs::Permissions::from_mode(0o600)).unwrap();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    drop(journal);
    assert!(Journal::open(&state, &repo).unwrap().status("task").is_ok());
    assert_eq!(
        fs::read_to_string(interrupted).unwrap(),
        "SQLite format 3\0partial"
    );
}

#[test]
fn failed_start_cleans_its_private_spool_but_preserves_unbound_residue() {
    let (_temp, repo, state) = fixture();
    fs::write(repo.join("tracked.txt"), "private dirty source").unwrap();
    let journal = Journal::open(&state, &repo).unwrap();
    assert!(
        journal
            .start("task", "missing", "context", forge())
            .is_err()
    );
    assert!(!state.join("spool-task").exists());
    drop(journal);
    let db = rusqlite::Connection::open(state.join("journal.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_start BEFORE INSERT ON tasks BEGIN SELECT RAISE(ABORT,'fixture failure after capture'); END;").unwrap();
    drop(db);
    let journal = Journal::open(&state, &repo).unwrap();
    assert!(journal.start("task", "main", "context", forge()).is_err());
    assert!(!state.join("spool-task").exists());
    assert!(journal.status("task").is_err());
    let residue = state.join("spool-task");
    fs::create_dir(&residue).unwrap();
    fs::write(residue.join("preserve"), "interrupted private source").unwrap();
    assert!(journal.start("task", "main", "context", forge()).is_err());
    assert_eq!(
        fs::read_to_string(residue.join("preserve")).unwrap(),
        "interrupted private source"
    );
}

#[test]
fn same_named_tag_does_not_change_draft_branch_identity() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    git(&repo, &["tag", "feature"]);
    journal
        .draft("task", "draft", "feature", "main", "Synthetic", "")
        .unwrap();
    assert!(
        journal
            .revalidate("task")
            .unwrap()
            .local_blockers
            .is_empty()
    );
}

#[test]
fn local_status_ignores_ambient_git_configuration() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    drop(journal);
    let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.bare")
        .env("GIT_CONFIG_VALUE_0", "true")
        .args([
            "continuity",
            "--repo",
            repo.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
            "status",
            "--id",
            "task",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["task"]["id"], "task");
}

#[test]
fn missing_base_object_is_not_fetched_from_a_promisor_remote() {
    let (_temp, repo, state) = fixture();
    let base = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("tracked.txt"), "promised source").unwrap();
    git(&repo, &["commit", "-am", "promised"]);
    let promised = git(&repo, &["rev-parse", "HEAD"]);
    let peer = state.parent().unwrap().join("peer.git");
    git(
        &repo,
        &[
            "clone",
            "--bare",
            "--no-hardlinks",
            ".",
            peer.to_str().unwrap(),
        ],
    );
    git(&repo, &["reset", "--hard", &base]);
    git(&repo, &["branch", "uncached", &promised]);
    git(&repo, &["remote", "add", "promise", peer.to_str().unwrap()]);
    git(&repo, &["config", "remote.promise.promisor", "true"]);
    git(
        &repo,
        &["config", "remote.promise.partialclonefilter", "blob:none"],
    );
    let object = repo
        .join(".git/objects")
        .join(&promised[..2])
        .join(&promised[2..]);
    fs::remove_file(&object).unwrap();
    let journal = Journal::open(&state, &repo).unwrap();
    assert!(
        journal
            .start("task", "uncached", "context", forge())
            .is_err()
    );
    assert!(!state.join("spool-task").exists());
    assert!(!object.exists());
    assert_eq!(
        fs::read_dir(repo.join(".git/objects/pack"))
            .unwrap()
            .count(),
        0
    );
    // Positive control: ordinary Git can lazily retrieve this exact missing object.
    assert_eq!(git(&repo, &["rev-parse", "uncached^{commit}"]), promised);
}

#[test]
fn missing_source_tree_is_not_fetched_by_shared_capture_paths() {
    let (_temp, repo, state) = fixture();
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let peer = state.parent().unwrap().join("peer.git");
    git(
        &repo,
        &[
            "clone",
            "--bare",
            "--no-hardlinks",
            ".",
            peer.to_str().unwrap(),
        ],
    );
    git(&repo, &["remote", "add", "promise", peer.to_str().unwrap()]);
    git(&repo, &["config", "remote.promise.promisor", "true"]);
    git(
        &repo,
        &["config", "remote.promise.partialclonefilter", "tree:0"],
    );
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    let object = repo.join(".git/objects").join(&tree[..2]).join(&tree[2..]);
    fs::remove_file(&object).unwrap();
    // The commit/base objects remain present, so the journal's reference reads
    // succeed. The shared capture path must fail locally on the missing tree.
    assert!(
        journal
            .start("new-task", "main", "context", forge())
            .is_err()
    );
    assert!(!state.join("spool-new-task").exists());
    assert!(
        journal
            .draft("task", "draft", "feature", "main", "Synthetic", "")
            .is_err()
    );
    assert!(journal.revalidate("task").is_err());
    assert!(journal.check("task", "check", success_argv(), 10).is_err());
    assert!(journal.status("task").unwrap().checks.is_empty());
    assert!(!object.exists());
    assert_eq!(
        fs::read_dir(repo.join(".git/objects/pack"))
            .unwrap()
            .count(),
        0
    );
    // Positive control: ordinary Git retrieves the same tree from the local peer.
    assert_eq!(git(&repo, &["cat-file", "-t", &tree]), "tree");
    assert!(
        object.exists()
            || fs::read_dir(repo.join(".git/objects/pack"))
                .unwrap()
                .count()
                > 0
    );
}

#[test]
fn saved_draft_source_branch_switch_detach_and_moved_ref_are_explicit_blockers() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    let saved = git(&repo, &["rev-parse", "HEAD"]);
    journal
        .draft("task", "draft", "feature", "main", "Synthetic draft", "")
        .unwrap();
    git(&repo, &["checkout", "-b", "other"]);
    assert!(
        journal
            .revalidate("task")
            .unwrap()
            .local_blockers
            .iter()
            .any(|s| s.contains("symbolic source branch"))
    );
    fs::write(repo.join("tracked.txt"), "other").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "other"]);
    let advanced = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "--detach", &saved]);
    git(&repo, &["branch", "-f", "feature", &advanced]);
    let blockers = journal.revalidate("task").unwrap().local_blockers;
    assert!(
        blockers
            .iter()
            .any(|s| s.contains("symbolic source branch"))
    );
    assert!(
        blockers
            .iter()
            .any(|s| s.contains("saved local source branch moved"))
    );
}

#[test]
fn new_draft_and_checks_use_their_own_base_after_historical_task_base_moves() {
    let (_temp, repo, state) = fixture();
    let journal = Journal::open(&state, &repo).unwrap();
    start(&journal);
    journal
        .draft("task", "old-draft", "feature", "main", "Old draft", "")
        .unwrap();
    assert_eq!(
        journal
            .check("task", "old-check", success_argv(), 10)
            .unwrap()
            .exit_code,
        Some(0)
    );
    fs::write(repo.join("tracked.txt"), "advance").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "advance"]);
    git(&repo, &["branch", "-f", "main", "HEAD"]);
    journal.cancel_draft("task", "old-draft").unwrap();
    journal
        .draft("task", "new-draft", "feature", "main", "New draft", "")
        .unwrap();
    assert_eq!(
        journal
            .check("task", "new-check", success_argv(), 10)
            .unwrap()
            .exit_code,
        Some(0)
    );
    let result = journal.revalidate("task").unwrap();
    assert!(result.local_blockers.is_empty());
    assert_eq!(
        result
            .checks
            .iter()
            .find(|c| c.operation_id == "old-check")
            .unwrap()
            .applicability,
        "stale"
    );
    assert_eq!(
        result
            .checks
            .iter()
            .find(|c| c.operation_id == "new-check")
            .unwrap()
            .applicability,
        "current"
    );
    assert!(!result.publication_authorized);
}
