#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

fn invoke(data: &Path, settings: &Path, flags: &[&str], rest: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
    command
        .arg("--data-dir")
        .arg(data)
        .args(["skill-workflow", "install-stop", "--settings"])
        .arg(settings)
        .args(flags);
    if !rest.is_empty() {
        command.arg("--").args(rest);
    }
    command.output().unwrap()
}

fn json_out(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn claude(workspace: &Path, change: &str) -> Vec<String> {
    let w = workspace.to_str().unwrap();
    [
        "claude-stop",
        change,
        "--workspace",
        w,
        "--docker",
        "/d",
        "--docker-socket",
        "/s",
        "--claude",
        "/c",
    ]
    .map(String::from)
    .to_vec()
}

fn refs(words: &[String]) -> Vec<&str> {
    words.iter().map(String::as_str).collect()
}

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        for name in ["data", "ws", "cfg"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        Self { root }
    }
    fn data(&self) -> std::path::PathBuf {
        self.root.path().join("data")
    }
    fn ws(&self) -> std::path::PathBuf {
        fs::canonicalize(self.root.path().join("ws")).unwrap()
    }
    fn settings(&self) -> std::path::PathBuf {
        self.root.path().join("cfg/settings.json")
    }
}

#[test]
fn install_is_previewed_idempotent_replaceable_and_removable() {
    let f = Fixture::new();
    let original = json!({"model":"m","hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"a"}]}],
        "Stop":[{"hooks":[{"type":"command","command":"keep"}]}]}});
    let text = serde_json::to_string_pretty(&original).unwrap() + "\n";
    fs::write(f.settings(), &text).unwrap();
    let one = claude(&f.ws(), "c1");
    let preview = json_out(&invoke(&f.data(), &f.settings(), &[], &refs(&one)));
    assert_eq!(
        (preview["action"].as_str(), preview["applied"].as_bool()),
        (Some("add"), Some(false))
    );
    assert_eq!(fs::read_to_string(f.settings()).unwrap(), text);

    let applied = json_out(&invoke(&f.data(), &f.settings(), &["--apply"], &refs(&one)));
    assert_eq!(applied["action"], "add");
    assert_eq!(applied["agent"], "claude");
    assert_eq!(applied["entry"]["hooks"][0]["args"][2], "skill-workflow");
    let first = fs::read(f.settings()).unwrap();
    let again = json_out(&invoke(&f.data(), &f.settings(), &["--apply"], &refs(&one)));
    assert_eq!(
        (again["action"].as_str(), again["applied"].as_bool()),
        (Some("unchanged"), Some(false))
    );
    assert_eq!(fs::read(f.settings()).unwrap(), first);

    let two = claude(&f.ws(), "c2");
    let replaced = json_out(&invoke(&f.data(), &f.settings(), &["--apply"], &refs(&two)));
    assert_eq!(replaced["action"], "replace");
    let now: Value = serde_json::from_slice(&fs::read(f.settings()).unwrap()).unwrap();
    assert_eq!(now["hooks"]["Stop"].as_array().unwrap().len(), 2);
    assert_eq!(now["hooks"]["Stop"][0], original["hooks"]["Stop"][0]);

    let removed = json_out(&invoke(
        &f.data(),
        &f.settings(),
        &["--apply", "--remove"],
        &[],
    ));
    assert_eq!(
        (removed["action"].as_str(), removed["removed"].as_u64()),
        (Some("remove"), Some(1))
    );
    let back: Value = serde_json::from_slice(&fs::read(f.settings()).unwrap()).unwrap();
    assert_eq!(back, original);
    let repeat = json_out(&invoke(
        &f.data(),
        &f.settings(),
        &["--apply", "--remove"],
        &[],
    ));
    assert_eq!(repeat["action"], "unchanged");
}

#[test]
fn new_file_is_private_and_removal_restores_empty_object() {
    let f = Fixture::new();
    let settings = f.root.path().join("new/dir/hooks.json");
    let words: Vec<String> = [
        "remote-codex-stop",
        "--endpoint",
        "https://h",
        "--repository",
        "o/r",
        "--profile",
        "p",
        "--token-file",
        "/tok en",
        "--workspace",
        f.ws().to_str().unwrap(),
        "--change",
        "c",
    ]
    .map(String::from)
    .to_vec();
    let out = json_out(&invoke(&f.data(), &settings, &["--apply"], &refs(&words)));
    assert_eq!(out["agent"], "codex");
    let command = out["entry"]["hooks"][0]["command"].as_str().unwrap();
    assert!(
        command.contains(" skill-workflow remote-codex-stop ") && command.contains("'/tok en'")
    );
    assert_eq!(
        fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
        0o600
    );
    json_out(&invoke(&f.data(), &settings, &["--apply", "--remove"], &[]));
    let back: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(back, json!({}));
}

#[test]
fn refuses_unsafe_or_invalid_requests_without_writing() {
    let f = Fixture::new();
    let words = claude(&f.ws(), "c");
    let inside = f.ws().join("sub/settings.json");
    assert!(
        !invoke(&f.data(), &inside, &["--apply"], &refs(&words))
            .status
            .success()
    );
    assert!(!f.ws().join("sub").exists());

    let target = f.root.path().join("cfg/real.json");
    fs::write(&target, "{}").unwrap();
    symlink(&target, f.settings()).unwrap();
    assert!(
        !invoke(&f.data(), &f.settings(), &["--apply"], &refs(&words))
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "{}");

    let broken = f.root.path().join("cfg/broken.json");
    fs::write(&broken, "{ // jsonc\n}").unwrap();
    assert!(
        !invoke(&f.data(), &broken, &["--apply"], &refs(&words))
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(&broken).unwrap(), "{ // jsonc\n}");

    let other = f.root.path().join("cfg/other.json");
    for rest in [
        vec!["check", "c"],
        vec![
            "claude-stop",
            "c",
            "--workspace",
            "rel",
            "--docker",
            "/d",
            "--docker-socket",
            "/s",
            "--claude",
            "/c",
        ],
    ] {
        let out = invoke(&f.data(), &other, &["--apply"], &rest);
        assert!(!out.status.success());
    }
    let out = invoke(&f.data(), &other, &[], &["check"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("remote-codex-stop"));
    assert!(
        !invoke(&f.data(), &other, &["--remove"], &refs(&words))
            .status
            .success()
    );
    assert!(!other.exists());
}

fn settings_json(f: &Fixture) -> Value {
    serde_json::from_slice(&fs::read(f.settings()).unwrap()).unwrap()
}

#[test]
fn install_and_remove_are_scoped_to_one_workspace() {
    let f = Fixture::new();
    let other = fs::canonicalize({
        let path = f.root.path().join("ws2");
        fs::create_dir(&path).unwrap();
        path
    })
    .unwrap();
    let (a, b) = (claude(&f.ws(), "c1"), claude(&other, "c1"));
    json_out(&invoke(&f.data(), &f.settings(), &["--apply"], &refs(&a)));
    let added = json_out(&invoke(&f.data(), &f.settings(), &["--apply"], &refs(&b)));
    assert_eq!(added["action"], "add");
    let both = fs::read(f.settings()).unwrap();
    assert_eq!(
        settings_json(&f)["hooks"]["Stop"].as_array().unwrap().len(),
        2
    );

    let again = json_out(&invoke(&f.data(), &f.settings(), &["--apply"], &refs(&a)));
    assert_eq!(
        (again["action"].as_str(), again["applied"].as_bool()),
        (Some("unchanged"), Some(false))
    );
    assert_eq!(fs::read(f.settings()).unwrap(), both);

    let removed = json_out(&invoke(
        &f.data(),
        &f.settings(),
        &[
            "--apply",
            "--remove",
            "--workspace",
            f.ws().to_str().unwrap(),
        ],
        &[],
    ));
    assert_eq!(removed["removed"], 1);
    let left = settings_json(&f);
    let stop = left["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 1);
    assert_eq!(stop[0]["hooks"][0]["args"][6], other.to_str().unwrap());

    let none = json_out(&invoke(
        &f.data(),
        &f.settings(),
        &[
            "--apply",
            "--remove",
            "--workspace",
            f.ws().to_str().unwrap(),
        ],
        &[],
    ));
    assert_eq!(none["action"], "unchanged");
    // --workspace without --remove is not accepted.
    assert!(
        !invoke(&f.data(), &f.settings(), &["--workspace", "/x"], &refs(&a))
            .status
            .success()
    );
}

#[test]
fn local_executor_paths_must_be_absolute() {
    let f = Fixture::new();
    let w = f.ws();
    for flag in ["--docker", "--docker-socket", "--claude"] {
        let mut words = claude(&w, "c");
        let at = words.iter().position(|word| word == flag).unwrap();
        words[at + 1] = "rel".into();
        let out = invoke(&f.data(), &f.settings(), &["--apply"], &refs(&words));
        assert!(!out.status.success(), "{flag}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("absolute"));
    }
    assert!(!f.settings().exists());
}

fn task(subcommand: &str, workspace: &Path, token: &str) -> Vec<String> {
    [
        subcommand,
        "c1",
        "--endpoint",
        "http://127.0.0.1:9",
        "--repository",
        "synthetic/project",
        "--profile",
        "checks",
        "--token-file",
        token,
        "--workspace",
        workspace.to_str().unwrap(),
    ]
    .map(String::from)
    .to_vec()
}

#[test]
fn task_stop_hooks_render_are_recognized_and_removable() {
    let f = Fixture::new();
    let claude_words = task("task-stop", &f.ws(), "/synthetic/agent.token");
    let applied = json_out(&invoke(
        &f.data(),
        &f.settings(),
        &["--apply"],
        &refs(&claude_words),
    ));
    assert_eq!(applied["agent"], "claude");
    assert!(
        applied["instructions"]
            .as_str()
            .unwrap()
            .contains("capabilities")
    );
    let hook = &settings_json(&f)["hooks"]["Stop"][0]["hooks"][0];
    assert_eq!(hook["args"][2], "skill-workflow");
    assert_eq!(hook["args"][3], "task-stop");
    assert!(hook.get("args").is_some());

    // Replacing for the same workspace with the Codex form recognizes the earlier entry.
    let codex_words = task("task-codex-stop", &f.ws(), "/synthetic/agent.token");
    let replaced = json_out(&invoke(
        &f.data(),
        &f.settings(),
        &["--apply"],
        &refs(&codex_words),
    ));
    assert_eq!(
        (replaced["agent"].as_str(), replaced["action"].as_str()),
        (Some("codex"), Some("replace"))
    );
    let codex = &settings_json(&f)["hooks"]["Stop"][0]["hooks"][0];
    assert!(codex.get("args").is_none());
    let command = codex["command"].as_str().unwrap();
    assert!(
        command.contains(" skill-workflow task-codex-stop c1 "),
        "{command}"
    );
    assert!(command.contains("--token-file /synthetic/agent.token"));

    let removed = json_out(&invoke(
        &f.data(),
        &f.settings(),
        &[
            "--apply",
            "--remove",
            "--workspace",
            f.ws().to_str().unwrap(),
        ],
        &[],
    ));
    assert_eq!(removed["removed"], 1);
    assert_eq!(settings_json(&f), json!({}));
}

#[test]
fn task_stop_requires_absolute_token_file_and_workspace() {
    let f = Fixture::new();
    for subcommand in ["task-stop", "task-codex-stop"] {
        let relative = task(subcommand, &f.ws(), "agent.token");
        let out = invoke(&f.data(), &f.settings(), &["--apply"], &refs(&relative));
        assert!(!out.status.success(), "{subcommand}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("--token-file must be an absolute"));
        let mut words = task(subcommand, &f.ws(), "/synthetic/agent.token");
        let at = words.iter().position(|word| word == "--workspace").unwrap();
        words[at + 1] = "relative-ws".into();
        let out = invoke(&f.data(), &f.settings(), &["--apply"], &refs(&words));
        assert!(!out.status.success(), "{subcommand}");
    }
    assert!(!f.settings().exists());
}
