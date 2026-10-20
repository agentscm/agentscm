#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

fn codex_stop(data: &std::path::Path, workspace: &std::path::Path, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(["skill-workflow", "codex-stop", "change-1", "--workspace"])
        .arg(workspace)
        .args(["--docker", "/d", "--docker-socket", "/s", "--claude", "/c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn counter(data: &std::path::Path) -> u64 {
    let files: Vec<_> = fs::read_dir(data.join("agent-stop"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    let state: Value = serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
    state["rounds"].as_u64().unwrap()
}

#[test]
fn unusable_local_setup_blocks_and_counts_without_docker() {
    let root = TempDir::new().unwrap();
    let data = root.path().join("data");
    fs::create_dir(&data).unwrap();
    let workspace = root.path().join("missing-workspace");
    let payload = |active: bool| {
        json!({"hook_event_name": "Stop", "session_id": "synthetic", "cwd": workspace,
            "stop_hook_active": active})
        .to_string()
    };
    for (active, rounds) in [(false, 1), (true, 2)] {
        let output = codex_stop(&data, &workspace, &payload(active));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let out: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(out["decision"], "block", "{out}");
        assert_eq!(counter(&data), rounds);
    }
}

#[test]
fn stop_subcommand_parse_errors_are_not_blocking() {
    let data = TempDir::new().unwrap();
    let data = data.path().to_str().unwrap();
    let code = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(args)
            .output()
            .unwrap()
            .status
            .code()
    };
    for args in [
        &[
            "skill-workflow",
            "--data-dir",
            data,
            "codex-stop",
            "--bogus",
        ][..],
        &[
            "--data-dir",
            "skill-workflow",
            "skill-workflow",
            "codex-stop",
            "--bogus",
        ],
        &[
            "--data-dir",
            data,
            "skill-workflow",
            "remote-stop",
            "--bogus",
        ],
    ] {
        assert_eq!(code(args), Some(1), "{args:?}");
    }
    for args in [
        &["setup", "skill-workflow", "codex-stop", "--bogus"][..],
        &["skill-workflow", "check", "--bogus"],
    ] {
        assert_eq!(code(args), Some(2), "{args:?}");
    }
}
