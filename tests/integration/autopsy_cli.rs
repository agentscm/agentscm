//! The real binary's `autopsy`, over synthetic Claude Code logs and a temporary repository.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

const SECRET: &str = "SECRET_FILE_CONTENT";

fn iso(secs: i64) -> String {
    // Civil date from days since the epoch (Howard Hinnant).
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.123Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// One assistant line that edits `file` in `session`, `ago` seconds back.
fn edit_line(session: &str, branch: &str, file: &Path, ago: i64) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"{}","sessionId":"{session}","gitBranch":"{branch}","isSidechain":false,"message":{{"content":[{{"type":"tool_use","name":"Edit","input":{{"file_path":{},"old_string":"{SECRET}","new_string":"{SECRET}"}}}}]}}}}"#,
        iso(now() - ago),
        serde_json::to_string(&file.to_string_lossy()).unwrap(),
    )
}

fn write_log(logs: &Path, session: &str, lines: &[String]) {
    let dir = logs.join("-project");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{session}.jsonl")), lines.join("\n")).unwrap();
}

fn autopsy(repo: &Path, logs: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("autopsy")
        .arg("--repo")
        .arg(repo)
        .arg("--logs")
        .arg(logs)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn repo_with_logs(branches: (&str, &str)) -> (TempDir, TempDir) {
    let repo = TempDir::new().unwrap();
    fs::create_dir(repo.path().join(".git")).unwrap();
    let root = repo.path().canonicalize().unwrap();
    let file = root.join("src").join("a.rs");
    let logs = TempDir::new().unwrap();
    write_log(
        logs.path(),
        "s1",
        &[edit_line("s1", branches.0, &file, 3600)],
    );
    write_log(
        logs.path(),
        "s2",
        &[
            edit_line("s2", branches.1, &file, 3600 - 240),
            edit_line("s2", branches.1, Path::new("/elsewhere/x.rs"), 3000),
        ],
    );
    (repo, logs)
}

#[test]
fn reports_a_near_miss_without_leaking_contents() {
    let (repo, logs) = repo_with_logs(("feat-a", "feat-b"));
    let text = stdout(&autopsy(repo.path(), logs.path(), &[]));
    let card = text.lines().next().unwrap();
    assert!(
        card.starts_with(
            "7 days · 2 sessions · 1 near-miss · worst: src/a.rs (2 sessions, 4 min apart) · "
        ),
        "{card}"
    );
    assert!(text.contains("  src/a.rs — 1 near-miss\n"), "{text}");
    assert!(text.contains("the hub's 20-minute hold"), "{text}");
    assert!(text.contains("Read: 2 log files from "), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(!text.contains("elsewhere"), "{text}");

    let hidden = stdout(&autopsy(repo.path(), logs.path(), &["--no-paths"]));
    assert!(hidden.contains("worst: <file> (2 sessions"), "{hidden}");
    assert!(!hidden.contains("a.rs"), "{hidden}");
}

#[test]
fn subagent_logs_count_as_their_parent_session() {
    let repo = TempDir::new().unwrap();
    fs::create_dir(repo.path().join(".git")).unwrap();
    let file = repo.path().canonicalize().unwrap().join("a.rs");
    let logs = TempDir::new().unwrap();
    write_log(logs.path(), "s1", &[edit_line("s1", "a", &file, 600)]);
    let subagents = logs.path().join("-project").join("s2").join("subagents");
    fs::create_dir_all(&subagents).unwrap();
    fs::write(
        subagents.join("agent-1.jsonl"),
        edit_line("s2", "b", &file, 300),
    )
    .unwrap();
    let text = stdout(&autopsy(repo.path(), logs.path(), &[]));
    assert!(
        text.starts_with("7 days · 2 sessions · 1 near-miss"),
        "{text}"
    );
    assert!(text.contains("Read: 2 log files"), "{text}");
}

#[test]
fn file_names_cannot_reorder_or_escape_the_output() {
    let repo = TempDir::new().unwrap();
    fs::create_dir(repo.path().join(".git")).unwrap();
    let root = repo.path().canonicalize().unwrap();
    let logs = TempDir::new().unwrap();
    // Neither terminal controls nor hidden direction changes are paths the hub
    // would hold. A normal path still counts alongside these rejected names.
    let escape = root.join("esc\u{1b}[31m.rs");
    let bidi = root.join("evil\u{202e}.rs");
    let safe = root.join("safe.rs");
    write_log(
        logs.path(),
        "s1",
        &[
            edit_line("s1", "a", &escape, 600),
            edit_line("s1", "a", &bidi, 590),
            edit_line("s1", "a", &safe, 580),
        ],
    );
    write_log(
        logs.path(),
        "s2",
        &[
            edit_line("s2", "b", &escape, 300),
            edit_line("s2", "b", &bidi, 290),
            edit_line("s2", "b", &safe, 280),
        ],
    );
    let text = stdout(&autopsy(repo.path(), logs.path(), &[]));
    assert!(text.contains("1 near-miss"), "{text}");
    assert!(text.contains("safe.rs"), "{text}");
    assert!(!text.contains("evil") && !text.contains("esc"), "{text}");
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{202e}'),
        "{text:?}"
    );
}

#[test]
fn says_so_when_nothing_in_the_repository_was_edited() {
    let (repo, logs) = repo_with_logs(("a", "b"));
    let other = TempDir::new().unwrap();
    fs::create_dir(other.path().join(".git")).unwrap();
    let text = stdout(&autopsy(other.path(), logs.path(), &["--days", "1"]));
    assert!(
        text.starts_with("No edits to files in this repository were found in the last 1 day.\n"),
        "{text}"
    );
    assert!(text.contains("Read: 2 log files"), "{text}");
    drop(repo);
}

#[test]
fn logs_last_written_before_the_window_are_skipped() {
    let (repo, logs) = repo_with_logs(("a", "b"));
    let old = SystemTime::now() - std::time::Duration::from_secs(30 * 86_400);
    for log in ["s1", "s2"] {
        let file = fs::File::options()
            .write(true)
            .open(logs.path().join("-project").join(format!("{log}.jsonl")))
            .unwrap();
        file.set_modified(old).unwrap();
    }
    let text = stdout(&autopsy(repo.path(), logs.path(), &[]));
    assert!(
        text.starts_with("No edits to files in this repository"),
        "{text}"
    );
}

#[test]
fn days_must_be_between_1_and_90() {
    let (repo, logs) = repo_with_logs(("a", "b"));
    for days in ["0", "91"] {
        let out = autopsy(repo.path(), logs.path(), &["--days", days]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("1..=90"));
    }
}

#[cfg(unix)]
mod merge_tree {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let git = std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into());
        let out = Command::new(git)
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

    #[test]
    fn near_misses_on_one_branch_are_not_called_conflict_free() {
        let (repo, logs) = repo_with_logs(("main", "main"));
        let git_exe = std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into());
        let text = stdout(&autopsy(repo.path(), logs.path(), &["--git", &git_exe]));
        let card = text.lines().next().unwrap();
        assert!(card.ends_with("1 near-miss · worst: src/a.rs (2 sessions, 4 min apart) · merge-tree: nothing to check"), "{text}");
        assert!(
            text.contains("none of the near-misses were on two different local branches"),
            "{text}"
        );
    }

    #[test]
    fn only_a_file_that_conflicts_counts() {
        let repo = TempDir::new().unwrap();
        let root = repo.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        let body = |n: usize| {
            (0..30).map(|i| format!("line {i}\n")).collect::<String>() + &"x\n".repeat(n)
        };
        fs::write(root.join("clean.rs"), body(0)).unwrap();
        fs::write(root.join("clash.rs"), "one\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "base"]);
        for (branch, top, bottom, clash) in
            [("feat-a", "A", "", "two\n"), ("feat-b", "", "B", "three\n")]
        {
            git(&root, &["checkout", "-q", "-b", branch, "main"]);
            let text = format!("{top}{}{bottom}", body(0));
            fs::write(root.join("clean.rs"), text).unwrap();
            fs::write(root.join("clash.rs"), clash).unwrap();
            git(&root, &["commit", "-qam", branch]);
        }
        let logs = TempDir::new().unwrap();
        let (clean, clash) = (root.join("clean.rs"), root.join("clash.rs"));
        write_log(
            logs.path(),
            "s1",
            &[
                edit_line("s1", "feat-a", &clean, 900),
                edit_line("s1", "feat-a", &clash, 880),
            ],
        );
        write_log(
            logs.path(),
            "s2",
            &[
                edit_line("s2", "feat-b", &clean, 600),
                edit_line("s2", "feat-b", &clash, 580),
            ],
        );
        let git_exe = std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into());
        let text = stdout(&autopsy(&root, logs.path(), &["--git", &git_exe]));
        assert!(text.contains("2 near-misses"), "{text}");
        // clean.rs merges cleanly even though both branches changed it.
        assert!(text.contains("· merge-tree conflicts today: 1"), "{text}");
        assert!(
            text.contains("merge-tree check: 1 of 2 near-misses"),
            "{text}"
        );
    }

    #[test]
    fn counts_near_misses_that_conflict_in_git() {
        let repo = TempDir::new().unwrap();
        let root = repo.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "one\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "base"]);
        for (branch, text) in [("feat-a", "two\n"), ("feat-b", "three\n")] {
            git(&root, &["checkout", "-q", "-b", branch, "main"]);
            fs::write(root.join("src/a.rs"), text).unwrap();
            git(&root, &["commit", "-qam", branch]);
        }
        let logs = TempDir::new().unwrap();
        let file = root.join("src/a.rs");
        write_log(logs.path(), "s1", &[edit_line("s1", "feat-a", &file, 600)]);
        write_log(logs.path(), "s2", &[edit_line("s2", "feat-b", &file, 300)]);
        let git_exe = std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into());
        let text = stdout(&autopsy(&root, logs.path(), &["--git", &git_exe]));
        let card = text.lines().next().unwrap();
        assert!(
            card.ends_with("(2 sessions, 5 min apart) · merge-tree conflicts today: 1"),
            "{text}"
        );
        assert!(
            text.contains("merge-tree check: 1 of 1 near-miss"),
            "{text}"
        );
    }
}
