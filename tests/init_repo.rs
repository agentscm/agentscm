//! The real binary's `init-repo`, with a temporary HOME so no real configuration is read.
use std::path::Path;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

fn init_repo(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("init-repo")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("APPDATA", home.join("config"))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn checkout() -> (TempDir, TempDir) {
    let repo = TempDir::new().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    (repo, TempDir::new().unwrap())
}

#[test]
fn without_a_terminal_it_needs_yes_and_says_which_flags() {
    let (repo, home) = checkout();
    let out = init_repo(
        repo.path(),
        home.path(),
        &["--hub-url", "https://hub.example.test"],
    );
    assert!(!out.status.success());
    let message = stderr(&out);
    for flag in ["--yes", "--hub-url", "--agents"] {
        assert!(message.contains(flag), "{message}");
    }
    assert!(!repo.path().join("AGENTS.md").exists());
}

#[test]
fn yes_writes_from_a_subdirectory_and_a_second_run_writes_nothing() {
    let (repo, home) = checkout();
    let sub = repo.path().join("src/deep");
    std::fs::create_dir_all(&sub).unwrap();
    let args = [
        "--yes",
        "--hub-url",
        "https://hub.example.test/",
        "--agents",
        "claude,gemini",
    ];
    let first = init_repo(&sub, home.path(), &args);
    assert!(first.status.success(), "{}", stderr(&first));
    for path in ["CLAUDE.md", "GEMINI.md", "AGENTS.md"] {
        assert!(repo.path().join(path).exists(), "{path}");
    }
    for path in [".mcp.json", ".claude", ".codex", ".gemini"] {
        assert!(!repo.path().join(path).exists(), "{path}");
    }
    let text = stdout(&first);
    for agent in ["claude", "gemini"] {
        assert!(
            text.contains(&format!(
                "agentscm client setup --agent {agent} --hub https://hub.example.test\n"
            )),
            "{text}"
        );
    }
    assert!(!text.contains("AGENTSCM_HUB_TOKEN"), "{text}");
    let instructions = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
    for required in [
        "### Workflow tasks",
        "task `capabilities` operation",
        "`server_schema_versions`",
        "Never run recovery or feedback text as a shell command",
        "operator provides an existing native store and exact revision (hub-only repositories skip this)",
        "`files`, `stat` and `read`",
        "workspace path-state",
        "workspace expand ROOT OPERATION_ID EXPECTED_GENERATION --path PATH",
        "the `agentscm` MCP server at `/mcp` does not list these tools",
        "git worktree add --no-checkout",
        "never reconfigure an existing worktree",
        "git sparse-checkout add",
        "git sparse-checkout disable",
        "not measured token, dollar, network or time savings",
    ] {
        assert!(
            instructions.contains(required),
            "missing instruction: {required}"
        );
    }

    let second = init_repo(&sub, home.path(), &args);
    assert!(second.status.success());
    let text = stdout(&second);
    assert!(text.contains("Nothing to write"), "{text}");
    assert!(
        !text.contains("  create ") && !text.contains("  update "),
        "{text}"
    );
}

#[test]
fn no_hub_url_is_needed_but_a_git_repository_is() {
    let (repo, home) = checkout();
    let out = init_repo(repo.path(), home.path(), &["--yes", "--agents", "cursor"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("agentscm client setup --agent cursor --hub <HUB_URL>"),
        "{}",
        stdout(&out)
    );
    let rule = std::fs::read_to_string(repo.path().join(".cursor/rules/agentscm.mdc")).unwrap();
    assert!(
        rule.contains("`agentscm client setup --agent cursor --hub <HUB_URL>`"),
        "{rule}"
    );
    assert!(!repo.path().join(".cursor/mcp.json").exists());

    let elsewhere = TempDir::new().unwrap();
    let out = init_repo(elsewhere.path(), home.path(), &["--yes"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Git repository"), "{}", stderr(&out));
}

#[test]
fn a_bad_saved_hub_url_warns_and_keeps_the_placeholder() {
    let (repo, home) = checkout();
    let out = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(["init-repo", "--yes", "--agents", "codex"])
        .current_dir(repo.path())
        .env_clear()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("AGENTSCM_HUB_URL", "not a url")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("Not using the saved hub URL"),
        "{}",
        stderr(&out)
    );
    let agents = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
    assert!(
        agents.contains("`agentscm client setup --agent codex --hub <HUB_URL>`"),
        "{agents}"
    );
    // An explicit --hub-url goes into committed files, so a bad one is still an error.
    let out = init_repo(
        repo.path(),
        home.path(),
        &["--yes", "--hub-url", "not a url"],
    );
    assert!(!out.status.success());
}
