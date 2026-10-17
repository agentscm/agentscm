//! Actual CLI invocation fixtures, source only until explicitly run.
#![cfg(target_os = "macos")]
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use agentscm::authority::Authority;
use agentscm::local_recovery::IssueWorkflowRecoveryConfig;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn invoke(temp: &TempDir, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .current_dir(temp.path())
        .arg("--data-dir")
        .arg(temp.path().join("unused-global-data"))
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("invoke registered setup CLI")
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    value
}

fn failure(output: Output, operation: &str, kind: &str) -> Value {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["operation"], operation);
    assert_eq!(value["error"]["kind"], kind);
    value
}

fn parent(temp: &TempDir) -> std::path::PathBuf {
    fs::canonicalize(temp.path()).unwrap()
}

fn begin(temp: &TempDir, root: &Path) -> String {
    let value = success(invoke(temp, &["setup", "begin", root.to_str().unwrap()]));
    assert_eq!(value["operation"], "begin");
    assert_eq!(value["next_operation"], "resume");
    assert_eq!(value["installation"]["root"], root.to_str().unwrap());
    assert_eq!(
        value["installation"]["issue_recovery_config_path"],
        root.join("issue-recovery.json").to_str().unwrap()
    );
    let id = value["installation"]["installation_id"].as_str().unwrap();
    assert_eq!(id.len(), 32);
    id.to_owned()
}

fn owned(temp: &TempDir, operation: &str, root: &Path, id: &str) -> Output {
    invoke(
        temp,
        &[
            "setup",
            operation,
            root.to_str().unwrap(),
            "--installation-id",
            id,
        ],
    )
}

fn receipt(root: &Path) -> Vec<u8> {
    fs::read(root.join(".agentscm-installation.json")).unwrap()
}

fn executable_digest(path: &Path) -> String {
    let mut file = File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut consumed = 0usize;
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        consumed += count;
        assert!(consumed <= 128 * 1024 * 1024);
        hash.update(&buffer[..count]);
    }
    format!("{:x}", hash.finalize())
}

#[test]
fn setup_cli_help_and_missing_command_do_not_initialize_default_data() {
    let temp = TempDir::new().unwrap();
    let help = invoke(&temp, &["setup", "--help"]);
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    for name in ["begin", "resume", "doctor", "self-check"] {
        assert!(text.contains(name));
    }
    let missing = invoke(&temp, &["setup"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(!temp.path().join("unused-global-data").exists());
    assert!(!temp.path().join(".agentscm").exists());
}

#[test]
fn setup_cli_begin_returns_identity_before_native_state_or_packaging() {
    let temp = TempDir::new().unwrap();
    let root = parent(&temp).join("installation");
    let id = begin(&temp, &root);
    let value: Value = serde_json::from_slice(&receipt(&root)).unwrap();
    assert_eq!(value["installation_id"], id);
    assert_eq!(value["progress"]["step"], "claimed");
    assert!(!root.join("data").exists());
    assert!(!root.join("bundle").exists());
    assert!(!temp.path().join("unused-global-data").exists());
}

#[test]
fn setup_cli_resume_packages_actual_cli_and_exact_retry_keeps_receipt() {
    let temp = TempDir::new().unwrap();
    let root = parent(&temp).join("installation");
    let id = begin(&temp, &root);
    let first = success(owned(&temp, "resume", &root, &id));
    assert_eq!(first["operation"], "resume");
    assert_eq!(first["report"]["native"]["status"], "observed");
    assert_eq!(first["report"]["service_health"]["status"], "not_run");
    assert_eq!(
        first["report"]["operator_authentication"]["status"],
        "not_configured"
    );
    assert_eq!(first["report"]["team"]["status"], "unsupported");
    let bytes = receipt(&root);
    let retry = success(owned(&temp, "resume", &root, &id));
    assert_eq!(retry["report"], first["report"]);
    assert_eq!(receipt(&root), bytes);
    assert_eq!(
        executable_digest(&root.join("bundle/agentscm")),
        executable_digest(Path::new(env!("CARGO_BIN_EXE_agentscm")))
    );
    assert!(!temp.path().join("unused-global-data").exists());
}

#[test]
fn setup_cli_post_claim_failure_keeps_the_public_begin_identity_usable_for_retry() {
    let temp = TempDir::new().unwrap();
    let root = parent(&temp).join("installation");
    let id = begin(&temp, &root);
    let lock = File::open(root.join(".agentscm-setup.lock")).unwrap();
    lock.try_lock().unwrap();
    failure(owned(&temp, "resume", &root, &id), "resume", "busy");
    drop(lock);
    let result = success(owned(&temp, "resume", &root, &id));
    assert_eq!(result["report"]["installation_id"], id);
    assert_eq!(result["report"]["model"]["status"], "not_configured");
    assert_eq!(result["report"]["host_resources"]["status"], "not_run");
    let actual = Authority::open_read_only(root.join("data"))
        .unwrap()
        .repository_id()
        .unwrap();
    assert_eq!(result["report"]["native"]["detail"], actual);
    assert!(root.join("bundle/manifest.json").is_file());
}

#[test]
fn setup_cli_doctor_claimed_root_reports_blocked_without_initializing_it() {
    let temp = TempDir::new().unwrap();
    let root = parent(&temp).join("installation");
    let id = begin(&temp, &root);
    let bytes = receipt(&root);
    let value = success(owned(&temp, "doctor", &root, &id));
    assert_eq!(
        value["report"]["native"],
        json!({"status":"blocked","reason":"native_not_initialized"})
    );
    assert_eq!(value["report"]["forge"]["status"], "not_configured");
    assert_eq!(value["report"]["self_check"]["status"], "not_run");
    assert_eq!(receipt(&root), bytes);
    assert!(!root.join("data").exists());
}

#[test]
fn setup_cli_wrong_id_has_typed_redacted_failure_and_preserves_claim() {
    let temp = TempDir::new().unwrap();
    let root = parent(&temp).join("installation");
    let id = begin(&temp, &root);
    let wrong = if id == "f".repeat(32) {
        "e".repeat(32)
    } else {
        "f".repeat(32)
    };
    let bytes = receipt(&root);
    let error = failure(
        owned(&temp, "resume", &root, &wrong),
        "resume",
        "foreign_installation",
    );
    assert!(!error["error"]["message"].as_str().unwrap().contains(&wrong));
    assert_eq!(receipt(&root), bytes);
    assert!(!root.join("data").exists());
}

#[test]
fn setup_cli_partial_native_initialization_retains_actual_identity_and_returns_blocker() {
    let temp = TempDir::new().unwrap();
    let root = parent(&temp).join("installation");
    let id = begin(&temp, &root);
    let mut value: Value = serde_json::from_slice(&receipt(&root)).unwrap();
    value["progress"] = json!({"step":"initializing_native"});
    fs::write(
        root.join(".agentscm-installation.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let initialized = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(root.join("data"))
        .arg("init")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let actual = success(initialized)["repository"]
        .as_str()
        .unwrap()
        .to_owned();
    let error = failure(owned(&temp, "resume", &root, &id), "resume", "blocked");
    assert_eq!(
        error["error"]["blocker"],
        "interrupted_native_initialization"
    );
    assert_eq!(
        Authority::open_read_only(root.join("data"))
            .unwrap()
            .repository_id()
            .unwrap(),
        actual
    );
    assert!(!root.join("bundle").exists());
}

#[test]
fn setup_cli_existing_symlink_relative_and_overlapping_roots_are_refused() {
    let temp = TempDir::new().unwrap();
    let parent = parent(&temp);
    let existing = parent.join("existing");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("keep"), b"untouched").unwrap();
    failure(
        invoke(&temp, &["setup", "begin", existing.to_str().unwrap()]),
        "begin",
        "existing_path",
    );
    let alias = parent.join("alias");
    symlink(&existing, &alias).unwrap();
    failure(
        invoke(&temp, &["setup", "begin", alias.to_str().unwrap()]),
        "begin",
        "invalid_path",
    );
    let owned_root = parent.join("owned");
    begin(&temp, &owned_root);
    failure(
        invoke(
            &temp,
            &[
                "setup",
                "begin",
                owned_root.join("nested").to_str().unwrap(),
            ],
        ),
        "begin",
        "invalid_path",
    );
    assert_eq!(
        invoke(&temp, &["setup", "begin", "relative"]).status.code(),
        Some(2)
    );
    assert_eq!(fs::read(existing.join("keep")).unwrap(), b"untouched");
    assert!(!temp.path().join("relative").exists());
}

#[test]
fn setup_cli_strict_arguments_and_unsupported_profiles_have_no_installation_effects() {
    let temp = TempDir::new().unwrap();
    let root = parent(&temp).join("installation");
    let path = root.to_str().unwrap();
    for arguments in [
        vec!["setup", "resume", path],
        vec!["setup", "doctor", path, "--installation-id", "ABC"],
        vec![
            "setup",
            "doctor",
            path,
            "--installation-id",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ],
        vec!["setup", "begin", path, "--profile", "unknown"],
        vec!["setup", "begin", path, "--provider-token", "not-a-token"],
    ] {
        let output = invoke(&temp, &arguments);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stderr).unwrap()["ok"],
            false
        );
    }
    for profile in ["team", "linux-service", "model-assisted"] {
        failure(
            invoke(&temp, &["setup", "begin", path, "--profile", profile]),
            "begin",
            "unsupported_profile",
        );
    }
    assert!(!root.exists());
    assert!(!temp.path().join("unused-global-data").exists());
}

#[test]
fn setup_cli_self_check_is_explicit_and_separate_from_installation_authority() {
    let temp = TempDir::new().unwrap();
    let root = parent(&temp).join("installation");
    let id = begin(&temp, &root);
    success(owned(&temp, "resume", &root, &id));
    assert!(!root.join("self-check").exists());
    let result = success(owned(&temp, "self-check", &root, &id));
    assert_eq!(result["report"]["self_check"]["status"], "recorded");
    let main = Authority::open_read_only(root.join("data")).unwrap();
    let sample = Authority::open_read_only(root.join("self-check/data")).unwrap();
    assert_ne!(
        main.repository_id().unwrap(),
        sample.repository_id().unwrap()
    );
    assert_eq!(fs::read_dir(root.join("data/objects")).unwrap().count(), 0);
    assert!(main.events(0, 10).unwrap().is_empty());
    assert!(main.target("main").unwrap().is_none());
    let report = success(owned(&temp, "doctor", &root, &id));
    assert_eq!(report["report"]["self_check"]["status"], "recorded");
    let repeat = failure(
        owned(&temp, "self-check", &root, &id),
        "self_check",
        "blocked",
    );
    assert_eq!(repeat["error"]["blocker"], "self_check_already_recorded");
}

#[test]
fn setup_cli_non_utf8_root_is_a_parse_error_without_installation_effects() {
    let temp = TempDir::new().unwrap();
    let root = parent(&temp).join(OsString::from_vec(b"invalid-\xff".to_vec()));
    let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .current_dir(temp.path())
        .args(["setup", "begin"])
        .arg(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!root.exists());
    assert!(!temp.path().join(".agentscm").exists());
}

#[test]
fn setup_cli_recovery_config_parser_roundtrip_and_mismatch_doctor_are_actual_and_external() {
    let temp = TempDir::new().unwrap();
    let root = parent(&temp).join("installation");
    let id = begin(&temp, &root);
    let result = success(owned(&temp, "resume", &root, &id));
    let path = root.join("issue-recovery.json");
    assert_eq!(
        result["report"]["issue_recovery_config_path"],
        path.to_str().unwrap()
    );
    assert_eq!(
        result["report"]["issue_recovery_config"]["status"],
        "observed"
    );
    let native = Authority::open_read_only(root.join("data"))
        .unwrap()
        .repository_id()
        .unwrap();
    let profile = IssueWorkflowRecoveryConfig::read(&path).unwrap();
    assert_eq!(profile.native_repository(), native);
    assert_eq!(profile.installation_id(), id);
    assert!(!root.join("data/issue-workflow").exists());
    assert!(!root.join("data/issue-recovery.json").exists());
    let mismatched = IssueWorkflowRecoveryConfig::new(&native, "other-installed-id").unwrap();
    let bytes = serde_json::to_vec(&mismatched).unwrap();
    fs::write(&path, &bytes).unwrap();
    assert_eq!(
        IssueWorkflowRecoveryConfig::read(&path).unwrap(),
        mismatched
    );
    let doctor = success(owned(&temp, "doctor", &root, &id));
    assert_eq!(
        doctor["report"]["issue_recovery_config_path"],
        path.to_str().unwrap()
    );
    assert_eq!(
        doctor["report"]["issue_recovery_config"],
        json!({"status":"blocked","reason":"issue_recovery_config_mismatch"})
    );
    let error = failure(owned(&temp, "resume", &root, &id), "resume", "blocked");
    assert_eq!(error["error"]["blocker"], "issue_recovery_config_mismatch");
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(
        Authority::open_read_only(root.join("data"))
            .unwrap()
            .repository_id()
            .unwrap(),
        native
    );
}
