#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn private_tempdir() -> TempDir {
    let directory = TempDir::new().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn invoke(data: &TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data.path())
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn agent_task_help_exposes_typed_routes_and_recovery() {
    let data = private_tempdir();
    let output = invoke(&data, &["agent-task", "--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in [
        "key-generate",
        "key-info",
        "command-runtime-info",
        "review-transport-info",
        "enroll",
        "revoke",
        "profile-install",
        "inspect",
        "call",
        "upload",
        "collect",
        "retry-upload",
    ] {
        assert!(help.contains(command), "missing {command} in {help}");
    }
}

#[test]
fn key_generation_is_private_nonoverwriting_and_reports_enrollment_fields() {
    let data = private_tempdir();
    let key = data.path().join("collector.key");
    let generated = invoke(
        &data,
        &["agent-task", "key-generate", key.to_str().unwrap()],
    );
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    assert_eq!(
        fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(&key).unwrap().len(), 32);
    let value: Value = serde_json::from_slice(&generated.stdout).unwrap();
    assert_eq!(value["public_key_hex"].as_str().unwrap().len(), 64);
    assert_eq!(value["verification_key_id"].as_str().unwrap().len(), 64);
    assert_eq!(value["verification_key_sha256"].as_str().unwrap().len(), 64);

    let original = fs::read(&key).unwrap();
    let repeated = invoke(
        &data,
        &["agent-task", "key-generate", key.to_str().unwrap()],
    );
    assert!(!repeated.status.success());
    assert_eq!(fs::read(&key).unwrap(), original);
}

#[test]
fn key_reader_rejects_relative_permissive_and_oversized_inputs() {
    let data = private_tempdir();
    let relative = invoke(&data, &["agent-task", "key-info", "relative.key"]);
    assert!(!relative.status.success());

    let key = data.path().join("bad.key");
    fs::write(&key, [0_u8; 32]).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    let permissive = invoke(&data, &["agent-task", "key-info", key.to_str().unwrap()]);
    assert!(!permissive.status.success());

    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&key, [0_u8; 33]).unwrap();
    let oversized = invoke(&data, &["agent-task", "key-info", key.to_str().unwrap()]);
    assert!(!oversized.status.success());
}

#[test]
fn private_paths_reject_unsafe_leaf_links_and_immediate_parents() {
    let data = private_tempdir();
    let private = data.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let key = private.join("collector.key");
    fs::write(&key, [3_u8; 32]).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();

    let final_symlink = private.join("symlink.key");
    symlink(&key, &final_symlink).unwrap();
    assert!(
        !invoke(
            &data,
            &["agent-task", "key-info", final_symlink.to_str().unwrap()]
        )
        .status
        .success()
    );

    let hardlink = private.join("hardlink.key");
    fs::hard_link(&key, &hardlink).unwrap();
    assert!(
        !invoke(
            &data,
            &["agent-task", "key-info", hardlink.to_str().unwrap()]
        )
        .status
        .success()
    );
    fs::remove_file(&hardlink).unwrap();

    fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        !invoke(&data, &["agent-task", "key-info", key.to_str().unwrap()])
            .status
            .success()
    );
    let generated = private.join("generated.key");
    assert!(
        !invoke(
            &data,
            &["agent-task", "key-generate", generated.to_str().unwrap()]
        )
        .status
        .success()
    );
    assert!(!generated.exists());
}

#[test]
fn typed_call_rejects_unknown_operation_before_network() {
    let data = private_tempdir();
    let token = data.path().join("token");
    let input = data.path().join("request.json");
    fs::write(&token, "secret-token").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&input, r#"{"operation":"invented","subject":{}}"#).unwrap();
    let output = invoke(
        &data,
        &[
            "agent-task",
            "call",
            "--endpoint",
            "http://127.0.0.1:9",
            "--repository",
            "synthetic/project",
            "--profile",
            "checks",
            "--token-file",
            token.to_str().unwrap(),
            input.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let stderr: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        stderr["error"]
            .as_str()
            .unwrap()
            .contains("unknown variant")
    );
}
