use agentscm::authority::Authority;
use agentscm::context::{ChangeId, InputIdentity, ReviewContext};
use agentscm::local_recovery::{self, InventoryEntry, RecoveryManifest};
use agentscm::model::{Approval, FileChange};
use agentscm::store::Engine;
use agentscm::{Error, Result};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const UNSUPPORTED: &str =
    "issue-workflow recovery is unsupported until a typed journal adapter is available";

fn assert_unsupported<T>(result: Result<T>) {
    assert!(matches!(result, Err(Error::Invalid(message)) if message == UNSUPPORTED));
}

fn populated(data: &Path) {
    let engine = Engine::open(data).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "file.txt".into(),
                content: Some(b"retained source".to_vec()),
            }],
        )
        .unwrap();
    let mut authority = Authority::open(data).unwrap();
    let known = InputIdentity::Digest {
        sha256: "a".repeat(64),
    };
    let context = ReviewContext {
        version: 1,
        change: ChangeId {
            repository: authority.repository_id().unwrap(),
            id: "guard-fixture".into(),
        },
        change_version: 1,
        revision: revision.clone(),
        base: None,
        target: "main".into(),
        policy_epoch: 0,
        intent: "retain an unconsumed approval across recovery".into(),
        acceptance_criteria: vec!["unsupported journals never activate".into()],
        scope: vec!["file.txt".into()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: known.clone(),
        toolchain: known.clone(),
        environment: known,
        required_evidence: vec![],
        required_capabilities: vec!["agentscm.review-context/v1".into()],
    };
    let change = authority.start_change(&context).unwrap();
    authority
        .approve(&Approval {
            approval_id: "retained-approval".into(),
            target: "main".into(),
            revision,
            context_digest: change.context_digest,
            change: Some(change.id),
            change_version: Some(1),
            policy_epoch: 0,
        })
        .unwrap();
}

// Synthetic unsupported bytes: this fixture does not implement a journal schema.
fn namespace(data: &Path, kind: &str) -> Vec<String> {
    let root = data.join("issue-workflow");
    if kind == "file" {
        fs::write(&root, b"reserved namespace occupied by a file").unwrap();
        return vec!["data/issue-workflow".into()];
    }
    fs::create_dir(&root).unwrap();
    let names: &[&str] = match kind {
        "empty" => &[],
        "main" => &["state.sqlite"],
        "wal-only" => &["state.sqlite-wal"],
        "shm-only" => &["state.sqlite-shm"],
        "journal-only" => &["state.sqlite-journal"],
        "family" => &[
            "state.sqlite",
            "state.sqlite-wal",
            "state.sqlite-shm",
            "state.sqlite-journal",
        ],
        _ => panic!("unknown fixture"),
    };
    let mut paths = vec!["data/issue-workflow".into()];
    for name in names {
        fs::write(
            root.join(name),
            format!("retained unsupported bytes: {name}"),
        )
        .unwrap();
        paths.push(format!("data/issue-workflow/{name}"));
    }
    paths
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}
#[cfg(not(unix))]
fn mode(_: &fs::Metadata) -> u32 {
    0
}

fn add_inventory(root: &Path, manifest: &mut RecoveryManifest, paths: &[String]) {
    for relative in paths {
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(!metadata.file_type().is_symlink());
        let directory = metadata.is_dir();
        let bytes = if directory {
            Vec::new()
        } else {
            fs::read(&path).unwrap()
        };
        manifest.entries.push(InventoryEntry {
            path: relative.clone(),
            directory,
            bytes: bytes.len() as u64,
            sha256: if directory {
                String::new()
            } else {
                format!("{:x}", Sha256::digest(&bytes))
            },
            mode: mode(&metadata),
        });
    }
    manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
}

fn authority_state(data: &Path) -> (i64, i64, Option<i64>) {
    // Inspect a checkpointed copy so a read-only WAL opener cannot add sidecars
    // to the sealed inventory. Check the original before both observations so
    // an unintended activation write left only in WAL cannot escape this oracle.
    assert!(!data.join("state.sqlite-wal").exists());
    assert!(!data.join("state.sqlite-shm").exists());
    let inspection = TempDir::new().unwrap();
    let copy = inspection.path().join("state.sqlite");
    fs::copy(data.join("state.sqlite"), &copy).unwrap();
    let db = Connection::open_with_flags(copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.query_row("SELECT policy_epoch,(SELECT revoked FROM approvals WHERE approval_id='retained-approval'),(SELECT consumed_event FROM approvals WHERE approval_id='retained-approval') FROM authority_state WHERE singleton=1", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
}

#[test]
fn backup_rejects_any_reserved_namespace_before_output_creation() {
    for kind in [
        "empty",
        "main",
        "wal-only",
        "shm-only",
        "journal-only",
        "family",
        "file",
    ] {
        let temporary = TempDir::new().unwrap();
        let data = temporary.path().join("data");
        populated(&data);
        let paths = namespace(&data, kind);
        let retained: Vec<_> = paths
            .iter()
            .filter_map(|path| {
                let path = temporary.path().join(path);
                if path.is_file() {
                    Some((path.clone(), fs::read(path).unwrap()))
                } else {
                    None
                }
            })
            .collect();
        let output = temporary.path().join("backup");
        assert_unsupported(local_recovery::backup(&data, &output, &[], &[], true));
        assert!(
            !output.exists(),
            "unsupported input must fail before publishing output"
        );
        for (path, bytes) in retained {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }
}

#[cfg(unix)]
#[test]
fn backup_rejects_reserved_symlinks_including_dangling_links() {
    for dangling in [true, false] {
        let temporary = TempDir::new().unwrap();
        let data = temporary.path().join("data");
        populated(&data);
        let external = temporary.path().join("external");
        if !dangling {
            fs::create_dir(&external).unwrap();
            fs::write(external.join("retained"), b"outside data").unwrap();
        }
        let reserved = data.join("issue-workflow");
        std::os::unix::fs::symlink(&external, &reserved).unwrap();
        let output = temporary.path().join("backup");
        assert_unsupported(local_recovery::backup(&data, &output, &[], &[], true));
        assert!(!output.exists());
        assert!(
            fs::symlink_metadata(&reserved)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        if dangling {
            assert!(!external.exists());
        } else {
            assert_eq!(
                fs::read(external.join("retained")).unwrap(),
                b"outside data"
            );
        }
    }
}

#[test]
fn matching_legacy_inventory_rejects_verify_and_restore_before_destination_creation() {
    for kind in ["main", "wal-only", "empty"] {
        let temporary = TempDir::new().unwrap();
        let data = temporary.path().join("data");
        populated(&data);
        let backup = temporary.path().join("backup");
        let mut manifest = local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
        let payload = backup.join("payload");
        let paths = namespace(&payload.join("data"), kind);
        add_inventory(&payload, &mut manifest, &paths);
        fs::write(
            backup.join("recovery-manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        // Exact error proves normal inventory/schema checks passed first; these
        // are correctly inventoried pre-upgrade bytes, not checksum failures.
        assert_unsupported(local_recovery::verify(&backup));
        let destination = temporary.path().join("must-not-be-created");
        assert_unsupported(local_recovery::restore(&backup, &destination));
        assert!(!destination.exists());
    }
}

#[test]
fn matching_quarantined_legacy_restore_cannot_advance_epoch_or_revoke_approvals() {
    for kind in ["main", "shm-only", "empty"] {
        let temporary = TempDir::new().unwrap();
        let data = temporary.path().join("data");
        populated(&data);
        let backup = temporary.path().join("backup");
        let mut manifest = local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
        let restored = temporary.path().join("restored");
        local_recovery::restore(&backup, &restored).unwrap();
        let restored_data = restored.join("data");
        let paths = namespace(&restored_data, kind);
        add_inventory(&restored, &mut manifest, &paths);
        let complete = restored.join(".agentscm-recovery/restore-complete.json");
        fs::write(&complete, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let marker = restored.join(".agentscm-recovery/quarantine.json");
        let marker_before = fs::read(&marker).unwrap();
        let complete_before = fs::read(&complete).unwrap();
        let state_before = authority_state(&restored_data);
        assert_eq!(state_before, (0, 0, None));
        let database_before = fs::read(restored_data.join("state.sqlite")).unwrap();
        assert_unsupported(local_recovery::activate(&restored, true, true));
        assert_eq!(authority_state(&restored_data), state_before);
        assert_eq!(
            fs::read(restored_data.join("state.sqlite")).unwrap(),
            database_before
        );
        assert_eq!(fs::read(&marker).unwrap(), marker_before);
        assert_eq!(fs::read(&complete).unwrap(), complete_before);
        assert!(Authority::open(&restored_data).is_err());
        assert!(Engine::open(&restored_data).is_err());
    }
}

#[test]
fn absent_namespace_keeps_native_recovery_and_ordinary_attachments_supported() {
    let temporary = TempDir::new().unwrap();
    let data = temporary.path().join("data");
    populated(&data);
    fs::write(data.join("ordinary.sqlite"), b"ordinary native-root bytes").unwrap();
    fs::write(
        data.join("ordinary.sqlite-wal"),
        b"ordinary companion bytes",
    )
    .unwrap();
    // The same name outside native DATA is an explicit generic attachment.
    let attachment = temporary.path().join("issue-workflow");
    fs::create_dir(&attachment).unwrap();
    fs::write(
        attachment.join("state.sqlite"),
        b"ordinary attachment bytes",
    )
    .unwrap();
    fs::write(
        attachment.join("state.sqlite-wal"),
        b"ordinary attachment sidecar",
    )
    .unwrap();
    let backup = temporary.path().join("backup");
    let manifest = local_recovery::backup(&data, &backup, &[], &[attachment], true).unwrap();
    assert_eq!(local_recovery::verify(&backup).unwrap(), manifest);
    let restored = temporary.path().join("restored");
    local_recovery::restore(&backup, &restored).unwrap();
    assert_eq!(
        fs::read(restored.join("data/ordinary.sqlite")).unwrap(),
        b"ordinary native-root bytes"
    );
    assert_eq!(
        fs::read(restored.join("data/ordinary.sqlite-wal")).unwrap(),
        b"ordinary companion bytes"
    );
    assert_eq!(
        fs::read(restored.join("attachments/0/state.sqlite")).unwrap(),
        b"ordinary attachment bytes"
    );
    assert_eq!(
        fs::read(restored.join("attachments/0/state.sqlite-wal")).unwrap(),
        b"ordinary attachment sidecar"
    );
    assert_eq!(local_recovery::activate(&restored, true, true).unwrap(), 1);
    assert_eq!(authority_state(&restored.join("data")), (1, 1, None));
}
