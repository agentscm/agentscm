#![cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

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

fn git(dir: &Path, args: &[&str]) {
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
}

fn commit_file(dir: &Path, branch: &str, file: &str, text: &str) {
    fs::write(dir.join(file), text).unwrap();
    git(dir, &["commit", "-qam", branch]);
}

fn run(repo: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("collisions")
        .arg("--git")
        .arg(git_path())
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("PATH", "")
        .output()
        .unwrap()
}

fn report(repo: &Path, args: &[&str]) -> Value {
    let out = run(repo, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    value["collisions"].clone()
}

fn pairs(report: &Value) -> Vec<(String, String, String)> {
    report["collisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let items = c["items"].as_array().unwrap();
            (
                c["level"].as_str().unwrap().to_owned(),
                items[0].as_str().unwrap().to_owned(),
                items[1].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn reports_same_file_conflict_and_dirty_worktree_collisions() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("f"), "1\n2\n3\n4\n5\n6\n7\n").unwrap();
    fs::write(repo.join("h"), "1\n2\n3\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    // Same file, different lines: Git merges these cleanly.
    for (branch, text) in [
        ("feat-a", "A\n2\n3\n4\n5\n6\n7\n"),
        ("feat-b", "1\n2\n3\n4\n5\n6\nB\n"),
    ] {
        git(&repo, &["checkout", "-q", "-b", branch, "main"]);
        commit_file(&repo, branch, "f", text);
    }
    // Stacked on feat-a: one line of work, not a collision with feat-a.
    git(&repo, &["checkout", "-q", "-b", "feat-a2", "feat-a"]);
    commit_file(&repo, "feat-a2", "f", "A\n2\nA2\n4\n5\n6\n7\n");
    // Same line changed two ways: Git reports a conflict.
    for (branch, text) in [("feat-c", "1\nC\n3\n"), ("feat-d", "1\nD\n3\n")] {
        git(&repo, &["checkout", "-q", "-b", branch, "main"]);
        commit_file(&repo, branch, "h", text);
    }
    // A file on one side and a directory of the same name on the other: no equal path.
    git(&repo, &["checkout", "-q", "-b", "feat-e", "main"]);
    fs::write(repo.join("d"), "file\n").unwrap();
    git(&repo, &["add", "d"]);
    git(&repo, &["commit", "-qm", "feat-e"]);
    git(&repo, &["checkout", "-q", "-b", "feat-f", "main"]);
    fs::create_dir(repo.join("d")).unwrap();
    fs::write(repo.join("d/x"), "x\n").unwrap();
    git(&repo, &["add", "d"]);
    git(&repo, &["commit", "-qm", "feat-f"]);
    git(&repo, &["checkout", "-q", "main"]);
    // An agent that has not committed yet, in a linked worktree.
    let other = dir.path().join("other");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wip",
            other.to_str().unwrap(),
        ],
    );
    fs::write(other.join("f"), "1\n2\n3\n4\nWIP\n6\n7\n").unwrap();
    fs::write(other.join("untracked.txt"), "x").unwrap();

    let committed = report(&repo, &[]);
    assert_eq!(committed["base"]["name"], "main");
    assert_eq!(
        pairs(&committed),
        [
            ("conflict".into(), "feat-c".into(), "feat-d".into()),
            ("conflict".into(), "feat-e".into(), "feat-f".into()),
            ("same-file".into(), "feat-a".into(), "feat-b".into()),
            ("same-file".into(), "feat-a2".into(), "feat-b".into()),
        ]
    );
    let conflict = &committed["collisions"][0];
    assert_eq!(conflict["git_merge"]["conflicted_paths"][0], "h");
    assert_eq!(conflict["shared_paths"][0], "h");
    assert_eq!(committed["collisions"][2]["git_merge"]["result"], "clean");
    assert!(
        committed["notes"][0]
            .as_str()
            .unwrap()
            .contains("not proof")
    );

    let all = report(&repo, &["--worktrees"]);
    let wip = format!("worktree:{}", other.canonicalize().unwrap().display());
    let found = pairs(&all);
    assert_eq!(found.len(), 7, "{found:?}");
    assert!(found.contains(&("same-file".into(), "feat-a".into(), wip.clone())));
    assert!(found.contains(&("same-file".into(), "feat-b".into(), wip.clone())));
    let against_wip = &all["collisions"][3]["git_merge"];
    assert_eq!(all["collisions"][3]["items"][1], wip.as_str());
    assert_eq!(against_wip["result"], "not-evaluated");
    // The untracked file is a change but collides with nothing.
    let item = all["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == wip.as_str());
    assert_eq!(item.unwrap()["paths_total"], 2);

    let narrowed = report(&repo, &["--refs", "feat-c", "feat-d"]);
    assert_eq!(pairs(&narrowed).len(), 1);
}

#[test]
fn rejects_option_like_and_unknown_refs() {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "base"]);
    for args in [
        vec!["--base=--output=x"],
        vec!["--refs=--all"],
        vec!["--refs", "no-such-branch"],
    ] {
        let out = run(dir.path(), &args);
        assert!(!out.status.success(), "{args:?}");
        let error: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(error["ok"], false);
    }
    assert!(!dir.path().join("x").exists());
}

#[test]
fn reports_git_too_old_instead_of_guessing() {
    let dir = TempDir::new().unwrap();
    let fake = dir.path().join("git");
    fs::write(&fake, "#!/bin/sh\necho 'git version 2.30.0'\n").unwrap();
    fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(["collisions", "--git"])
        .arg(&fake)
        .arg("--git-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let error: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(error["error"].as_str().unwrap().contains("2.43"));
}

fn conflicting_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("h"), "1\n2\n3\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    for (branch, text) in [("feat-c", "1\nC\n3\n"), ("feat-d", "1\nD\n3\n")] {
        git(&repo, &["checkout", "-q", "-b", branch, "main"]);
        commit_file(&repo, branch, "h", text);
    }
    git(&repo, &["checkout", "-q", "main"]);
    repo
}

fn request(repo: PathBuf, git: PathBuf) -> agentscm::git_collisions::Request {
    agentscm::git_collisions::Request {
        git,
        repository: repo,
        base: Some("main".into()),
        refs: vec![],
        worktrees: false,
        active_days: None,
        include_merged: false,
        deadline: None,
        max_bytes: None,
    }
}

/// main plus: `ff` (merged by ancestry), `squashed` (same change as one commit on main),
/// `rebased` (cherry-picked onto main) and two live branches that conflict on `h`.
fn merged_repo(root: &Path) -> PathBuf {
    let repo = root.join("merged");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for (name, text) in [("a", "a\n"), ("b", "b\n"), ("c", "c\n"), ("h", "1\n2\n3\n")] {
        fs::write(repo.join(name), text).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    for (branch, file, text) in [
        ("ff", "a", "ff\n"),
        ("squashed", "b", "sq\n"),
        ("rebased", "c", "rb\n"),
    ] {
        git(&repo, &["checkout", "-q", "-b", branch, "main"]);
        commit_file(&repo, branch, file, text);
        git(&repo, &["checkout", "-q", "main"]);
    }
    git(&repo, &["merge", "-q", "--ff-only", "ff"]);
    // The squash and the rebase land on main as different commits with the same changes.
    git(&repo, &["checkout", "-q", "squashed", "--", "b"]);
    git(&repo, &["commit", "-qm", "squash squashed"]);
    git(&repo, &["cherry-pick", "rebased"]);
    for (branch, text) in [("live-x", "1\nX\n3\n"), ("live-y", "1\nY\n3\n")] {
        git(&repo, &["checkout", "-q", "-b", branch, "main"]);
        commit_file(&repo, branch, "h", text);
        git(&repo, &["checkout", "-q", "main"]);
    }
    repo
}

#[test]
fn branches_the_base_already_has_are_skipped_and_counted() {
    let dir = TempDir::new().unwrap();
    let repo = merged_repo(dir.path());
    let ids = |report: &Value| -> Vec<String> {
        report["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let mut req = request(repo, git_path());
    let default = collisions(&req);
    assert_eq!(ids(&default), ["live-x", "live-y"]);
    // ff (ancestry), squashed (squash merge) and rebased (cherry-pick) are left out and counted.
    assert_eq!(default["merged_refs_excluded"], 3);
    assert_eq!(default["inactive_refs_excluded"], 0);

    req.include_merged = true;
    let all = collisions(&req);
    assert_eq!(ids(&all), ["live-x", "live-y", "rebased", "squashed"]);
    // Ancestry-merged branches have nothing to compare and stay out; the field is not counted.
    assert_eq!(all["merged_refs_excluded"], 0);

    // Naming a ref asks for it, merged or not.
    req.include_merged = false;
    req.refs = vec!["squashed".into(), "live-x".into()];
    assert_eq!(ids(&collisions(&req)), ["live-x", "squashed"]);
}

#[test]
fn a_squash_merge_is_found_even_after_later_edits_to_the_same_file() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("squash");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let lines = |a: &str, b: &str, c: &str| {
        format!("{a}\nm1\nm2\nm3\nm4\nm5\nm6\nm7\n{b}\nm8\nm9\nm10\nm11\nm12\n{c}\n")
    };
    fs::write(repo.join("d"), lines("a", "b", "c")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    // Two commits on the branch, squashed into one on main, then main edits the file again.
    git(&repo, &["checkout", "-q", "-b", "feature", "main"]);
    commit_file(&repo, "one", "d", &lines("A", "b", "c"));
    commit_file(&repo, "two", "d", &lines("A", "B", "c"));
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("d"), lines("A", "B", "c")).unwrap();
    git(&repo, &["commit", "-qam", "squash feature (#1)"]);
    commit_file(&repo, "later", "d", &lines("A", "B", "C"));
    let req = request(repo, git_path());
    let report = collisions(&req);
    assert!(report["items"].as_array().unwrap().is_empty());
    assert_eq!(report["merged_refs_excluded"], 1);
}

/// The base's patch IDs are computed once and shared: branches forked at different points
/// search overlapping ranges of it, and each squash is still found.
#[test]
fn squash_merges_of_branches_forked_at_different_points_are_all_found() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("squashes");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let lines = |a: &str, b: &str| format!("{a}\n1\n2\n3\n4\n5\n6\n7\n8\n9\n{b}\n");
    fs::write(repo.join("d"), lines("a", "z")).unwrap();
    fs::write(repo.join("e"), lines("a", "z")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["checkout", "-q", "-b", "early", "main"]);
    commit_file(&repo, "early", "d", &lines("A", "z"));
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("f"), "f\n").unwrap();
    git(&repo, &["add", "f"]);
    git(&repo, &["commit", "-qm", "unrelated"]);
    git(&repo, &["checkout", "-q", "-b", "late", "main"]);
    commit_file(&repo, "late", "e", &lines("A", "z"));
    git(&repo, &["checkout", "-q", "-b", "live", "main"]);
    commit_file(&repo, "live", "e", &lines("a", "LIVE"));
    git(&repo, &["checkout", "-q", "main"]);
    // Each lands as a squash, then main edits both files again so their content differs.
    git(&repo, &["checkout", "-q", "late", "--", "e"]);
    git(&repo, &["commit", "-qm", "squash late (#2)"]);
    git(&repo, &["checkout", "-q", "early", "--", "d"]);
    git(&repo, &["commit", "-qm", "squash early (#1)"]);
    fs::write(repo.join("d"), lines("A", "Z")).unwrap();
    fs::write(repo.join("e"), lines("A", "Y")).unwrap();
    git(&repo, &["commit", "-qam", "later"]);
    let report = collisions(&request(repo, git_path()));
    let ids: Vec<&str> = report["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["live"]);
    assert_eq!(report["merged_refs_excluded"], 2);
}

#[test]
fn a_multi_commit_rebase_merge_is_found_even_after_later_edits() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("rebase");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let text = |a: &str, b: &str| format!("{a}\n1\n2\n3\n4\n5\n6\n7\n8\n9\n{b}\n");
    fs::write(repo.join("d"), text("a", "z")).unwrap();
    fs::write(repo.join("e"), "e\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["checkout", "-q", "-b", "feature", "main"]);
    commit_file(&repo, "one", "d", &text("A", "z"));
    commit_file(&repo, "two", "e", "E\n");
    git(&repo, &["checkout", "-q", "main"]);
    // Both commits land one by one, then main edits the same files again.
    git(&repo, &["cherry-pick", "feature~1", "feature"]);
    fs::write(repo.join("d"), text("A", "Z")).unwrap();
    fs::write(repo.join("e"), "EE\n").unwrap();
    git(&repo, &["commit", "-qam", "later"]);
    let report = collisions(&request(repo, git_path()));
    assert!(report["items"].as_array().unwrap().is_empty());
    assert_eq!(report["merged_refs_excluded"], 1);
}

#[test]
fn a_branch_whose_files_the_base_reached_another_way_is_found() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("same-content");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("f"), "o\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["checkout", "-q", "-b", "feature", "main"]);
    commit_file(&repo, "feature", "f", "X\n");
    git(&repo, &["checkout", "-q", "main"]);
    // Main gets to the same file content in two steps, so no commit has the branch's patch.
    commit_file(&repo, "step1", "f", "Y\n");
    commit_file(&repo, "step2", "f", "X\n");
    let report = collisions(&request(repo, git_path()));
    assert!(report["items"].as_array().unwrap().is_empty());
    assert_eq!(report["merged_refs_excluded"], 1);
}

#[test]
fn a_branch_with_work_the_base_lacks_is_not_called_merged() {
    let dir = TempDir::new().unwrap();
    let repo = merged_repo(dir.path());
    // Same file as the squashed branch, but with content main does not have.
    git(&repo, &["checkout", "-q", "-b", "more", "main"]);
    commit_file(&repo, "more", "b", "different\n");
    git(&repo, &["checkout", "-q", "main"]);
    let report = collisions(&request(repo, git_path()));
    let ids: Vec<&str> = report["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["live-x", "live-y", "more"]);
}

#[test]
fn output_budget_truncates_with_marker() {
    let dir = TempDir::new().unwrap();
    let mut req = request(conflicting_repo(dir.path()), git_path());
    let full = agentscm::git_collisions::collisions(&req).unwrap();
    assert!(full["truncated"].is_null());
    assert!(!full["collisions"].as_array().unwrap().is_empty());

    req.max_bytes = Some(1);
    let cut = agentscm::git_collisions::collisions(&req).unwrap();
    assert_eq!(cut["truncated"], "output");
    assert!(cut["collisions"].as_array().unwrap().is_empty());
}

#[test]
fn deadline_truncates_with_marker() {
    let dir = TempDir::new().unwrap();
    let repo = conflicting_repo(dir.path());
    // The Git layer clears the environment, so the wrapper uses absolute paths.
    let wrapper = dir.path().join("git-wrapper");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = merge-tree ] && exec /bin/sleep 30; done\nexec {} \"$@\"\n",
            git_path().display()
        ),
    )
    .unwrap();
    fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let mut req = request(repo, wrapper);
    req.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(10));

    let start = std::time::Instant::now();
    let report = agentscm::git_collisions::collisions(&req).unwrap();
    assert_eq!(report["truncated"], "deadline");
    assert!(start.elapsed() < std::time::Duration::from_secs(25));
}

#[test]
fn active_days_skips_and_counts_stale_branches() {
    let dir = TempDir::new().unwrap();
    let repo = conflicting_repo(dir.path());
    git(&repo, &["checkout", "-q", "-b", "stale", "main"]);
    fs::write(repo.join("h"), "1\nS\n3\n").unwrap();
    let out = Command::new(git_path())
        .current_dir(&repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(["-c", "commit.gpgsign=false", "commit", "-qam", "stale"])
        .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(out.status.success());
    git(&repo, &["checkout", "-q", "main"]);

    let mut req = request(repo, git_path());
    assert_eq!(collisions(&req)["inactive_refs_excluded"], 0);
    req.active_days = Some(14);
    let recent = collisions(&req);
    assert_eq!(recent["active_days"], 14);
    assert_eq!(recent["inactive_refs_excluded"], 1);
    assert!(
        recent["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["id"] != "stale")
    );
}

fn collisions(req: &agentscm::git_collisions::Request) -> Value {
    agentscm::git_collisions::collisions(req).unwrap()
}

#[test]
fn text_flag_prints_a_report_and_json_keeps_its_shape() {
    let dir = TempDir::new().unwrap();
    let repo = conflicting_repo(dir.path());
    let text = run(&repo, &["--text"]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.starts_with(
        "Checked 2 branches/worktrees against main · 1 pair touches the same files · 1 would conflict in git\n"
    ));
    assert!(text.contains("feat-c  ↔  feat-d   h\n"));

    let json = run(&repo, &["--json"]);
    assert!(json.status.success());
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(pairs(&value["collisions"]).len(), 1);
    // Piped output without a flag is the same JSON.
    assert_eq!(json.stdout, run(&repo, &[]).stdout);
    let both = run(&repo, &["--json", "--text"]);
    assert!(!both.status.success());
}

#[test]
fn text_mode_reports_a_bad_base_as_one_plain_line() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    let out = run(&repo, &["--text", "--base", "no-such-ref"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        !err.trim_start().starts_with('{') && err.trim().lines().count() == 1,
        "{err}"
    );
}

fn advisory(repo: &Path) -> agentscm::git_collisions::AdvisorySnapshot {
    agentscm::git_collisions::advisory_snapshot(
        &git_path(),
        repo,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
    )
    .unwrap()
}

#[test]
fn durable_branch_advisory_survives_claim_lifetime_and_includes_dirty_peers() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("shared.rs"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let peer = dir.path().join("peer");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "old-work",
            peer.to_str().unwrap(),
        ],
    );
    fs::write(peer.join("shared.rs"), "peer\n").unwrap();
    // A committed path remains visible long after any live claim TTL has elapsed.
    let out = Command::new(git_path())
        .current_dir(&peer)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qam",
            "old work",
        ])
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(out.status.success());
    let clean = advisory(&repo);
    assert!(clean.valid(clean.observed_at));
    assert_eq!(clean.touching("shared.rs").len(), 1);
    assert_eq!(clean.touching("shared.rs")[0].name, "old-work");
    assert!(clean.touching("shared.rs")[0].fork.is_some());
    assert_eq!(clean.touching("unrelated.rs").len(), 0);
    // The peer's own clean checkout does not collide with its own branch.
    assert!(advisory(&peer).touching("shared.rs").is_empty());
    fs::write(peer.join("shared.rs"), "dirty peer\n").unwrap();
    let dirty = advisory(&repo);
    assert!(
        dirty
            .touching("shared.rs")
            .iter()
            .any(|item| item.kind == "worktree")
    );
    assert!(
        dirty
            .touching("shared.rs")
            .iter()
            .any(|item| item.kind == "ref")
    );
    for path in [
        "../shared.rs",
        "/shared.rs",
        "a//b",
        "a/./b",
        "a\nfile",
        "a\\file",
    ] {
        assert!(dirty.touching(path).is_empty(), "{path:?}");
    }
    // A cache is historical: later peer paths do not silently become covered.
    fs::write(peer.join("later.rs"), "later\n").unwrap();
    assert!(dirty.touching("later.rs").is_empty());
    assert_eq!(advisory(&repo).touching("later.rs").len(), 1);
    assert!(!dirty.valid(dirty.observed_at + 4 * 60 * 60 + 1));
}

#[test]
fn branch_advisory_reports_unknown_base_and_honors_deadline() {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "base"]);
    git(dir.path(), &["checkout", "-qb", "master"]);
    fs::write(dir.path().join("x"), "x").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "other base"]);
    assert!(
        agentscm::git_collisions::advisory_snapshot(
            &git_path(),
            dir.path(),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string()
        .contains("ambiguous")
    );
    assert!(
        agentscm::git_collisions::advisory_snapshot(
            &git_path(),
            dir.path(),
            std::time::Instant::now(),
        )
        .is_err()
    );
}

#[test]
fn branch_advisory_bounds_the_version_probe_and_rejects_non_utf8_paths() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let slow = dir.path().join("git");
    fs::write(&slow, "#!/bin/sh\nexec /bin/sleep 10\n").unwrap();
    fs::set_permissions(&slow, fs::Permissions::from_mode(0o700)).unwrap();
    let start = std::time::Instant::now();
    assert!(
        agentscm::git_collisions::advisory_snapshot(
            &slow,
            dir.path(),
            start + std::time::Duration::from_millis(50),
        )
        .is_err()
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    let peer = dir.path().join("peer");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "peer",
            peer.to_str().unwrap(),
        ],
    );
    // Some Unix filesystems reject invalid UTF-8 filenames. Feed the actual Git
    // status parser byte output through a wrapper so this boundary is exercised
    // on those filesystems too, while all other Git operations remain real.
    let invalid_status = dir.path().join("git-invalid-status");
    let real_git = git_path().to_string_lossy().replace('\'', "'\\''");
    fs::write(
        &invalid_status,
        format!(
            "#!/bin/sh\nfor arg do\n  if [ \"$arg\" = status ]; then\n    printf '?? bad\\377\\000?? valid.txt\\000'\n    exit 0\n  fi\ndone\nexec '{real_git}' \"$@\"\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&invalid_status, fs::Permissions::from_mode(0o700)).unwrap();
    let snapshot = agentscm::git_collisions::advisory_snapshot(
        &invalid_status,
        &repo,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
    )
    .unwrap();
    assert!(snapshot.incomplete);
    assert_eq!(snapshot.peers.len(), 1);
    assert_eq!(snapshot.peers[0].kind, "worktree");
    assert_eq!(
        snapshot.peers[0].paths,
        std::collections::BTreeSet::from(["valid.txt".to_owned()])
    );
    assert_eq!(snapshot.touching("valid.txt").len(), 1);
    assert!(snapshot.touching("bad\u{fffd}").is_empty());
}

#[test]
fn branch_advisory_prioritizes_dirty_worktrees_and_newest_refs_under_cap() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("shared.rs"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["checkout", "-qb", "seed"]);
    commit_file(&repo, "seed", "shared.rs", "branch\n");
    let text = |args: &[&str]| {
        let out = Command::new(git_path())
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    let tree = text(&["rev-parse", "seed^{tree}"]);
    let base = text(&["rev-parse", "main"]);
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["branch", "-D", "seed"]);
    for index in 0..41 {
        let date = format!("2020-01-01T00:00:{index:02}Z");
        let out = Command::new(git_path())
            .current_dir(&repo)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit-tree",
                &tree,
                "-p",
                &base,
                "-m",
                "branch",
            ])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(out.status.success());
        let oid = String::from_utf8(out.stdout).unwrap();
        git(
            &repo,
            &[
                "update-ref",
                &format!("refs/heads/branch-{index:02}"),
                oid.trim(),
            ],
        );
    }
    let peer = dir.path().join("dirty-peer");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "dirty-peer",
            peer.to_str().unwrap(),
        ],
    );
    fs::write(peer.join("shared.rs"), "dirty\n").unwrap();
    let snapshot = advisory(&repo);
    assert!(snapshot.incomplete);
    assert!(snapshot.peers.len() <= 40);
    assert_eq!(snapshot.peers[0].kind, "worktree");
    assert!(snapshot.peers[0].paths.contains("shared.rs"));
    let refs: Vec<_> = snapshot
        .peers
        .iter()
        .filter(|peer| peer.kind == "ref")
        .collect();
    assert!(!refs.is_empty(), "at least the newest ref must be observed");
    // A deadline may stop this real-Git scan early, but every observed ref must
    // be a prefix of newest-first order after the dirty worktree takes priority.
    for (peer, index) in refs.iter().zip((2..=40).rev()) {
        assert_eq!(peer.name, format!("branch-{index:02}"));
    }
    assert!(!snapshot.peers.iter().any(|peer| peer.name == "branch-00"));
    assert_eq!(snapshot.touching("shared.rs")[0].kind, "worktree");
}

#[test]
fn branch_advisory_skips_own_status_without_changing_full_collision_scan() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("shared.rs"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let peer = dir.path().join("peer");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "peer",
            peer.to_str().unwrap(),
        ],
    );
    fs::write(repo.join("shared.rs"), "own dirty\n").unwrap();
    fs::write(peer.join("shared.rs"), "peer dirty\n").unwrap();
    let status_log = dir.path().join("status-paths");
    let wrapper = dir.path().join("git-status-log");
    let real_git = git_path().to_string_lossy().replace('\'', "'\\''");
    let quoted_log = status_log.to_string_lossy().replace('\'', "'\\''");
    fs::write(&wrapper, format!(
        "#!/bin/sh\ncwd=\nprevious=\nstatus=\nfor arg do\n  if [ \"$previous\" = '-C' ]; then cwd=$arg; fi\n  if [ \"$arg\" = status ]; then status=1; fi\n  previous=$arg\ndone\nif [ \"$status\" = 1 ]; then printf '%s\\n' \"$cwd\" >> '{quoted_log}'; fi\nexec '{real_git}' \"$@\"\n"
    )).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let snapshot = agentscm::git_collisions::advisory_snapshot(
        &wrapper,
        &repo,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(snapshot.touching("shared.rs")[0].kind, "worktree");
    let checked: Vec<_> = fs::read_to_string(&status_log)
        .unwrap()
        .lines()
        .map(|path| PathBuf::from(path).canonicalize().unwrap())
        .collect();
    assert_eq!(checked, vec![peer.canonicalize().unwrap()]);
    // The explicit full collision command still includes the current dirty tree.
    let full = agentscm::git_collisions::collisions(&agentscm::git_collisions::Request {
        git: wrapper,
        repository: repo.clone(),
        base: Some("main".into()),
        refs: Vec::new(),
        worktrees: true,
        active_days: None,
        include_merged: false,
        deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(10)),
        max_bytes: None,
    })
    .unwrap();
    assert_eq!(full["collisions"].as_array().unwrap().len(), 1);
    let checked: Vec<_> = fs::read_to_string(&status_log)
        .unwrap()
        .lines()
        .map(|path| PathBuf::from(path).canonicalize().unwrap())
        .collect();
    assert!(checked.contains(&repo.canonicalize().unwrap()));
}
