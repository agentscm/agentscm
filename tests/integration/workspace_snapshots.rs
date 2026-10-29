use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use agentscm::Error;
use agentscm::context::ChangeId;
use agentscm::model::FileChange;
use agentscm::store::Engine;
use agentscm::workspace::{
    CapturePause, CaptureResult, CaptureSkip, CheckEvidenceCoverage, CheckLinkRequest,
    CheckLinkResult, OperationOutcome, SnapshotTrigger, TimelineEntry, Workspace,
};
use rusqlite::{Connection, OptionalExtension, params};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    data: std::path::PathBuf,
    root: std::path::PathBuf,
    engine: Engine,
    workspace: Workspace,
    base: String,
    change: ChangeId,
}

fn put(path: &str, content: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(content.to_vec()),
    }
}

fn fixture(scoped: bool) -> Fixture {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let root = temp.path().join("work");
    let engine = Engine::open(&data).unwrap();
    let base = engine
        .checkpoint(
            None,
            &[
                put("selected.txt", b"selected base"),
                put("outside.txt", b"outside base"),
            ],
        )
        .unwrap();
    let change = ChangeId {
        repository: "repository".into(),
        id: "change-one".into(),
    };
    let workspace = if scoped {
        Workspace::create_scoped(
            &engine,
            &root,
            change.clone(),
            &base,
            &["selected.txt".into()],
        )
        .unwrap()
    } else {
        Workspace::create(&engine, &root, change.clone(), &base).unwrap()
    };
    Fixture {
        _temp: temp,
        data,
        root,
        engine,
        workspace,
        base,
        change,
    }
}

#[test]
fn repeated_capture_is_non_advancing_deduplicated_and_scope_preserving() {
    let mut fixture = fixture(true);
    fs::write(fixture.root.join("selected.txt"), b"selected edit").unwrap();
    fs::write(fixture.root.join("outside.txt"), b"outside local").unwrap();
    let before = fixture.workspace.state().unwrap();
    let legacy_probe = fixture
        .root
        .join(".agentscm-workspace/.agentscm-probe-sentinel");
    fs::write(&legacy_probe, b"live legacy probe").unwrap();

    let entry = match fixture
        .workspace
        .capture(&fixture.engine, SnapshotTrigger::Manual)
        .unwrap()
    {
        CaptureResult::Captured { entry } => entry,
        other => panic!("unexpected capture result: {other:?}"),
    };
    assert_eq!(fs::read(&legacy_probe).unwrap(), b"live legacy probe");
    assert!(
        !fixture
            .root
            .join(".agentscm-workspace/timeline-probes")
            .exists()
    );
    assert_eq!(fixture.workspace.state().unwrap(), before);
    assert_eq!(entry.scope, before.scope);
    assert_eq!(entry.changed_paths, 1);
    assert_eq!(
        fixture
            .engine
            .read_file(&entry.revision, "selected.txt")
            .unwrap(),
        b"selected edit"
    );
    assert_eq!(
        fixture
            .engine
            .read_file(&entry.revision, "outside.txt")
            .unwrap(),
        b"outside base"
    );
    assert!(matches!(
        fixture
            .workspace
            .capture(&fixture.engine, SnapshotTrigger::TurnEnd)
            .unwrap(),
        CaptureResult::Unchanged { revision } if revision == entry.revision
    ));
    assert_eq!(
        fixture.workspace.timeline(100, None).unwrap().entries.len(),
        1
    );

    let checkpoint = fixture
        .workspace
        .checkpoint(&fixture.engine, "after-captures", before.generation)
        .unwrap();
    assert!(matches!(
        checkpoint,
        OperationOutcome::Checkpointed { generation: 1, .. }
    ));
    assert_eq!(
        fs::read(fixture.root.join("outside.txt")).unwrap(),
        b"outside local"
    );
}

#[test]
fn sparse_capture_does_not_deduplicate_across_base_revisions() {
    let fixture = fixture(true);
    fs::write(fixture.root.join("selected.txt"), b"selected edit").unwrap();
    let first = match fixture
        .workspace
        .capture(&fixture.engine, SnapshotTrigger::Manual)
        .unwrap()
    {
        CaptureResult::Captured { entry } => entry,
        other => panic!("unexpected first capture result: {other:?}"),
    };
    let next_base = fixture
        .engine
        .checkpoint(
            Some(&fixture.base),
            &[put("outside.txt", b"outside advanced")],
        )
        .unwrap();
    let database = fixture.root.join(".agentscm-workspace/state.sqlite");
    let db = Connection::open(&database).unwrap();
    db.execute(
        "UPDATE workspace_state SET base_revision=?1,generation=1 WHERE singleton=1",
        [&next_base],
    )
    .unwrap();
    drop(db);

    let second = match fixture
        .workspace
        .capture(&fixture.engine, SnapshotTrigger::TurnEnd)
        .unwrap()
    {
        CaptureResult::Captured { entry } => entry,
        other => panic!("unexpected second capture result: {other:?}"),
    };
    assert_ne!(second.revision, first.revision);
    assert_eq!(second.base_revision, next_base);
    assert_eq!(second.generation, 1);
    assert_eq!(
        fixture
            .engine
            .read_file(&second.revision, "outside.txt")
            .unwrap(),
        b"outside advanced"
    );
    assert_eq!(
        fixture.workspace.timeline(100, None).unwrap().entries.len(),
        2
    );
}

#[test]
fn quota_and_deadline_stop_before_object_publication() {
    let fixture = fixture(false);
    fs::write(fixture.root.join("selected.txt"), b"deadline edit").unwrap();
    let before = object_files(&fixture.data.join("objects"));
    assert!(matches!(
        fixture
            .workspace
            .capture_with_deadline(
                &fixture.engine,
                SnapshotTrigger::McpExplicit,
                Duration::ZERO,
            )
            .unwrap(),
        CaptureResult::Skipped {
            reason: CaptureSkip::Deadline
        }
    ));
    assert_eq!(object_files(&fixture.data.join("objects")), before);

    let db = Connection::open(fixture.root.join(".agentscm-workspace/state.sqlite")).unwrap();
    db.execute(
        "INSERT INTO timeline_entries(
            kind,trigger,recorded_at,source_revision,base_revision,generation,scope_json,
            changed_paths,admitted_bytes
         ) VALUES ('snapshot','manual',0,?1,?1,0,?2,1,1073741824)",
        params![
            fixture.base,
            serde_json::to_vec(&Option::<Vec<String>>::None).unwrap()
        ],
    )
    .unwrap();
    drop(db);
    assert!(matches!(
        fixture
            .workspace
            .capture(&fixture.engine, SnapshotTrigger::Manual)
            .unwrap(),
        CaptureResult::Paused {
            reason: CapturePause::ByteLimit
        }
    ));
    assert_eq!(object_files(&fixture.data.join("objects")), before);

    let mut db = Connection::open(fixture.root.join(".agentscm-workspace/state.sqlite")).unwrap();
    let transaction = db.transaction().unwrap();
    for _ in 1..4_096 {
        transaction
            .execute(
                "INSERT INTO timeline_entries(
                    kind,trigger,recorded_at,source_revision,base_revision,generation,scope_json,
                    changed_paths,admitted_bytes
                 ) VALUES ('snapshot','manual',0,?1,?1,0,?2,1,0)",
                params![
                    fixture.base,
                    serde_json::to_vec(&Option::<Vec<String>>::None).unwrap()
                ],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    assert!(matches!(
        fixture
            .workspace
            .capture(&fixture.engine, SnapshotTrigger::Manual)
            .unwrap(),
        CaptureResult::Paused {
            reason: CapturePause::EntryLimit
        }
    ));
    assert_eq!(object_files(&fixture.data.join("objects")), before);
}

#[test]
fn pending_and_incomplete_workspaces_skip_without_claiming_capture() {
    let fixture = fixture(false);
    fs::write(fixture.root.join("selected.txt"), b"edit").unwrap();
    let database = fixture.root.join(".agentscm-workspace/state.sqlite");
    let db = Connection::open(&database).unwrap();
    db.execute(
        "INSERT INTO operations(
            operation_id,kind,request_digest,status,expected_generation,before_revision
         ) VALUES ('pending','checkpoint',?1,'pending',0,?2)",
        params!["a".repeat(64), fixture.base],
    )
    .unwrap();
    drop(db);
    assert!(matches!(
        fixture
            .workspace
            .capture(&fixture.engine, SnapshotTrigger::Manual)
            .unwrap(),
        CaptureResult::Skipped {
            reason: CaptureSkip::OperationPending
        }
    ));

    let db = Connection::open(&database).unwrap();
    db.execute("DELETE FROM operations WHERE operation_id='pending'", [])
        .unwrap();
    db.execute(
        "UPDATE workspace_state SET lifecycle='initializing' WHERE singleton=1",
        [],
    )
    .unwrap();
    drop(db);
    assert!(matches!(
        fixture
            .workspace
            .capture(&fixture.engine, SnapshotTrigger::Manual)
            .unwrap(),
        CaptureResult::Skipped {
            reason: CaptureSkip::WorkspaceIncomplete
        }
    ));
}

#[test]
fn check_links_are_exact_references_and_strictly_deduplicated() {
    let fixture = fixture(false);
    let tested = fixture
        .engine
        .checkpoint(Some(&fixture.base), &[put("prepared.txt", b"prepared")])
        .unwrap();
    let request = CheckLinkRequest {
        source_revision: fixture.base.clone(),
        tested_revision: tested.clone(),
        change: fixture.change.clone(),
        change_version: 3,
        context_digest: "b".repeat(64),
        policy_epoch: 7,
        target: "main".into(),
        target_revision: Some(fixture.base.clone()),
        target_version: Some(9),
        requirement_digest: "c".repeat(64),
        run_id: format!("loop-{}", "d".repeat(59)),
    };
    let entry = match fixture
        .workspace
        .record_check_start(&fixture.engine, &request)
        .unwrap()
    {
        CheckLinkResult::Recorded { entry } => entry,
        other => panic!("unexpected check link result: {other:?}"),
    };
    assert_eq!(entry.source_revision, fixture.base);
    assert_eq!(entry.tested_revision, tested);
    assert_eq!(
        entry.evidence_coverage,
        CheckEvidenceCoverage::ReferenceOnly
    );
    assert!(matches!(
        fixture
            .workspace
            .record_check_start(&fixture.engine, &request)
            .unwrap(),
        CheckLinkResult::AlreadyRecorded { .. }
    ));

    let mut reused_identity = request.clone();
    reused_identity.tested_revision = fixture.base.clone();
    assert!(matches!(
        fixture
            .workspace
            .record_check_start(&fixture.engine, &reused_identity),
        Err(Error::Conflict(message)) if message.contains("reused with different check inputs")
    ));

    let mut wrong_source = request.clone();
    wrong_source.source_revision = tested.clone();
    wrong_source.run_id = format!("loop-{}", "e".repeat(59));
    assert!(matches!(
        fixture
            .workspace
            .record_check_start(&fixture.engine, &wrong_source)
            .unwrap(),
        CheckLinkResult::Skipped {
            reason: CaptureSkip::WorkspaceChanged
        }
    ));
    let timeline = fixture.workspace.timeline(100, None).unwrap();
    assert!(!timeline.coverage.includes_pre_feature_history);
    assert!(matches!(
        timeline.entries.as_slice(),
        [TimelineEntry::CheckLink(_)]
    ));

    let mut encoded = serde_json::to_value(&request).unwrap();
    encoded
        .as_object_mut()
        .unwrap()
        .insert("unknown".into(), true.into());
    assert!(serde_json::from_value::<CheckLinkRequest>(encoded).is_err());
}

#[test]
fn concurrent_capture_does_not_block_checkpoint_or_leave_pending_state() {
    for attempt in 0..16 {
        let fixture = fixture(false);
        fs::write(
            fixture.root.join("selected.txt"),
            vec![b'x'; 256 * 1024 + attempt],
        )
        .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let capture_workspace = fixture.workspace.clone();
        let capture_engine = fixture.engine.clone();
        let capture_barrier = barrier.clone();
        let capture = std::thread::spawn(move || {
            capture_barrier.wait();
            capture_workspace.capture(&capture_engine, SnapshotTrigger::TurnEnd)
        });
        let mut checkpoint_workspace = fixture.workspace.clone();
        let checkpoint_engine = fixture.engine.clone();
        let checkpoint_barrier = barrier.clone();
        let checkpoint = std::thread::spawn(move || {
            checkpoint_barrier.wait();
            checkpoint_workspace.checkpoint(&checkpoint_engine, "parallel-checkpoint", 0)
        });
        barrier.wait();
        let capture = capture.join().unwrap().unwrap();
        let checkpoint = checkpoint.join().unwrap().unwrap();
        assert!(matches!(
            capture,
            CaptureResult::Captured { .. }
                | CaptureResult::Unchanged { .. }
                | CaptureResult::Skipped {
                    reason: CaptureSkip::WorkspaceChanged | CaptureSkip::OperationPending
                }
        ));
        assert!(matches!(
            checkpoint,
            OperationOutcome::Checkpointed { generation: 1, .. }
        ));
        assert!(fixture.workspace.state().unwrap().pending.is_none());
    }
}

#[test]
fn migration_and_fork_preserve_retained_scope_and_enforce_guards() {
    let fixture = fixture(true);
    let database = fixture.root.join(".agentscm-workspace/state.sqlite");
    drop(fixture.workspace);
    let db = Connection::open(&database).unwrap();
    db.execute_batch(
        "DROP INDEX timeline_check_link_identity;
         DROP TABLE timeline_entries;
         PRAGMA user_version=4;",
    )
    .unwrap();
    drop(db);
    let workspace = Workspace::open(&fixture.root).unwrap();
    assert!(workspace.timeline(10, None).unwrap().entries.is_empty());
    fs::write(fixture.root.join("selected.txt"), b"fork point").unwrap();
    let entry = match workspace
        .capture(&fixture.engine, SnapshotTrigger::Manual)
        .unwrap()
    {
        CaptureResult::Captured { entry } => entry,
        other => panic!("unexpected capture result: {other:?}"),
    };
    let destination = fixture._temp.path().join("fork");
    let fork = workspace
        .fork_from_entry(
            &fixture.engine,
            entry.id,
            &destination,
            ChangeId {
                repository: fixture.change.repository.clone(),
                id: "change-two".into(),
            },
        )
        .unwrap();
    assert_eq!(fork.state().unwrap().scope, entry.scope);
    assert_eq!(
        fs::read(destination.join("selected.txt")).unwrap(),
        b"fork point"
    );
    assert!(!destination.join("outside.txt").exists());
    assert!(
        workspace
            .fork_from_entry(
                &fixture.engine,
                entry.id,
                fixture.root.join("nested"),
                ChangeId {
                    repository: fixture.change.repository.clone(),
                    id: "change-three".into(),
                }
            )
            .is_err()
    );
    assert!(
        workspace
            .fork_from_entry(
                &fixture.engine,
                entry.id,
                fixture._temp.path().join("same-change"),
                fixture.change.clone(),
            )
            .is_err()
    );
    assert!(
        workspace
            .fork_from_entry(
                &fixture.engine,
                entry.id,
                &destination,
                ChangeId {
                    repository: fixture.change.repository.clone(),
                    id: "change-four".into(),
                },
            )
            .is_err()
    );
    assert!(
        workspace
            .fork_from_entry(
                &fixture.engine,
                entry.id,
                fixture._temp.path().join("other-repository"),
                ChangeId {
                    repository: "other-repository".into(),
                    id: "change-five".into(),
                },
            )
            .is_err()
    );
    let db = Connection::open(&database).unwrap();
    db.execute(
        "INSERT INTO timeline_entries(
            kind,trigger,recorded_at,source_revision,base_revision,generation,scope_json,
            changed_paths,admitted_bytes
         ) VALUES ('snapshot','manual',0,?1,?2,0,?3,1,1)",
        params![
            "f".repeat(64),
            fixture.base,
            serde_json::to_vec(&entry.scope).unwrap()
        ],
    )
    .unwrap();
    let missing_id = u64::try_from(db.last_insert_rowid()).unwrap();
    drop(db);
    assert!(
        workspace
            .fork_from_entry(
                &fixture.engine,
                missing_id,
                fixture._temp.path().join("missing-revision"),
                ChangeId {
                    repository: fixture.change.repository.clone(),
                    id: "change-six".into(),
                },
            )
            .is_err()
    );
}

#[test]
fn read_only_v4_preserves_state_status_and_empty_timeline_without_migrating() {
    let fixture = fixture(true);
    let database = fixture.root.join(".agentscm-workspace/state.sqlite");
    drop(fixture.workspace);
    let db = Connection::open(&database).unwrap();
    db.execute_batch(
        "DROP INDEX timeline_check_link_identity;
         DROP TABLE timeline_entries;
         PRAGMA user_version=4;",
    )
    .unwrap();
    drop(db);

    let workspace = Workspace::open_read_only(&fixture.root).unwrap();
    let state = workspace.state().unwrap();
    assert_eq!(state.base_revision, fixture.base);
    let status = workspace.status(&fixture.engine, 10).unwrap();
    assert!(status.complete);
    assert!(status.changes.is_empty());
    let timeline = workspace.timeline(10, None).unwrap();
    assert!(timeline.entries.is_empty());
    assert!(timeline.coverage.earliest_recorded_id.is_none());
    assert!(!timeline.coverage.includes_pre_feature_history);
    assert!(matches!(
        workspace.timeline_entry(1),
        Err(Error::NotFound(_))
    ));
    drop(workspace);

    let db = Connection::open(&database).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    assert!(
        db.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='timeline_entries'",
            [],
            |_| Ok(())
        )
        .optional()
        .unwrap()
        .is_none()
    );
}

fn object_files(root: &Path) -> usize {
    let mut pending = vec![root.to_path_buf()];
    let mut files = 0;
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                files += 1;
            }
        }
    }
    files
}
