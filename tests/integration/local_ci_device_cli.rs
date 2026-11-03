//! `local-ci device` against temporary directories. Synthetic keys only; same host.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use agentscm::local_ci::{
    EnrollmentExpectation, EnrollmentStatement, OwnerIdentity, verify_enrollment,
};
use serde_json::Value;

fn cli(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .current_dir(cwd)
        .args(["local-ci", "device"])
        .args(args)
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A key directory must be owner-private; `tempdir` follows the umask.
fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn enroll_args<'a>(key: &'a str, generation: &'a str) -> Vec<&'a str> {
    vec![
        "enroll-request",
        "--key",
        key,
        "--device-id",
        "lcd_cli",
        "--repository",
        "github.com/acme/widgets",
        "--owner-provider",
        "github",
        "--owner-host",
        "github.com",
        "--owner-person",
        "7",
        "--generation",
        generation,
    ]
}

#[test]
fn generated_key_is_private_and_never_printed() {
    let dir = private_tempdir();
    let key = dir.path().canonicalize().unwrap().join("device.key");
    let key_arg = key.to_str().unwrap();
    let out = cli(dir.path(), &["key-generate", "--key", key_arg]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let private = fs::read(&key).unwrap();
    assert_eq!(private.len(), 32);
    assert!(!text(&out).contains(&hex(&private)));
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(json["public_key_hex"].is_string() && json["verification_key_id"].is_string());

    let request = cli(dir.path(), &enroll_args(key_arg, "1"));
    assert!(request.status.success(), "{}", text(&request));
    assert!(!text(&request).contains(&hex(&private)));
    // An existing key is never overwritten.
    assert!(
        !cli(dir.path(), &["key-generate", "--key", key_arg])
            .status
            .success()
    );
    assert_eq!(fs::read(&key).unwrap(), private);
}

#[test]
fn enroll_request_verifies_and_rejects_wrong_expectations() {
    let dir = private_tempdir();
    let key = dir.path().canonicalize().unwrap().join("device.key");
    let key_arg = key.to_str().unwrap();
    assert!(
        cli(dir.path(), &["key-generate", "--key", key_arg])
            .status
            .success()
    );
    let out = cli(dir.path(), &enroll_args(key_arg, "2"));
    assert!(out.status.success(), "{}", text(&out));
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    let statement: EnrollmentStatement = serde_json::from_value(json["statement"].clone()).unwrap();
    let signature = json["signature"].as_str().unwrap();
    let owner = OwnerIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        person_id: "7".into(),
    };
    let expected = |generation| EnrollmentExpectation {
        repository: "github.com/acme/widgets",
        owner: &owner,
        device_id: "lcd_cli",
        generation,
    };
    verify_enrollment(&statement, signature, expected(2)).unwrap();
    assert!(verify_enrollment(&statement, signature, expected(1)).is_err());
    let mut tampered = statement.clone();
    tampered.device_id = "lcd_other".into();
    assert!(verify_enrollment(&tampered, signature, expected(2)).is_err());
}

#[test]
fn a_key_path_inside_a_git_checkout_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let init = Command::new("git")
        .current_dir(&root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    assert!(init.status.success());

    // Refused before anything is written.
    let inside = root.join("device.key");
    let out = cli(&root, &["key-generate", "--key", inside.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("inside a Git checkout"),
        "{}",
        text(&out)
    );
    assert!(!inside.exists());

    // An existing key already inside the checkout is not used to sign.
    let outside = private_tempdir();
    let outside_key = outside.path().canonicalize().unwrap().join("device.key");
    let made = cli(
        outside.path(),
        &["key-generate", "--key", outside_key.to_str().unwrap()],
    );
    assert!(made.status.success(), "{}", text(&made));
    let moved = root.join("moved.key");
    fs::copy(&outside_key, &moved).unwrap();
    fs::set_permissions(&moved, fs::Permissions::from_mode(0o600)).unwrap();
    let out = cli(&root, &enroll_args(moved.to_str().unwrap(), "1"));
    assert!(!out.status.success());
    assert!(text(&out).contains("inside a Git checkout"));
    // The same key outside the checkout still signs when run from inside it.
    let out = cli(&root, &enroll_args(outside_key.to_str().unwrap(), "1"));
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn the_keys_own_checkout_is_found_wherever_the_command_runs() {
    let checkout = tempfile::tempdir().unwrap();
    let root = checkout.path().canonicalize().unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    let nested = root.join("keys");
    fs::create_dir(&nested).unwrap();
    let elsewhere = private_tempdir();

    // Run from outside the checkout: the key's own ancestors still refuse it.
    let inside = nested.join("device.key");
    let out = cli(
        elsewhere.path(),
        &["key-generate", "--key", inside.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(
        text(&out).contains("inside a Git checkout"),
        "{}",
        text(&out)
    );
    assert!(!inside.exists());

    // A clean path is accepted from the same directory.
    let clean = elsewhere.path().canonicalize().unwrap().join("device.key");
    let out = cli(
        elsewhere.path(),
        &["key-generate", "--key", clean.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out));
}
