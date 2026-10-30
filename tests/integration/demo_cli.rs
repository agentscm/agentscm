//! `agentscm demo --once` against a synthetic Git repository: the scripted refusal and repeat
//! happen over the real hook endpoint, then a handoff over MCP, and neither the repository nor
//! the temporary directory is left changed.
use std::path::Path;
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Demo", "-c", "user.email=demo@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Runs the demo with its temporary files directed at `tmp`.
fn demo(dir: &Path, tmp: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(["demo", "--once", "--no-open"])
        .current_dir(dir)
        .env("TMPDIR", tmp)
        .env("TEMP", tmp)
        .env("TMP", tmp)
        .output()
        .unwrap()
}

/// A repository whose most often changed file is `busiest`.
fn repository(root: &Path, busiest: &str) {
    git(root, &["init", "-q"]);
    for (round, others) in [
        ("one", vec!["beta.txt"]),
        ("two", vec!["gamma.txt"]),
        ("three", vec!["beta.txt", "gamma.txt"]),
    ] {
        for file in others.iter().copied().chain([busiest]) {
            std::fs::write(root.join(file), round).unwrap();
        }
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", round]);
    }
}

fn check_demo(busiest: &str) {
    let dir = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    repository(dir.path(), busiest);

    let output = demo(dir.path(), tmp.path());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout.lines().next(),
        Some("Simulated agents, real hub, your file names.")
    );
    let refusal = stdout
        .lines()
        .find(|line| line.starts_with("     \"AgentSCM: "))
        .unwrap_or_else(|| panic!("no refusal in {stdout}"));
    assert!(refusal.contains(busiest), "{refusal}");
    assert!(stdout.contains("let through"), "{stdout}");
    assert!(
        stdout
            .lines()
            .last()
            .is_some_and(|line| line.contains("agentscm quickstart")),
        "{stdout}"
    );
    // The repository has three files, so the handoff beat runs.
    assert!(
        stdout.lines().any(|line| line.starts_with("5. demo-riley (codex) is refused on ")
            && line.ends_with(
                ", asks demo-alex's agent to hand it over (experimental), and demo-alex hands it over; demo-riley's retry passes."
            )),
        "{stdout}"
    );

    let status = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(status.stdout.is_empty(), "{:?}", status.stdout);
    let left: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
    assert!(left.is_empty(), "temporary files left behind: {left:?}");
}

#[test]
fn demo_refuses_then_lets_a_repeat_through_without_writing() {
    check_demo("alpha.txt");
}

#[test]
fn a_file_named_deny_does_not_confuse_the_refusal_check() {
    check_demo("deny.toml");
}

#[test]
fn demo_outside_a_git_repository_explains_what_it_needs() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let output = demo(dir.path(), tmp.path());
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr
            .contains("run agentscm demo inside a Git repository with at least two tracked files"),
        "{stderr}"
    );
}
