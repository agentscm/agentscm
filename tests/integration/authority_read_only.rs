use agentscm::authority::Authority;
use std::fs;
use tempfile::TempDir;

#[test]
fn readonly_current_authority_observes_live_commits_and_refuses_mutation() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let mut writer = Authority::open(&data).unwrap();
    let repository = writer.repository_id().unwrap();
    let mut reader = Authority::open_read_only(&data).unwrap();
    assert_eq!(reader.repository_id().unwrap(), repository);
    assert_eq!(reader.policy_epoch().unwrap(), 0);
    writer.advance_policy(0).unwrap();
    assert_eq!(reader.policy_epoch().unwrap(), 1);
    assert!(reader.advance_policy(1).is_err());
    assert_eq!(writer.policy_epoch().unwrap(), 1);
    assert!(reader.target("main").unwrap().is_none());
    assert!(reader.events(0, 100).unwrap().is_empty());
}

#[test]
fn readonly_open_refuses_missing_old_or_incomplete_schema_without_migration() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("missing");
    assert!(Authority::open_read_only(&missing).is_err());
    assert!(!missing.exists());
    let empty = temp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    assert!(Authority::open_read_only(&empty).is_err());
    assert_eq!(fs::read_dir(&empty).unwrap().count(), 0);
    let data = temp.path().join("old");
    {
        Authority::open(&data).unwrap();
    }
    {
        let db = rusqlite::Connection::open(data.join("state.sqlite")).unwrap();
        db.execute("INSERT INTO approvals(approval_id,target,revision,context_digest,policy_epoch,semantics_version) VALUES('retain-legacy','main',?1,?2,0,1)",rusqlite::params!["a".repeat(64),"b".repeat(64)]).unwrap();
        db.pragma_update(None, "user_version", 3).unwrap();
    }
    let before = fs::read(data.join("state.sqlite")).unwrap();
    assert!(Authority::open_read_only(&data).is_err());
    assert_eq!(fs::read(data.join("state.sqlite")).unwrap(), before);
    {
        let db = rusqlite::Connection::open(data.join("state.sqlite")).unwrap();
        let revoked: i64 = db
            .query_row(
                "SELECT revoked FROM approvals WHERE approval_id='retain-legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revoked, 0);
    }
    let broken = temp.path().join("broken");
    {
        Authority::open(&broken).unwrap();
    }
    {
        let db = rusqlite::Connection::open(broken.join("state.sqlite")).unwrap();
        db.execute_batch("DROP TABLE landing_preparations").unwrap();
    }
    let before = fs::read(broken.join("state.sqlite")).unwrap();
    assert!(Authority::open_read_only(&broken).is_err());
    assert_eq!(fs::read(broken.join("state.sqlite")).unwrap(), before);
}

#[test]
fn readonly_open_rejects_missing_event_read_column_without_mutation() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    {
        Authority::open(&data).unwrap();
    }
    {
        let db = rusqlite::Connection::open(data.join("state.sqlite")).unwrap();
        db.execute_batch("ALTER TABLE integration_events RENAME COLUMN target TO omitted_target")
            .unwrap();
    }
    let before = fs::read(data.join("state.sqlite")).unwrap();
    assert!(Authority::open_read_only(&data).is_err());
    assert_eq!(fs::read(data.join("state.sqlite")).unwrap(), before);
}

#[test]
fn readonly_prepare_landing_rejects_before_source_publication() {
    use agentscm::context::{ChangeId, InputIdentity, ReviewContext};
    use agentscm::model::{FileChange, PrepareLandingRequest, TargetState};
    use agentscm::store::Engine;
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let engine = Engine::open(&data).unwrap();
    let base = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "file".into(),
                content: Some(b"base".to_vec()),
            }],
        )
        .unwrap();
    let tip = engine
        .checkpoint(
            Some(&base),
            &[FileChange {
                path: "file".into(),
                content: Some(b"tip".to_vec()),
            }],
        )
        .unwrap();
    let writer = Authority::open(&data).unwrap();
    let repository = writer.repository_id().unwrap();
    let known = InputIdentity::Digest {
        sha256: "a".repeat(64),
    };
    let context = ReviewContext {
        version: 1,
        change: ChangeId {
            repository,
            id: "unused".into(),
        },
        change_version: 2,
        revision: tip.clone(),
        base: Some(agentscm::context::BaseState {
            revision: base.clone(),
            target_version: 1,
        }),
        target: "main".into(),
        policy_epoch: 0,
        intent: "read-only publication refusal".into(),
        acceptance_criteria: vec!["no source writes".into()],
        scope: vec!["file".into()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: known.clone(),
        toolchain: known.clone(),
        environment: known,
        required_evidence: vec![],
        required_capabilities: vec!["agentscm.review-context/v1".into()],
    };
    fn inventory(path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        for prefix in fs::read_dir(path).unwrap() {
            for entry in fs::read_dir(prefix.unwrap().path()).unwrap() {
                let path = entry.unwrap().path();
                files.push((path.to_str().unwrap().to_owned(), fs::read(path).unwrap()));
            }
        }
        files.sort();
        files
    }
    let before = inventory(&data.join("objects"));
    let mut reader = Authority::open_read_only(&data).unwrap();
    let result = reader.prepare_landing(
        &engine,
        &PrepareLandingRequest {
            operation_id: "refused".into(),
            target: "main".into(),
            expected_target: TargetState {
                revision: base,
                version: 1,
            },
            source_tip: tip,
            change_version: 1,
            context,
        },
    );
    assert!(
        matches!(result,Err(agentscm::Error::Conflict(message)) if message.contains("read-only"))
    );
    assert_eq!(inventory(&data.join("objects")), before);
}

#[test]
fn readonly_open_requires_existing_managed_lock_without_creating_it() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let repository = Authority::open(&data).unwrap().repository_id().unwrap();
    let lock = data.join(".agentscm-local-access.lock");
    fs::remove_file(&lock).unwrap();
    let before = fs::read(data.join("state.sqlite")).unwrap();
    assert!(Authority::open_read_only(&data).is_err());
    assert!(!lock.exists());
    assert_eq!(fs::read(data.join("state.sqlite")).unwrap(), before);
    // The explicit writer still provisions the managed control file.
    drop(Authority::open(&data).unwrap());
    assert_eq!(
        Authority::open_read_only(&data)
            .unwrap()
            .repository_id()
            .unwrap(),
        repository
    );
}

#[cfg(unix)]
#[test]
fn readonly_open_accepts_readonly_lock_and_retains_its_lease() {
    use agentscm::local_recovery::AccessGuard;
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    drop(Authority::open(&data).unwrap());
    let lock = data.join(".agentscm-local-access.lock");
    fs::write(&lock, b"retained authority control").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o400)).unwrap();
    let reader = Authority::open_read_only(&data).unwrap();
    assert_eq!(reader.policy_epoch().unwrap(), 0);
    assert_eq!(fs::read(&lock).unwrap(), b"retained authority control");
    assert_eq!(
        fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
        0o400
    );
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(AccessGuard::exclusive(&data).is_err());
    drop(reader);
    drop(AccessGuard::exclusive(&data).unwrap());
}

#[test]
fn readonly_open_rejects_quarantined_authority_without_creating_lock() {
    use agentscm::local_recovery;
    use agentscm::store::Engine;
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    drop(Engine::open(&data).unwrap());
    drop(Authority::open(&data).unwrap());
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let restored = temp.path().join("restored");
    local_recovery::restore(&backup, &restored).unwrap();
    let restored_data = restored.join("data");
    let lock = restored_data.join(".agentscm-local-access.lock");
    let before = fs::read(restored_data.join("state.sqlite")).unwrap();
    assert!(!lock.exists());
    assert!(matches!(Authority::open_read_only(&restored_data),
        Err(agentscm::Error::Conflict(message)) if message.contains("explicit recovery activation")));
    assert!(!lock.exists());
    assert_eq!(
        fs::read(restored_data.join("state.sqlite")).unwrap(),
        before
    );
    local_recovery::activate(&restored, true, true).unwrap();
    assert_eq!(
        Authority::open_read_only(&restored_data)
            .unwrap()
            .policy_epoch()
            .unwrap(),
        1
    );
}
