use agentscm::authority::Authority;
use agentscm::checks::SourcePathExpectation;
use agentscm::context::{ChangeId, InputIdentity, ReviewContext};
use agentscm::local_recovery::{self, AccessGuard};
use agentscm::model::{Approval, FileChange, IntegrateRequest};
use agentscm::skill_workflow::{
    RunState, SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource,
};
use agentscm::store::{Engine, digest};
use agentscm::workspace::Workspace;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn existing_shared_guard_never_creates_lock_and_keeps_clone_lifetime() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("missing");
    assert!(AccessGuard::shared_existing(&missing).is_err());
    assert!(!missing.exists());
    let data = temp.path().join("data");
    fs::create_dir(&data).unwrap();
    assert!(AccessGuard::shared_existing(&data).is_err());
    assert_eq!(fs::read_dir(&data).unwrap().count(), 0);
    drop(Engine::open(&data).unwrap());
    let lock = data.join(".agentscm-local-access.lock");
    fs::write(&lock, b"retained control bytes").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o400)).unwrap();
    let reader = AccessGuard::shared_existing(&data).unwrap();
    let clone = reader.clone();
    assert_eq!(fs::read(&lock).unwrap(), b"retained control bytes");
    assert_eq!(
        fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
        0o400
    );
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(AccessGuard::exclusive(&data).is_err());
    drop(reader);
    assert!(AccessGuard::exclusive(&data).is_err());
    drop(clone);
    drop(AccessGuard::exclusive(&data).unwrap());
    fs::remove_file(&lock).unwrap();
    assert!(AccessGuard::shared_existing(&data).is_err());
    assert!(!lock.exists());
    fs::create_dir(&lock).unwrap();
    assert!(AccessGuard::shared_existing(&data).is_err());
    fs::remove_dir(&lock).unwrap();
    let external = temp.path().join("external");
    fs::write(&external, b"ordinary external bytes").unwrap();
    std::os::unix::fs::symlink(&external, &lock).unwrap();
    assert!(AccessGuard::shared_existing(&data).is_err());
    assert_eq!(fs::read(&external).unwrap(), b"ordinary external bytes");
}

#[test]
fn existing_shared_guard_rejects_quarantine_before_lock_creation() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let destination = temp.path().join("restored");
    local_recovery::restore(&backup, &destination).unwrap();
    let restored_data = destination.join("data");
    let lock = restored_data.join(".agentscm-local-access.lock");
    assert!(!lock.exists());
    assert!(matches!(AccessGuard::shared_existing(&restored_data),
        Err(agentscm::Error::Conflict(message)) if message.contains("explicit recovery activation")));
    assert!(!lock.exists());
    local_recovery::activate(&destination, true, true).unwrap();
    assert!(AccessGuard::shared_existing(&restored_data).is_ok());
}

fn populated(root: &Path) -> (String, String, ReviewContext, Approval) {
    let engine = Engine::open(root).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "hello.txt".into(),
                content: Some(b"retained".to_vec()),
            }],
        )
        .unwrap();
    let mut authority = Authority::open(root).unwrap();
    let repository = authority.repository_id().unwrap();
    let known = InputIdentity::Digest {
        sha256: "a".repeat(64),
    };
    let context = ReviewContext {
        version: 1,
        change: ChangeId {
            repository: repository.clone(),
            id: "recovery-test".into(),
        },
        change_version: 1,
        revision: revision.clone(),
        base: None,
        target: "main".into(),
        policy_epoch: 0,
        intent: "retain state across offline recovery".into(),
        acceptance_criteria: vec!["exact source and authority survive".into()],
        scope: vec!["hello.txt".into()],
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
    let approval = Approval {
        approval_id: "before-backup".into(),
        target: "main".into(),
        revision: revision.clone(),
        context_digest: change.context_digest,
        change: Some(change.id),
        change_version: Some(1),
        policy_epoch: 0,
    };
    authority.approve(&approval).unwrap();
    (repository, revision, context, approval)
}

fn populated_with_workflow(root: &Path, run_id: &str) {
    let (repository, revision, _, _) = populated(root);
    let skill_path = "skills/recovery/SKILL.md";
    let source_path = "src/recovery.rs";
    let source_bytes = b"pub fn recovered() -> bool { true }\n";
    let engine = Engine::open(root).unwrap();
    let revision = engine
        .checkpoint(
            Some(&revision),
            &[
                FileChange {
                    path: skill_path.into(),
                    content: Some(b"# Recovery workflow\n".to_vec()),
                },
                FileChange {
                    path: source_path.into(),
                    content: Some(source_bytes.to_vec()),
                },
            ],
        )
        .unwrap();
    drop(engine);
    let definition = WorkflowDefinition {
        version: 1,
        name: "recovery-schema-validation".into(),
        source: WorkflowSource {
            repository,
            revision,
            policy_epoch: 0,
        },
        skill: SkillReference {
            entrypoint: skill_path.into(),
            resources: vec![],
        },
        steps: vec![StepDefinition {
            id: "source".into(),
            needs: vec![],
            required: true,
            action: StepAction::SourcePaths {
                expectations: vec![SourcePathExpectation {
                    path: source_path.into(),
                    sha256: digest(source_bytes),
                }],
            },
        }],
    };
    let plan = agentscm::skill_workflow::compile(root, &definition).unwrap();
    agentscm::skill_workflow::start(root, run_id, &plan).unwrap();
    let view = agentscm::skill_workflow::advance(root, run_id, 1).unwrap();
    assert_eq!(view.state, RunState::Succeeded);
}

#[test]
fn populated_restore_keeps_dirty_workspace_receipts_and_revokes_old_approval() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (repository, revision, context, approval) = populated(&data);
    let work = temp.path().join("work");
    {
        let engine = Engine::open(&data).unwrap();
        Workspace::create(&engine, &work, context.change.clone(), &revision).unwrap();
    }
    fs::write(work.join("hello.txt"), b"uncheckpointed developer edit").unwrap();
    fs::write(work.join("untracked.txt"), b"keep me too").unwrap();
    fs::write(
        work.join(".agentscm-local-access.lock"),
        b"ordinary source filename",
    )
    .unwrap();
    fs::write(work.join("ordinary.sqlite"), b"ordinary developer bytes").unwrap();
    fs::write(
        work.join("ordinary.sqlite-wal"),
        b"ordinary companion filename",
    )
    .unwrap();
    fs::create_dir(work.join(".agentscm-recovery")).unwrap();
    fs::write(
        work.join(".agentscm-recovery/quarantine.json"),
        b"ordinary candidate filename",
    )
    .unwrap();
    let receipt = temp.path().join("capture.json");
    fs::write(&receipt, b"private receipt").unwrap();
    let backup = temp.path().join("backup");
    let manifest = local_recovery::backup(
        &data,
        &backup,
        std::slice::from_ref(&work),
        std::slice::from_ref(&receipt),
        true,
    )
    .unwrap();
    assert_eq!(manifest.repository, repository);
    let restored = temp.path().join("restored");
    local_recovery::restore(&backup, &restored).unwrap();
    assert!(Engine::open(restored.join("data")).is_err());
    assert!(Workspace::open(restored.join("workspaces/0")).is_err());
    assert!(local_recovery::activate(&restored, false, true).is_err());
    assert_eq!(local_recovery::activate(&restored, true, true).unwrap(), 1);
    let engine = Engine::open(restored.join("data")).unwrap();
    assert_eq!(
        engine.read_file(&revision, "hello.txt").unwrap(),
        b"retained"
    );
    let authority = Authority::open(restored.join("data")).unwrap();
    assert_eq!(authority.repository_id().unwrap(), repository);
    assert_eq!(
        authority
            .review_context(&context.digest().unwrap())
            .unwrap(),
        context
    );
    assert_eq!(authority.policy_epoch().unwrap(), 1);
    {
        let db = rusqlite::Connection::open(restored.join("data/state.sqlite")).unwrap();
        let revoked: i64 = db
            .query_row(
                "SELECT revoked FROM approvals WHERE approval_id='before-backup'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revoked, 1);
    }
    let workspace = Workspace::open(restored.join("workspaces/0")).unwrap();
    assert_eq!(workspace.state().unwrap().base_revision, revision);
    assert_eq!(
        fs::read(restored.join("workspaces/0/hello.txt")).unwrap(),
        b"uncheckpointed developer edit"
    );
    assert_eq!(
        fs::read(restored.join("workspaces/0/untracked.txt")).unwrap(),
        b"keep me too"
    );
    assert_eq!(
        fs::read(restored.join("attachments/0")).unwrap(),
        b"private receipt"
    );
    assert_eq!(
        fs::read(restored.join("workspaces/0/.agentscm-local-access.lock")).unwrap(),
        b"ordinary source filename"
    );
    assert_eq!(
        fs::read(restored.join("workspaces/0/ordinary.sqlite-wal")).unwrap(),
        b"ordinary companion filename"
    );
    drop(authority);
    let mut authority = Authority::open(restored.join("data")).unwrap();
    let request = IntegrateRequest {
        operation_id: "restored-old-approval".into(),
        target: approval.target,
        expected_target: None,
        revision,
        context_digest: approval.context_digest,
        approval_id: approval.approval_id,
        change: approval.change,
        change_version: approval.change_version,
        policy_epoch: 0,
    };
    assert!(authority.integrate(&engine, &request).is_err());
    assert!(authority.target("main").unwrap().is_none());
    assert_eq!(Authority::open(&data).unwrap().policy_epoch().unwrap(), 0);
}

#[test]
fn active_native_handles_and_clones_block_backup() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let engine = Engine::open(&data).unwrap();
    let clone = engine.clone();
    drop(engine);
    let output = temp.path().join("backup");
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    assert!(!output.exists());
    drop(clone);
    let authority = Authority::open(&data).unwrap();
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    drop(authority);
    let journal = agentscm::merge::MergeJournal::open(
        &data,
        &Authority::open(&data).unwrap().repository_id().unwrap(),
    )
    .unwrap();
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    drop(journal);
    local_recovery::backup(&data, &output, &[], &[], true).unwrap();
}

#[test]
fn exclusive_lease_rejects_openers_before_mutable_state_creation() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    fs::create_dir(&data).unwrap();
    let guard = AccessGuard::exclusive(&data).unwrap();
    assert!(Engine::open(&data).is_err());
    assert!(!data.join("objects").exists());
    assert!(Authority::open(&data).is_err());
    assert!(!data.join("state.sqlite").exists());
    assert!(agentscm::merge::MergeJournal::open(&data, "repository-1").is_err());
    assert!(!data.join("merges").exists());
    drop(guard);
    Engine::open(&data).unwrap();
}

#[test]
fn active_workspace_and_raw_sqlite_writer_are_rejected() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (_, revision, context, _) = populated(&data);
    let work = temp.path().join("work");
    let engine = Engine::open(&data).unwrap();
    let workspace = Workspace::create(&engine, &work, context.change, &revision).unwrap();
    drop(engine);
    let output = temp.path().join("backup");
    assert!(
        local_recovery::backup(&data, &output, std::slice::from_ref(&work), &[], true).is_err()
    );
    drop(workspace);
    let connection = rusqlite::Connection::open(data.join("state.sqlite")).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE; UPDATE authority_state SET policy_epoch=policy_epoch+1 WHERE singleton=1").unwrap();
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    assert!(!output.exists());
    connection.execute_batch("ROLLBACK").unwrap();
    drop(connection);
    local_recovery::backup(&data, &output, &[work], &[], true).unwrap();
}

#[cfg(unix)]
#[test]
fn workspace_containing_a_symlink_is_refused_before_publication() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (_, revision, context, _) = populated(&data);
    let engine = Engine::open(&data).unwrap();
    let work = temp.path().join("work");
    let workspace = Workspace::create(&engine, &work, context.change, &revision).unwrap();
    std::os::unix::fs::symlink("hello.txt", work.join("link")).unwrap();
    drop(workspace);
    drop(engine);
    let output = temp.path().join("backup");
    let error = local_recovery::backup(&data, &output, &[work], &[], true).unwrap_err();
    assert!(
        error.to_string().contains("unsupported backup entry")
            && error.to_string().ends_with("link"),
        "{error}"
    );
    assert!(!output.exists());
}

#[test]
fn wal_commits_are_captured_and_source_authority_is_unchanged() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    // Hold a raw idle connection to retain committed WAL pages until backup.
    let connection = rusqlite::Connection::open(data.join("state.sqlite")).unwrap();
    connection.execute_batch("PRAGMA wal_autocheckpoint=0; UPDATE authority_state SET policy_epoch=7 WHERE singleton=1").unwrap();
    assert!(data.join("state.sqlite-wal").metadata().unwrap().len() > 0);
    connection
        .execute_batch("BEGIN; SELECT policy_epoch FROM authority_state")
        .unwrap();
    // An old/raw reader is not fenced by advisory leases. Exclusive SQLite
    // capture must reject it; physical quiescence requires closing it first.
    let output = temp.path().join("backup");
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    drop(connection);
    local_recovery::backup(&data, &output, &[], &[], true).unwrap();
    assert_eq!(local_recovery::verify(&output).unwrap().policy_epoch, 7);
    assert_eq!(Authority::open(&data).unwrap().policy_epoch().unwrap(), 7);
}

#[test]
fn corrupt_truncated_extra_and_wrong_schema_backups_never_touch_destination() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let manifest = fs::read(backup.join("recovery-manifest.json")).unwrap();
    fs::write(backup.join("recovery-manifest.json"), b"{").unwrap();
    let destination = temp.path().join("restore");
    assert!(local_recovery::restore(&backup, &destination).is_err());
    assert!(!destination.exists());
    fs::write(backup.join("recovery-manifest.json"), &manifest).unwrap();
    let extra = backup.join("payload/data/unlisted");
    fs::write(&extra, b"extra").unwrap();
    assert!(local_recovery::verify(&backup).is_err());
    fs::remove_file(extra).unwrap();
    let main = backup.join("payload/data/state.sqlite");
    let bytes = fs::read(&main).unwrap();
    fs::write(&main, &bytes[..100]).unwrap();
    assert!(local_recovery::verify(&backup).is_err());
    fs::write(&main, bytes).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
    value["version"] = 2.into();
    fs::write(
        backup.join("recovery-manifest.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    assert!(local_recovery::verify(&backup).is_err());
    fs::write(backup.join("recovery-manifest.json"), manifest).unwrap();
    local_recovery::restore(&backup, &destination).unwrap();
    fs::write(destination.join("unrelated"), b"keep").unwrap();
    assert!(local_recovery::restore(&backup, &destination).is_err());
    assert_eq!(fs::read(destination.join("unrelated")).unwrap(), b"keep");
}

#[test]
fn current_authority_backup_rejects_missing_workflow_tables() {
    for table in ["workflow_requirements", "workflow_receipts"] {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        populated(&data);
        let connection = rusqlite::Connection::open(data.join("state.sqlite")).unwrap();
        connection
            .execute_batch(&format!("DROP TABLE {table}"))
            .unwrap();
        drop(connection);

        let output = temp.path().join("backup");
        assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
        assert!(!output.exists());
    }
}

#[test]
fn current_skill_workflow_backup_rejects_missing_review_tables() {
    for table in ["workflow_review_calls", "workflow_reviews"] {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        populated_with_workflow(&data, "schema-check");
        let connection =
            rusqlite::Connection::open(data.join("skill-workflows/state.sqlite")).unwrap();
        connection
            .execute_batch(&format!("DROP TABLE {table}"))
            .unwrap();
        drop(connection);

        let output = temp.path().join("backup");
        assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
        assert!(!output.exists());
    }
}

#[test]
fn current_skill_workflow_journal_survives_backup_and_restore() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated_with_workflow(&data, "restored-workflow");
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    local_recovery::verify(&backup).unwrap();

    let restored = temp.path().join("restored");
    local_recovery::restore(&backup, &restored).unwrap();
    local_recovery::activate(&restored, true, true).unwrap();
    let view =
        agentscm::skill_workflow::status(&restored.join("data"), "restored-workflow").unwrap();
    assert_eq!(view.state, RunState::Succeeded);
}

#[test]
fn missing_objects_wrong_authority_schema_and_links_fail_backup() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (_, revision, _, _) = populated(&data);
    let output = temp.path().join("backup");
    let object = data
        .join("objects")
        .join(&revision[..2])
        .join(&revision[2..]);
    let bytes = fs::read(&object).unwrap();
    fs::remove_file(&object).unwrap();
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    assert!(!output.exists());
    fs::write(object, bytes).unwrap();
    let connection = rusqlite::Connection::open(data.join("state.sqlite")).unwrap();
    connection.pragma_update(None, "user_version", 99).unwrap();
    drop(connection);
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    assert!(!output.exists());
}

#[test]
fn later_operations_are_absent_and_cannot_be_replayed_with_restored_approval() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (_, revision, _, approval) = populated(&data);
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let request = IntegrateRequest {
        operation_id: "after-backup".into(),
        target: approval.target,
        expected_target: None,
        revision,
        context_digest: approval.context_digest,
        approval_id: approval.approval_id,
        change: approval.change,
        change_version: approval.change_version,
        policy_epoch: 0,
    };
    {
        let engine = Engine::open(&data).unwrap();
        Authority::open(&data)
            .unwrap()
            .integrate(&engine, &request)
            .unwrap();
    }
    let destination = temp.path().join("restore");
    local_recovery::restore(&backup, &destination).unwrap();
    local_recovery::activate(&destination, true, true).unwrap();
    let engine = Engine::open(destination.join("data")).unwrap();
    let mut authority = Authority::open(destination.join("data")).unwrap();
    assert!(authority.target("main").unwrap().is_none());
    assert!(authority.events(0, 100).unwrap().is_empty());
    assert!(authority.integrate(&engine, &request).is_err());
    assert!(
        Authority::open(&data)
            .unwrap()
            .target("main")
            .unwrap()
            .is_some()
    );
}

#[cfg(unix)]
#[test]
fn links_and_traversal_manifests_are_rejected() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let output = temp.path().join("backup");
    std::os::unix::fs::symlink(temp.path(), data.join("external-link")).unwrap();
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    assert!(!output.exists());
    fs::remove_file(data.join("external-link")).unwrap();
    local_recovery::backup(&data, &output, &[], &[], true).unwrap();
    let path = output.join("recovery-manifest.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["entries"][0]["path"] = "../outside".into();
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    let restore = temp.path().join("restore");
    assert!(local_recovery::restore(&output, &restore).is_err());
    assert!(!restore.exists());
}

#[test]
fn sealed_backups_and_restores_reject_unlisted_committed_wal() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let restored = temp.path().join("restore");
    local_recovery::restore(&backup, &restored).unwrap();
    let attack = temp.path().join("attack.sqlite");
    fs::copy(backup.join("payload/data/state.sqlite"), &attack).unwrap();
    let db = rusqlite::Connection::open(&attack).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE approvals SET revoked=1",
    )
    .unwrap();
    let wal = fs::read(temp.path().join("attack.sqlite-wal")).unwrap();
    assert!(!wal.is_empty());
    let original = fs::read(backup.join("payload/data/state.sqlite")).unwrap();
    fs::write(backup.join("payload/data/state.sqlite-wal"), &wal).unwrap();
    assert_eq!(
        fs::read(backup.join("payload/data/state.sqlite")).unwrap(),
        original
    );
    assert!(local_recovery::verify(&backup).is_err());
    let rejected = temp.path().join("rejected");
    assert!(local_recovery::restore(&backup, &rejected).is_err());
    assert!(!rejected.exists());
    fs::write(restored.join("data/state.sqlite-wal"), &wal).unwrap();
    assert!(local_recovery::activate(&restored, true, true).is_err());
    assert!(restored.join(".agentscm-recovery/quarantine.json").exists());
    assert_eq!(
        fs::read(restored.join("data/state.sqlite")).unwrap(),
        original
    );
}

#[test]
fn missing_merge_only_revision_and_foreign_merge_journal_are_rejected() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (repository, base, _, _) = populated(&data);
    let tip;
    {
        let engine = Engine::open(&data).unwrap();
        tip = engine
            .checkpoint(
                Some(&base),
                &[FileChange {
                    path: "hello.txt".into(),
                    content: Some(b"merge-only tip".to_vec()),
                }],
            )
            .unwrap();
        let mut merge = agentscm::merge::MergeJournal::open(&data, &repository).unwrap();
        merge.begin(&engine, "merge-only", &base, &tip).unwrap();
    }
    let object = data.join("objects").join(&tip[..2]).join(&tip[2..]);
    let bytes = fs::read(&object).unwrap();
    fs::remove_file(&object).unwrap();
    let output = temp.path().join("missing-tip");
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    assert!(!output.exists());
    fs::write(object, bytes).unwrap();
    {
        let db = rusqlite::Connection::open(data.join("merges/state.sqlite")).unwrap();
        db.execute(
            "UPDATE merge_repository SET repository='foreign-repository' WHERE singleton=1",
            [],
        )
        .unwrap();
    }
    assert!(
        local_recovery::backup(&data, &temp.path().join("foreign-merge"), &[], &[], true).is_err()
    );
}

#[test]
fn missing_historical_workspace_operation_revision_is_rejected() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (_, base, context, _) = populated(&data);
    let work = temp.path().join("work");
    let tip;
    {
        let engine = Engine::open(&data).unwrap();
        let mut workspace = Workspace::create(&engine, &work, context.change, &base).unwrap();
        fs::write(work.join("hello.txt"), b"checkpoint then undo").unwrap();
        workspace.checkpoint(&engine, "checkpoint", 0).unwrap();
        tip = workspace.state().unwrap().base_revision;
        workspace.undo(&engine, "undo", 1, "checkpoint").unwrap();
        assert_eq!(workspace.state().unwrap().base_revision, base);
    }
    fs::remove_file(data.join("objects").join(&tip[..2]).join(&tip[2..])).unwrap();
    assert!(
        local_recovery::backup(
            &data,
            &temp.path().join("missing-operation"),
            &[work],
            &[],
            true
        )
        .is_err()
    );
}

#[test]
fn incomplete_restore_cannot_activate_or_open_native_state() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (_, revision, _, _) = populated(&data);
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let restored = temp.path().join("restore");
    local_recovery::restore(&backup, &restored).unwrap();
    // Simulate interruption before the completion record was installed.
    fs::remove_file(restored.join(".agentscm-recovery/restore-complete.json")).unwrap();
    assert!(local_recovery::activate(&restored, true, true).is_err());
    assert!(Engine::open(restored.join("data")).is_err());
    assert_eq!(
        Engine::open(&data)
            .unwrap()
            .read_file(&revision, "hello.txt")
            .unwrap(),
        b"retained"
    );
}

#[test]
fn generic_directory_attachment_preserves_control_looking_names_without_mutation() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let attachment = temp.path().join("transfer-state");
    fs::create_dir(&attachment).unwrap();
    fs::write(
        attachment.join(".agentscm-local-access.lock"),
        b"ordinary transfer bytes",
    )
    .unwrap();
    fs::write(
        attachment.join("ordinary.sqlite-wal"),
        b"ordinary WAL-looking bytes",
    )
    .unwrap();
    let before: Vec<_> = fs::read_dir(&attachment)
        .unwrap()
        .map(|v| v.unwrap().file_name())
        .collect();
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], std::slice::from_ref(&attachment), true).unwrap();
    let after: Vec<_> = fs::read_dir(&attachment)
        .unwrap()
        .map(|v| v.unwrap().file_name())
        .collect();
    assert_eq!(before, after);
    assert_eq!(
        fs::read(attachment.join(".agentscm-local-access.lock")).unwrap(),
        b"ordinary transfer bytes"
    );
    let restored = temp.path().join("restore");
    local_recovery::restore(&backup, &restored).unwrap();
    assert_eq!(
        fs::read(restored.join("attachments/0/.agentscm-local-access.lock")).unwrap(),
        b"ordinary transfer bytes"
    );
    assert_eq!(
        fs::read(restored.join("attachments/0/ordinary.sqlite-wal")).unwrap(),
        b"ordinary WAL-looking bytes"
    );
    let empty = temp.path().join("generic-empty");
    fs::create_dir(&empty).unwrap();
    local_recovery::backup(
        &data,
        &temp.path().join("empty-backup"),
        &[],
        std::slice::from_ref(&empty),
        true,
    )
    .unwrap();
    assert_eq!(fs::read_dir(empty).unwrap().count(), 0);
}

#[test]
fn restore_destination_cannot_modify_backup_or_payload() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let manifest = fs::read(backup.join("recovery-manifest.json")).unwrap();
    for destination in [
        backup.join("new-root"),
        backup.join("payload/data/new-root"),
    ] {
        assert!(local_recovery::restore(&backup, &destination).is_err());
        assert!(!destination.exists());
        assert_eq!(
            fs::read(backup.join("recovery-manifest.json")).unwrap(),
            manifest
        );
        local_recovery::verify(&backup).unwrap();
    }
}

#[test]
fn database_role_is_independent_of_user_chosen_data_directory_name() {
    for name in ["merges", ".agentscm-workspace"] {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join(name);
        populated(&data);
        local_recovery::backup(&data, &temp.path().join("backup"), &[], &[], true).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn changed_restored_mode_blocks_activation() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let restored = temp.path().join("restored");
    local_recovery::restore(&backup, &restored).unwrap();
    let path = restored.join("data/state.sqlite");
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    fs::set_permissions(&path, fs::Permissions::from_mode(mode ^ 0o040)).unwrap();
    assert!(local_recovery::activate(&restored, true, true).is_err());
    assert!(restored.join(".agentscm-recovery/quarantine.json").exists());
}

#[test]
fn ambiguous_root_mapping_is_rejected_before_restore() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let attachment = temp.path().join("receipt");
    fs::write(&attachment, b"receipt").unwrap();
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[attachment], true).unwrap();
    let path = backup.join("recovery-manifest.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut duplicate = value["roots"][1].clone();
    duplicate["original"] = temp
        .path()
        .join("different-original")
        .to_str()
        .unwrap()
        .into();
    value["roots"].as_array_mut().unwrap().push(duplicate);
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(local_recovery::restore(&backup, &temp.path().join("restore")).is_err());
}

#[test]
fn activation_revokes_unconsumed_approval_and_keeps_consumed_audit_history() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (_, revision, _, approval) = populated(&data);
    {
        let engine = Engine::open(&data).unwrap();
        let mut authority = Authority::open(&data).unwrap();
        let mut consumed = approval.clone();
        consumed.approval_id = "consumed-history".into();
        authority.approve(&consumed).unwrap();
        authority
            .integrate(
                &engine,
                &IntegrateRequest {
                    operation_id: "history".into(),
                    target: consumed.target,
                    expected_target: None,
                    revision,
                    context_digest: consumed.context_digest,
                    approval_id: consumed.approval_id,
                    change: consumed.change,
                    change_version: consumed.change_version,
                    policy_epoch: 0,
                },
            )
            .unwrap();
    }
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let restored = temp.path().join("restore");
    local_recovery::restore(&backup, &restored).unwrap();
    local_recovery::activate(&restored, true, true).unwrap();
    let db = rusqlite::Connection::open(restored.join("data/state.sqlite")).unwrap();
    let unused: (i64, Option<i64>) = db
        .query_row(
            "SELECT revoked,consumed_event FROM approvals WHERE approval_id='before-backup'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(unused, (1, None));
    let used: (i64, Option<i64>) = db
        .query_row(
            "SELECT revoked,consumed_event FROM approvals WHERE approval_id='consumed-history'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(used, (0, Some(1)));
}

#[test]
fn quarantined_constructor_failures_do_not_create_paths() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (repository, revision, context, _) = populated(&data);
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let restored = temp.path().join("restore");
    local_recovery::restore(&backup, &restored).unwrap();
    assert!(Engine::open(&restored).is_err());
    assert!(!restored.join("objects").exists());
    assert!(Authority::open(&restored).is_err());
    assert!(!restored.join("state.sqlite").exists());
    assert!(agentscm::merge::MergeJournal::open(&restored, &repository).is_err());
    assert!(!restored.join("merges").exists());
    for name in [
        "new-engine",
        "new-authority",
        "new-merge",
        ".agentscm-workspace",
    ] {
        let path = restored.join(name);
        assert!(Engine::open(&path).is_err());
        assert!(!path.exists());
        assert!(Authority::open(&path).is_err());
        assert!(!path.exists());
        assert!(agentscm::merge::MergeJournal::open(&path, &repository).is_err());
        assert!(!path.exists());
    }
    let engine = Engine::open(&data).unwrap();
    let work = restored.join("new-work");
    assert!(Workspace::create(&engine, &work, context.change, &revision).is_err());
    assert!(!work.exists());
    local_recovery::activate(&restored, true, true).unwrap();
}

#[test]
fn malformed_typed_merge_plan_is_rejected_before_backup_publication() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (repository, base, _, _) = populated(&data);
    {
        let engine = Engine::open(&data).unwrap();
        let tip = engine
            .checkpoint(
                Some(&base),
                &[FileChange {
                    path: "hello.txt".into(),
                    content: Some(b"tip".to_vec()),
                }],
            )
            .unwrap();
        let mut merge = agentscm::merge::MergeJournal::open(&data, &repository).unwrap();
        merge.begin(&engine, "typed-plan", &base, &tip).unwrap();
    }
    {
        let db = rusqlite::Connection::open(data.join("merges/state.sqlite")).unwrap();
        db.execute(
            "UPDATE merge_sessions SET plan_json=?1",
            [br#"{"files":[1],"conflicts":[]}"#.to_vec()],
        )
        .unwrap();
    }
    let output = temp.path().join("backup");
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    assert!(!output.exists());
}

#[cfg(unix)]
#[test]
fn non_utf8_destination_fails_before_mutation() {
    use std::os::unix::ffi::OsStringExt;
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let output = temp
        .path()
        .join(std::ffi::OsString::from_vec(vec![b'o', 255]));
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    assert!(!output.exists());
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    assert!(local_recovery::restore(&backup, &output).is_err());
    assert!(!output.exists());
}

#[test]
fn missing_conflict_only_blob_reference_blocks_backup() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (repository, base, _, _) = populated(&data);
    {
        let engine = Engine::open(&data).unwrap();
        let left = engine
            .checkpoint(
                Some(&base),
                &[FileChange {
                    path: "hello.txt".into(),
                    content: Some(b"left conflict".to_vec()),
                }],
            )
            .unwrap();
        let right = engine
            .checkpoint(
                Some(&base),
                &[FileChange {
                    path: "hello.txt".into(),
                    content: Some(b"right conflict".to_vec()),
                }],
            )
            .unwrap();
        let mut merge = agentscm::merge::MergeJournal::open(&data, &repository).unwrap();
        merge
            .begin(&engine, "conflict-root", &left, &right)
            .unwrap();
    }
    {
        let db = rusqlite::Connection::open(data.join("merges/state.sqlite")).unwrap();
        let bytes: Vec<u8> = db
            .query_row("SELECT plan_json FROM merge_sessions", [], |row| row.get(0))
            .unwrap();
        let mut plan: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!plan["conflicts"].as_array().unwrap().is_empty());
        plan["conflicts"][0]["paths"][0]["left"] = "f".repeat(64).into();
        db.execute(
            "UPDATE merge_sessions SET plan_json=?1",
            [serde_json::to_vec(&plan).unwrap()],
        )
        .unwrap();
    }
    let output = temp.path().join("backup");
    assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
    assert!(!output.exists());
}

#[cfg(unix)]
#[test]
fn quarantined_symlinked_ancestor_is_resolved_before_constructor_mutation() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (repository, revision, context, _) = populated(&data);
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let restored = temp.path().join("restore");
    local_recovery::restore(&backup, &restored).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(restored.join("data"), &alias).unwrap();
    for name in ["new-engine", "nested/new-authority", "new-merge"] {
        let path = alias.join(name);
        assert!(Engine::open(&path).is_err());
        assert!(!restored.join("data").join(name).exists());
        assert!(Authority::open(&path).is_err());
        assert!(!restored.join("data").join(name).exists());
        assert!(agentscm::merge::MergeJournal::open(&path, &repository).is_err());
        assert!(!restored.join("data").join(name).exists());
    }
    let work = alias.join("new-work");
    let engine = Engine::open(&data).unwrap();
    assert!(Workspace::create(&engine, &work, context.change, &revision).is_err());
    assert!(!restored.join("data/new-work").exists());
    let ambiguous = restored.join("not-created/../outside");
    assert!(Engine::open(&ambiguous).is_err());
    assert!(!restored.join("not-created").exists());
    local_recovery::activate(&restored, true, true).unwrap();
}

#[test]
fn every_restore_has_a_new_occurrence_even_when_owned_destination_path_is_reused() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    populated(&data);
    let backup = temp.path().join("backup");
    local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    let restored = temp.path().join("restore");
    local_recovery::restore(&backup, &restored).unwrap();
    let first = local_recovery::restore_instance(&restored).unwrap();
    assert_eq!(first.len(), 32);
    fs::remove_dir_all(&restored).unwrap();
    local_recovery::restore(&backup, &restored).unwrap();
    let second = local_recovery::restore_instance(&restored).unwrap();
    assert_ne!(first, second);
    fs::remove_file(restored.join(".agentscm-recovery/instance.json")).unwrap();
    assert!(local_recovery::activate(&restored, true, true).is_err());
    assert!(restored.join(".agentscm-recovery/quarantine.json").exists());
    assert_eq!(Authority::open(&data).unwrap().policy_epoch().unwrap(), 0);
}

#[test]
fn interrupted_publication_scratch_is_inventoried_and_restored_as_opaque_bytes() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let (_, revision, _, _) = populated(&data);
    let canonical = data
        .join("objects")
        .join(&revision[..2])
        .join(&revision[2..]);
    let complete = fs::read(&canonical).unwrap();
    let scratch = [
        (
            "ab/.tmp-42-0-ab0123456789".to_owned(),
            b"partial publication".to_vec(),
        ),
        ("ab/.tmp-42-1-ab0000000000".to_owned(), Vec::new()),
        (
            format!("{}/.tmp-42-2-{}", &revision[..2], &revision[..12]),
            complete,
        ),
    ];
    for (index, (relative, bytes)) in scratch.iter().enumerate() {
        let path = data.join("objects").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if index == 2 {
            fs::hard_link(&canonical, path).unwrap();
        } else {
            fs::write(path, bytes).unwrap();
        }
    }
    // An interrupted shard creation can leave a valid empty shard too.
    let empty_shard = (0u16..=255)
        .map(|value| format!("{value:02x}"))
        .find(|shard| !data.join("objects").join(shard).exists())
        .unwrap();
    fs::create_dir(data.join("objects").join(&empty_shard)).unwrap();
    let backup = temp.path().join("backup");
    let manifest = local_recovery::backup(&data, &backup, &[], &[], true).unwrap();
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| entry.directory && entry.path == format!("data/objects/{empty_shard}"))
    );
    for (relative, bytes) in &scratch {
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == format!("data/objects/{relative}"))
            .unwrap();
        assert!(!entry.directory);
        assert_eq!(entry.bytes, bytes.len() as u64);
        assert_eq!(entry.sha256, agentscm::store::digest(bytes));
        assert_eq!(
            fs::read(backup.join("payload/data/objects").join(relative)).unwrap(),
            *bytes
        );
    }
    local_recovery::verify(&backup).unwrap();
    let restored = temp.path().join("restored");
    local_recovery::restore(&backup, &restored).unwrap();
    local_recovery::activate(&restored, true, true).unwrap();
    assert!(
        fs::read_dir(restored.join("data/objects").join(&empty_shard))
            .unwrap()
            .next()
            .is_none()
    );
    for (relative, bytes) in &scratch {
        assert_eq!(
            fs::read(restored.join("data/objects").join(relative)).unwrap(),
            *bytes
        );
    }
    let engine = Engine::open(restored.join("data")).unwrap();
    assert_eq!(
        engine.read_file(&revision, "hello.txt").unwrap(),
        b"retained"
    );
    let next = engine
        .checkpoint(
            Some(&revision),
            &[FileChange {
                path: "hello.txt".into(),
                content: Some(b"continued after restore".to_vec()),
            }],
        )
        .unwrap();
    assert_eq!(
        engine.read_file(&next, "hello.txt").unwrap(),
        b"continued after restore"
    );
}

#[test]
fn unknown_object_entries_cannot_masquerade_as_publication_scratch() {
    let cases = [
        ("ab/.tmp-0-0-ab0123456789", false),
        ("ab/.tmp-01-0-ab0123456789", false),
        ("ab/.tmp-4294967296-0-ab0123456789", false),
        ("ab/.tmp-1-01-ab0123456789", false),
        ("ab/.tmp-1-18446744073709551616-ab0123456789", false),
        ("ab/.tmp-1-0-ac0123456789", false),
        ("ab/.tmp-1-0-ab012345678A", false),
        ("ab/.tmp-1-0-ab0123456789-extra", false),
        ("ab/unknown", false),
        (".tmp-1-0-ab0123456789", false),
        ("unknown", true),
        ("ZZ", true),
        ("ab/nested", true),
    ];
    for (relative, directory) in cases {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        populated(&data);
        let path = data.join("objects").join(relative);
        if directory {
            fs::create_dir_all(path).unwrap();
        } else {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"unrecognized entry").unwrap();
        }
        let output = temp.path().join("rejected");
        assert!(
            local_recovery::backup(&data, &output, &[], &[], true).is_err(),
            "accepted {relative}"
        );
        assert!(!output.exists(), "published output for {relative}");
    }
}

#[test]
fn publication_scratch_never_substitutes_for_a_missing_or_corrupted_canonical_object() {
    for corrupt in [false, true] {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        let (_, revision, _, _) = populated(&data);
        let shard = data.join("objects").join(&revision[..2]);
        let canonical = shard.join(&revision[2..]);
        let scratch = shard.join(format!(".tmp-42-0-{}", &revision[..12]));
        let bytes = fs::read(&canonical).unwrap();
        fs::write(&scratch, &bytes).unwrap();
        if corrupt {
            fs::write(&canonical, b"corrupted published object").unwrap();
        } else {
            fs::remove_file(&canonical).unwrap();
        }
        let output = temp.path().join("rejected");
        assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
        assert!(!output.exists());
        assert_eq!(fs::read(scratch).unwrap(), bytes);
    }
}

#[cfg(unix)]
#[test]
fn publication_scratch_still_rejects_links_and_oversized_files() {
    for linked in [false, true] {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        populated(&data);
        let scratch = data.join("objects/ab/.tmp-42-0-ab0123456789");
        fs::create_dir_all(scratch.parent().unwrap()).unwrap();
        if linked {
            let external = temp.path().join("ordinary-file");
            fs::write(&external, b"outside object store").unwrap();
            std::os::unix::fs::symlink(external, &scratch).unwrap();
        } else {
            fs::File::create(&scratch)
                .unwrap()
                .set_len(agentscm::store::MAX_OBJECT_BYTES + 1)
                .unwrap();
        }
        let output = temp.path().join("rejected");
        assert!(local_recovery::backup(&data, &output, &[], &[], true).is_err());
        assert!(!output.exists());
    }
}
