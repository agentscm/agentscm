#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agentscm::authority::Authority;
use agentscm::issue_workflow::IssueJournal;
use agentscm::local_recovery::IssueWorkflowRecoveryConfig;
use agentscm::store::Engine;
use serde_json::Value;
use tempfile::TempDir;

const INSTALLATION: &str = "installation-cli";

fn run(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("PATH", "")
        .output()
        .unwrap()
}

fn ok(data: &Path, args: &[&str]) -> Value {
    let output = run(data, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn fail(data: &Path, args: &[&str]) -> Value {
    let output = run(data, args);
    assert!(
        !output.status.success(),
        "unexpected success {args:?}: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    serde_json::from_slice(&output.stderr).unwrap()
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

struct Fixture {
    _temporary: TempDir,
    root: PathBuf,
    data: PathBuf,
    config: PathBuf,
    native: String,
}

impl Fixture {
    fn new() -> Self {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().to_path_buf();
        let data = root.join("data");
        drop(Engine::open(&data).unwrap());
        let authority = Authority::open(&data).unwrap();
        let native = authority.repository_id().unwrap();
        drop(authority);
        drop(IssueJournal::open(&data, &native, INSTALLATION).unwrap());
        let config = root.join("issue-recovery.json");
        fs::write(
            &config,
            serde_json::to_vec(&IssueWorkflowRecoveryConfig::new(&native, INSTALLATION).unwrap())
                .unwrap(),
        )
        .unwrap();
        Self {
            _temporary: temporary,
            root,
            data,
            config,
            native,
        }
    }
}

#[test]
fn configured_cli_backup_verify_restore_activate_round_trip() {
    let fixture = Fixture::new();
    let backup = fixture.root.join("backup");
    ok(
        &fixture.data,
        &[
            "recovery",
            "backup",
            text(&backup),
            "--quiesced",
            "--issue-config",
            text(&fixture.config),
        ],
    );
    ok(
        &fixture.root.join("unavailable-old-data"),
        &[
            "recovery",
            "verify",
            text(&backup),
            "--issue-config",
            text(&fixture.config),
        ],
    );
    let restored = fixture.root.join("restored");
    let result = ok(
        &fixture.root.join("unavailable-old-data"),
        &[
            "recovery",
            "restore",
            text(&backup),
            text(&restored),
            "--issue-config",
            text(&fixture.config),
        ],
    );
    assert_eq!(result["quarantined"], true);
    let result = ok(
        &fixture.root.join("unavailable-old-data"),
        &[
            "recovery",
            "activate",
            text(&restored),
            "--old-writer-disabled",
            "--external-state-reconciled",
            "--issue-config",
            text(&fixture.config),
        ],
    );
    assert_eq!(result["policy_epoch"], 1);
    assert!(!restored.join(".agentscm-recovery/quarantine.json").exists());
    let journal = IssueJournal::open(&restored.join("data"), &fixture.native, INSTALLATION);
    assert!(journal.is_ok());
}

#[test]
fn cli_refuses_missing_malformed_mismatched_and_linked_configuration() {
    let fixture = Fixture::new();
    let legacy_output = fixture.root.join("legacy-output");
    fail(
        &fixture.data,
        &["recovery", "backup", text(&legacy_output), "--quiesced"],
    );
    assert!(!legacy_output.exists());

    let malformed = fixture.root.join("malformed.json");
    fs::write(&malformed, b"{}").unwrap();
    let malformed_output = fixture.root.join("malformed-output");
    fail(
        &fixture.data,
        &[
            "recovery",
            "backup",
            text(&malformed_output),
            "--quiesced",
            "--issue-config",
            text(&malformed),
        ],
    );
    assert!(!malformed_output.exists());

    let mismatch = fixture.root.join("mismatch.json");
    fs::write(
        &mismatch,
        serde_json::to_vec(
            &IssueWorkflowRecoveryConfig::new(&"f".repeat(32), INSTALLATION).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let mismatch_output = fixture.root.join("mismatch-output");
    fail(
        &fixture.data,
        &[
            "recovery",
            "backup",
            text(&mismatch_output),
            "--quiesced",
            "--issue-config",
            text(&mismatch),
        ],
    );
    assert!(!mismatch_output.exists());

    let linked = fixture.root.join("linked.json");
    std::os::unix::fs::symlink(&fixture.config, &linked).unwrap();
    let linked_output = fixture.root.join("linked-output");
    fail(
        &fixture.data,
        &[
            "recovery",
            "backup",
            text(&linked_output),
            "--quiesced",
            "--issue-config",
            text(&linked),
        ],
    );
    assert!(!linked_output.exists());
}
