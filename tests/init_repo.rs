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
    for path in [
        ".mcp.json",
        ".claude/settings.json",
        "CLAUDE.md",
        "GEMINI.md",
        "AGENTS.md",
    ] {
        assert!(repo.path().join(path).exists(), "{path}");
    }
    assert!(!repo.path().join(".codex").exists());
    let mcp = std::fs::read_to_string(repo.path().join(".mcp.json")).unwrap();
    assert!(mcp.contains("https://hub.example.test/mcp"), "{mcp}");
    assert!(mcp.contains("${AGENTSCM_HUB_TOKEN}"));
    assert!(stdout(&first).contains("export AGENTSCM_HUB_TOKEN="));
    let instructions = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
    for required in [
        "### Workflow tasks",
        "task `capabilities` operation",
        "`server_schema_versions`",
        "Never run recovery or feedback text as a shell command",
        "operator provides an existing native store and exact revision",
        "`files`, `stat` and `read`",
        "workspace path-state",
        "workspace expand ROOT OPERATION_ID EXPECTED_GENERATION --path PATH",
        "review `--scope`",
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
fn yes_without_a_hub_url_or_a_repository_is_an_error() {
    let (repo, home) = checkout();
    let out = init_repo(repo.path(), home.path(), &["--yes"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--hub-url"), "{}", stderr(&out));

    let elsewhere = TempDir::new().unwrap();
    let out = init_repo(
        elsewhere.path(),
        home.path(),
        &["--yes", "--hub-url", "https://hub.example.test"],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Git repository"), "{}", stderr(&out));
}

#[cfg(unix)]
#[test]
fn client_setup_after_init_repo_names_the_file_and_leaves_it_unchanged() {
    for (agent, file) in [("claude", ".mcp.json"), ("cursor", ".cursor/mcp.json")] {
        let (repo, home) = checkout();
        let hub = "https://hub.example.test";
        let out = init_repo(
            repo.path(),
            home.path(),
            &["--yes", "--hub-url", hub, "--agents", agent],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        let before = std::fs::read_to_string(repo.path().join(file)).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["client", "setup", "--agent", agent, "--hub", hub])
            .current_dir(repo.path())
            .env_clear()
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let message = stderr(&out);
        assert!(!out.status.success());
        assert!(
            message.contains(file)
                && message.contains("AGENTSCM_HUB_TOKEN")
                && message.contains("Connect agents page cannot verify")
                && message.contains("remove only the `agentscm` entry"),
            "{message}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join(file)).unwrap(),
            before
        );
    }
}

#[test]
fn next_steps_give_a_token_command_that_parses_and_one_line_per_agent() {
    let (repo, home) = checkout();
    let out = init_repo(
        repo.path(),
        home.path(),
        &[
            "--yes",
            "--hub-url",
            "https://hub.example.test",
            "--repo",
            "o/n",
            "--agents",
            "gemini,cursor",
        ],
    );
    let text = stdout(&out);
    assert!(
        text.contains("hub login --hub https://hub.example.test"),
        "{text}"
    );
    assert!(
        text.contains("hub token create --name <token name> --repo o/n --ttl 30d"),
        "{text}"
    );
    assert!(
        text.contains("Gemini: only GEMINI.md") && text.contains("Cursor:"),
        "{text}"
    );
}
