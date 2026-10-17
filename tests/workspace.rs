use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use agentscm::Error;
use agentscm::context::ChangeId;
use agentscm::model::{EntryChange, EntryKind, FileChange};
use agentscm::store::{Engine, digest};
use agentscm::workspace::{
    METADATA_DIRECTORY, OperationOutcome, RecoveryChoice, Workspace, WorkspaceChange,
};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

type StagedFixture<'a> = (&'a str, Option<&'a [u8]>, Option<&'a [u8]>);

fn put(path: &str, content: &[u8]) -> FileChange {
    FileChange {
        path: path.to_owned(),
        content: Some(content.to_vec()),
    }
}

fn fixture() -> (TempDir, Engine, String, ChangeId) {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path().join("source")).unwrap();
    let revision = engine
        .checkpoint(None, &[put("a.txt", b"old"), put("dir/kept.txt", b"kept")])
        .unwrap();
    let change = ChangeId {
        repository: "repository-1".to_owned(),
        id: "change-1".to_owned(),
    };
    (directory, engine, revision, change)
}

fn single_file_fixture(path: &str, content: &[u8]) -> (TempDir, Engine, String, ChangeId) {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path().join("source")).unwrap();
    let revision = engine.checkpoint(None, &[put(path, content)]).unwrap();
    let change = ChangeId {
        repository: "repository-1".to_owned(),
        id: "change-1".to_owned(),
    };
    (directory, engine, revision, change)
}

fn database(root: &std::path::Path) -> Connection {
    Connection::open(root.join(METADATA_DIRECTORY).join("state.sqlite")).unwrap()
}

fn seed_pending_undo(root: &std::path::Path, current: &str, previous: &str) {
    seed_pending_transition_undo(
        root,
        current,
        previous,
        &[
            ("a.txt", Some(b"new"), Some(b"old")),
            ("added.txt", Some(b"added"), None),
        ],
    );
}

fn seed_pending_transition_undo(
    root: &std::path::Path,
    current: &str,
    previous: &str,
    stages: &[StagedFixture<'_>],
) {
    let connection = database(root);
    connection
        .execute(
            "INSERT INTO operations(operation_id, kind, request_digest, status, expected_generation, before_revision, after_revision, checkpoint_operation_id) VALUES ('pending-undo', 'undo', ?1, 'pending', 1, ?2, ?3, 'checkpoint-1')",
            params!["0".repeat(64), current, previous],
        )
        .unwrap();
    for (path, before, after) in stages {
        connection
            .execute(
                "INSERT INTO staged_paths(operation_id, path, before_content, after_content) VALUES ('pending-undo', ?1, ?2, ?3)",
                params![path, before, after],
            )
            .unwrap();
    }
}

fn recovery_temp(
    root: &std::path::Path,
    operation: &str,
    path: &str,
    content: &[u8],
) -> std::path::PathBuf {
    recovery_temp_of_kind(root, operation, path, "regular", content)
}

fn recovery_temp_of_kind(
    root: &std::path::Path,
    operation: &str,
    path: &str,
    kind: &str,
    content: &[u8],
) -> std::path::PathBuf {
    let operation = digest(operation.as_bytes());
    let mut hasher = Sha256::new();
    // Regular files use the original TEMP1 layout (path, content); other
    // kinds use TEMP2, which adds the kind.
    let regular = kind == "regular";
    hasher.update(if regular {
        b"AGSCM-WORKSPACE-TEMP1\0"
    } else {
        b"AGSCM-WORKSPACE-TEMP2\0"
    });
    hasher.update((path.len() as u64).to_be_bytes());
    hasher.update(path.as_bytes());
    if !regular {
        hasher.update((kind.len() as u64).to_be_bytes());
        hasher.update(kind.as_bytes());
    }
    hasher.update((content.len() as u64).to_be_bytes());
    hasher.update(content);
    let identity = format!("{:x}", hasher.finalize());
    root.join(METADATA_DIRECTORY)
        .join("temps")
        .join(&operation[..16])
        .join(&identity[..32])
}

#[test]
fn creates_without_overwriting_and_tracks_checkpoint_retry_and_undo() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change.clone(), &base).unwrap();
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"old");
    assert!(root.join("dir/kept.txt").is_file());
    assert!(matches!(
        Workspace::create(&engine, &root, change, &base),
        Err(Error::Conflict(_))
    ));

    fs::write(root.join("a.txt"), b"new").unwrap();
    fs::write(root.join("new.txt"), b"added").unwrap();
    fs::write(root.join(METADATA_DIRECTORY).join("ignored"), b"private").unwrap();
    let status = workspace.status(&engine, 10).unwrap();
    assert!(status.complete);
    assert_eq!(
        status.changes,
        vec![
            WorkspaceChange::Modified {
                path: "a.txt".to_owned()
            },
            WorkspaceChange::Added {
                path: "new.txt".to_owned()
            }
        ]
    );

    let checkpoint = workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap();
    assert_eq!(
        checkpoint,
        workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap()
    );
    assert!(matches!(
        workspace.checkpoint(&engine, "checkpoint-1", 1),
        Err(Error::Conflict(_))
    ));
    let revision = match checkpoint {
        OperationOutcome::Checkpointed {
            revision,
            generation: 1,
        } => revision,
        other => panic!("unexpected checkpoint outcome: {other:?}"),
    };
    assert_eq!(engine.read_file(&revision, "a.txt").unwrap(), b"new");

    fs::write(root.join("local.txt"), b"untracked").unwrap();
    let undone = workspace
        .undo(&engine, "undo-1", 1, "checkpoint-1")
        .unwrap();
    assert_eq!(
        undone,
        OperationOutcome::Undone {
            revision: base.clone(),
            generation: 2
        }
    );
    assert_eq!(
        undone,
        workspace
            .undo(&engine, "undo-1", 1, "checkpoint-1")
            .unwrap()
    );
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"old");
    assert!(!root.join("new.txt").exists());
    assert_eq!(fs::read(root.join("local.txt")).unwrap(), b"untracked");
}

#[test]
fn create_rejects_an_over_byte_limit_source_without_reserving_the_destination() {
    let (directory, engine, base, change) = fixture();
    let large = vec![b'x'; 64 * 1024 * 1024];
    let first = engine
        .checkpoint(Some(&base), &[put("large-a.bin", &large)])
        .unwrap();
    let over_limit = engine
        .checkpoint(Some(&first), &[put("large-b.bin", &large)])
        .unwrap();
    drop(large);
    let root = directory.path().join("workspace");

    assert!(matches!(
        Workspace::create(&engine, &root, change, &over_limit),
        Err(Error::Limit(_))
    ));
    assert!(!root.exists());
}

#[test]
fn create_rejects_an_over_depth_source_without_reserving_the_destination() {
    let (directory, engine, base, change) = fixture();
    let path = format!("{}/file", vec!["d"; 129].join("/"));
    let over_limit = engine
        .checkpoint(Some(&base), &[put(&path, b"deep")])
        .unwrap();
    let root = directory.path().join("workspace");

    assert!(matches!(
        Workspace::create(&engine, &root, change, &over_limit),
        Err(Error::Limit(_))
    ));
    assert!(!root.exists());
}

#[test]
fn create_rejects_the_reserved_source_namespace_before_reserving_the_destination() {
    let (directory, engine, base, change) = fixture();
    let revision = engine
        .checkpoint(
            Some(&base),
            &[put(".agentscm-workspace/nested", b"reserved")],
        )
        .unwrap();
    let root = directory.path().join("workspace");

    assert!(matches!(
        Workspace::create(&engine, &root, change, &revision),
        Err(Error::Conflict(_))
    ));
    assert!(!root.exists());
}

#[test]
fn create_rejects_reserved_names_in_any_case_and_position() {
    for path in [
        ".AGENTSCM-WORKSPACE/x",
        ".Git/x",
        "src/.git/hooks/post-checkout",
    ] {
        let (directory, engine, base, change) = fixture();
        let revision = engine
            .checkpoint(Some(&base), &[put(path, b"reserved")])
            .unwrap();
        let root = directory.path().join("workspace");

        assert!(
            matches!(
                Workspace::create(&engine, &root, change, &revision),
                Err(Error::Conflict(_))
            ),
            "{path}"
        );
        assert!(!root.exists(), "{path}");
    }
}

#[test]
fn reserved_names_in_the_working_tree_block_checkpoints_without_staging() {
    for name in [".git", ".AGENTSCM"] {
        let (directory, engine, base, change) = fixture();
        let root = directory.path().join("workspace");
        let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("config"), b"planted").unwrap();

        assert!(
            matches!(workspace.status(&engine, 100), Err(Error::Invalid(_))),
            "{name}"
        );
        assert!(
            matches!(
                workspace.checkpoint(&engine, "checkpoint-1", 0),
                Err(Error::Invalid(_))
            ),
            "{name}"
        );
        fs::remove_dir_all(root.join(name)).unwrap();
        // Nothing was staged or consumed: the same operation ID at the same generation still works.
        fs::write(root.join("a.txt"), b"edited").unwrap();
        workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap();
    }
}

#[test]
fn undo_refuses_dirty_affected_paths_without_writing() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::write(root.join("a.txt"), b"checkpointed").unwrap();
    workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap();
    fs::write(root.join("a.txt"), b"later user edit").unwrap();

    assert_eq!(
        workspace
            .undo(&engine, "undo-1", 1, "checkpoint-1")
            .unwrap(),
        OperationOutcome::Blocked {
            paths: vec!["a.txt".to_owned()]
        }
    );
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"later user edit");
    let state = workspace.state().unwrap();
    assert_eq!(state.generation, 1);
    assert!(state.pending.is_none());
}

#[test]
fn recover_completes_a_durable_pending_checkpoint() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::write(root.join("a.txt"), b"staged after crash").unwrap();
    let connection = database(&root);
    connection
        .execute(
            "INSERT INTO operations(operation_id, kind, request_digest, status, expected_generation, before_revision) VALUES ('pending-checkpoint', 'checkpoint', ?1, 'pending', 0, ?2)",
            params!["0".repeat(64), base],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO staged_paths(operation_id, path, before_content, after_content) VALUES ('pending-checkpoint', 'a.txt', ?1, ?2)",
            params![b"old".as_slice(), b"staged after crash".as_slice()],
        )
        .unwrap();
    drop(connection);

    let outcome = workspace
        .recover(&engine, RecoveryChoice::Complete)
        .unwrap();
    let revision = match outcome {
        OperationOutcome::Checkpointed {
            revision,
            generation: 1,
        } => revision,
        other => panic!("unexpected recovery outcome: {other:?}"),
    };
    assert_eq!(
        engine.read_file(&revision, "a.txt").unwrap(),
        b"staged after crash"
    );
    assert!(workspace.state().unwrap().pending.is_none());
}

#[cfg(unix)]
#[test]
fn killed_checkpoint_process_recovers_its_staged_bytes() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    Workspace::create(&engine, &root, change, &base).unwrap();
    let changed = vec![b'x'; 32 * 1024 * 1024];
    fs::write(root.join("a.txt"), &changed).unwrap();

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("subprocess_checkpoint_helper")
        .arg("--nocapture")
        .env("AGENTSCM_WORKSPACE_CHILD", "1")
        .env("AGENTSCM_WORKSPACE_ROOT", &root)
        .env("AGENTSCM_SOURCE_ROOT", directory.path().join("source"))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let observed_pending = loop {
        let connection = database(&root);
        let status = connection.query_row(
            "SELECT status FROM operations WHERE operation_id = 'killed-checkpoint'",
            [],
            |row| row.get::<_, String>(0),
        );
        if status.as_deref() == Ok("pending") {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::yield_now();
    };
    let killed = child.kill();
    let waited = child.wait();
    assert!(
        observed_pending,
        "checkpoint did not expose its durable pending intent"
    );
    killed.unwrap();
    waited.unwrap();

    let mut workspace = Workspace::open(&root).unwrap();
    assert_eq!(
        workspace.state().unwrap().pending.unwrap().operation_id,
        "killed-checkpoint"
    );
    let outcome = workspace
        .recover(&engine, RecoveryChoice::Complete)
        .unwrap();
    let revision = match outcome {
        OperationOutcome::Checkpointed {
            revision,
            generation: 1,
        } => revision,
        other => panic!("unexpected recovery outcome: {other:?}"),
    };
    assert_eq!(engine.read_file(&revision, "a.txt").unwrap(), changed);
}

#[test]
fn subprocess_checkpoint_helper() {
    if std::env::var_os("AGENTSCM_WORKSPACE_CHILD").is_none() {
        return;
    }
    let root = std::env::var_os("AGENTSCM_WORKSPACE_ROOT").unwrap();
    let source = std::env::var_os("AGENTSCM_SOURCE_ROOT").unwrap();
    let engine = Engine::open(source).unwrap();
    let mut workspace = Workspace::open(root).unwrap();
    workspace
        .checkpoint(&engine, "killed-checkpoint", 0)
        .unwrap();
}

#[test]
fn recover_finishes_an_interrupted_initial_materialization() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::remove_file(root.join("a.txt")).unwrap();
    let connection = database(&root);
    connection
        .execute(
            "UPDATE workspace_state SET lifecycle = 'initializing' WHERE singleton = 1",
            [],
        )
        .unwrap();
    drop(connection);

    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::Complete)
            .unwrap(),
        OperationOutcome::Recovered { generation: 0 }
    );
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"old");
    assert!(workspace.state().unwrap().ready);
}

#[test]
fn recovery_blocks_divergent_partial_undo_then_completes_after_resolution() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::write(root.join("a.txt"), b"new").unwrap();
    fs::write(root.join("added.txt"), b"added").unwrap();
    let current = match workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap() {
        OperationOutcome::Checkpointed { revision, .. } => revision,
        other => panic!("unexpected checkpoint outcome: {other:?}"),
    };
    seed_pending_undo(&root, &current, &base);
    fs::write(root.join("a.txt"), b"divergent user bytes").unwrap();

    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::Complete)
            .unwrap(),
        OperationOutcome::Blocked {
            paths: vec!["a.txt".to_owned()]
        }
    );
    assert_eq!(
        fs::read(root.join("a.txt")).unwrap(),
        b"divergent user bytes"
    );
    assert!(workspace.state().unwrap().pending.is_some());

    fs::write(root.join("a.txt"), b"old").unwrap();
    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::Complete)
            .unwrap(),
        OperationOutcome::Undone {
            revision: base,
            generation: 2
        }
    );
    assert!(!root.join("added.txt").exists());
}

#[test]
fn rollback_restores_the_before_side_of_a_partial_undo() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::write(root.join("a.txt"), b"new").unwrap();
    fs::write(root.join("added.txt"), b"added").unwrap();
    let current = match workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap() {
        OperationOutcome::Checkpointed { revision, .. } => revision,
        other => panic!("unexpected checkpoint outcome: {other:?}"),
    };
    seed_pending_undo(&root, &current, &base);
    // Simulate a crash after the replacement landed but before journal completion.
    fs::write(root.join("a.txt"), b"old").unwrap();

    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::RollBack)
            .unwrap(),
        OperationOutcome::Cancelled { generation: 1 }
    );
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"new");
    assert_eq!(fs::read(root.join("added.txt")).unwrap(), b"added");
    let state = workspace.state().unwrap();
    assert_eq!(state.base_revision, current);
    assert_eq!(state.generation, 1);
    assert!(state.pending.is_none());
}

#[test]
fn rollback_restores_a_checkpoint_tree_after_partial_file_undo() {
    let (directory, engine, base, change) = single_file_fixture("a", b"old");
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::remove_file(root.join("a")).unwrap();
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/c"), b"new").unwrap();
    let current = match workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap() {
        OperationOutcome::Checkpointed { revision, .. } => revision,
        other => panic!("unexpected checkpoint outcome: {other:?}"),
    };
    seed_pending_transition_undo(
        &root,
        &current,
        &base,
        &[("a", None, Some(b"old")), ("a/b/c", Some(b"new"), None)],
    );
    fs::remove_dir_all(root.join("a")).unwrap();
    fs::write(root.join("a"), b"old").unwrap();
    fs::write(root.join("local.txt"), b"keep").unwrap();

    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::RollBack)
            .unwrap(),
        OperationOutcome::Cancelled { generation: 1 }
    );
    assert_eq!(fs::read(root.join("a/b/c")).unwrap(), b"new");
    assert_eq!(fs::read(root.join("local.txt")).unwrap(), b"keep");
    assert_eq!(workspace.state().unwrap().base_revision, current);
}

#[test]
fn rollback_restores_a_checkpoint_file_after_partial_tree_undo() {
    let (directory, engine, base, change) = single_file_fixture("a/b/c", b"old");
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::remove_dir_all(root.join("a")).unwrap();
    fs::write(root.join("a"), b"new").unwrap();
    let current = match workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap() {
        OperationOutcome::Checkpointed { revision, .. } => revision,
        other => panic!("unexpected checkpoint outcome: {other:?}"),
    };
    seed_pending_transition_undo(
        &root,
        &current,
        &base,
        &[("a", Some(b"new"), None), ("a/b/c", None, Some(b"old"))],
    );
    fs::remove_file(root.join("a")).unwrap();
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/c"), b"old").unwrap();
    fs::write(root.join("local.txt"), b"keep").unwrap();

    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::RollBack)
            .unwrap(),
        OperationOutcome::Cancelled { generation: 1 }
    );
    assert_eq!(fs::read(root.join("a")).unwrap(), b"new");
    assert_eq!(fs::read(root.join("local.txt")).unwrap(), b"keep");
    assert_eq!(workspace.state().unwrap().base_revision, current);
}

#[test]
fn recovery_rebuilds_a_truncated_internal_temp() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::write(root.join("a.txt"), b"new").unwrap();
    fs::write(root.join("added.txt"), b"added").unwrap();
    let current = match workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap() {
        OperationOutcome::Checkpointed { revision, .. } => revision,
        other => panic!("unexpected checkpoint outcome: {other:?}"),
    };
    seed_pending_undo(&root, &current, &base);

    let temporary = recovery_temp(&root, "pending-undo", "a.txt", b"old");
    fs::create_dir_all(temporary.parent().unwrap()).unwrap();
    fs::write(&temporary, b"truncated").unwrap();
    fs::remove_file(root.join("a.txt")).unwrap();

    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::Complete)
            .unwrap(),
        OperationOutcome::Undone {
            revision: base,
            generation: 2
        }
    );
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"old");
    assert!(!root.join("added.txt").exists());
    assert!(!temporary.exists());
}

#[test]
fn recovery_finds_a_regular_file_temp_written_with_the_original_identity() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::write(root.join("a.txt"), b"new").unwrap();
    let current = match workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap() {
        OperationOutcome::Checkpointed { revision, .. } => revision,
        other => panic!("unexpected checkpoint outcome: {other:?}"),
    };
    seed_pending_undo(&root, &current, &base);

    // A complete TEMP1 temporary, as an older binary left it, and the target
    // already removed by the interrupted operation.
    let temporary = recovery_temp(&root, "pending-undo", "a.txt", b"old");
    fs::create_dir_all(temporary.parent().unwrap()).unwrap();
    fs::write(&temporary, b"old").unwrap();
    fs::remove_file(root.join("a.txt")).unwrap();

    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::Complete)
            .unwrap(),
        OperationOutcome::Undone {
            revision: base,
            generation: 2
        }
    );
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"old");
    assert!(!temporary.exists());
}

#[test]
fn undo_replaces_a_tracked_tree_with_its_base_file() {
    let (directory, engine, base, change) = single_file_fixture("a", b"old");
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::remove_file(root.join("a")).unwrap();
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/c"), b"new").unwrap();
    workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap();

    assert_eq!(
        workspace
            .undo(&engine, "undo-1", 1, "checkpoint-1")
            .unwrap(),
        OperationOutcome::Undone {
            revision: base,
            generation: 2
        }
    );
    assert_eq!(fs::read(root.join("a")).unwrap(), b"old");
}

#[test]
fn undo_replaces_a_checkpoint_file_with_its_base_tree() {
    let (directory, engine, base, change) = single_file_fixture("a/b/c", b"old");
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::remove_dir_all(root.join("a")).unwrap();
    fs::write(root.join("a"), b"new").unwrap();
    workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap();

    assert_eq!(
        workspace
            .undo(&engine, "undo-1", 1, "checkpoint-1")
            .unwrap(),
        OperationOutcome::Undone {
            revision: base,
            generation: 2
        }
    );
    assert_eq!(fs::read(root.join("a/b/c")).unwrap(), b"old");
}

#[test]
fn recovery_accepts_a_transition_file_installed_before_journal_completion() {
    let (directory, engine, base, change) = single_file_fixture("a", b"old");
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::remove_file(root.join("a")).unwrap();
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/c"), b"new").unwrap();
    let current = match workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap() {
        OperationOutcome::Checkpointed { revision, .. } => revision,
        other => panic!("unexpected checkpoint outcome: {other:?}"),
    };
    seed_pending_transition_undo(
        &root,
        &current,
        &base,
        &[("a", None, Some(b"old")), ("a/b/c", Some(b"new"), None)],
    );
    fs::remove_dir_all(root.join("a")).unwrap();
    fs::write(root.join("a"), b"old").unwrap();

    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::Complete)
            .unwrap(),
        OperationOutcome::Undone {
            revision: base,
            generation: 2
        }
    );
    assert_eq!(fs::read(root.join("a")).unwrap(), b"old");
}

#[test]
fn undo_preserves_an_untracked_empty_directory_that_blocks_a_file() {
    let (directory, engine, base, change) = single_file_fixture("a", b"old");
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::remove_file(root.join("a")).unwrap();
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/c"), b"new").unwrap();
    workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap();
    fs::create_dir(root.join("a/untracked-empty")).unwrap();

    assert_eq!(
        workspace
            .undo(&engine, "undo-1", 1, "checkpoint-1")
            .unwrap(),
        OperationOutcome::Blocked {
            paths: vec!["a/untracked-empty".to_owned()]
        }
    );
    assert!(root.join("a/untracked-empty").is_dir());
    assert_eq!(fs::read(root.join("a/b/c")).unwrap(), b"new");
    assert!(workspace.state().unwrap().pending.is_none());
}

#[test]
fn status_is_honestly_truncated() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    let workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::write(root.join("a.txt"), b"changed").unwrap();
    fs::write(root.join("new.txt"), b"new").unwrap();

    let status = workspace.status(&engine, 1).unwrap();
    assert!(!status.complete);
    assert_eq!(status.changes.len(), 1);
}

#[test]
fn independent_workspaces_checkpoint_concurrently() {
    let (directory, engine, base, change) = fixture();
    let left_root = directory.path().join("left");
    let right_root = directory.path().join("right");
    Workspace::create(&engine, &left_root, change.clone(), &base).unwrap();
    Workspace::create(&engine, &right_root, change, &base).unwrap();
    fs::write(left_root.join("left.txt"), b"left").unwrap();
    fs::write(right_root.join("right.txt"), b"right").unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let source = directory.path().join("source");
    let threads: Vec<_> = [(left_root, "left-op"), (right_root, "right-op")]
        .into_iter()
        .map(|(root, operation)| {
            let barrier = barrier.clone();
            let source = source.clone();
            thread::spawn(move || {
                let engine = Engine::open(source).unwrap();
                let mut workspace = Workspace::open(root).unwrap();
                barrier.wait();
                workspace.checkpoint(&engine, operation, 0).unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(outcomes.iter().all(|outcome| matches!(
        outcome,
        OperationOutcome::Checkpointed { generation: 1, .. }
    )));
}

fn entry(path: &str, content: &[u8], kind: EntryKind) -> EntryChange {
    EntryChange {
        path: path.to_owned(),
        content: Some(content.to_vec()),
        kind,
    }
}

fn kinds_fixture(entries: &[EntryChange]) -> (TempDir, Engine, String, ChangeId) {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path().join("source")).unwrap();
    let revision = engine.checkpoint_entries(None, entries).unwrap();
    let change = ChangeId {
        repository: "repository-1".to_owned(),
        id: "change-1".to_owned(),
    };
    (directory, engine, revision, change)
}

fn checkpointed(outcome: OperationOutcome) -> String {
    match outcome {
        OperationOutcome::Checkpointed { revision, .. } => revision,
        other => panic!("unexpected checkpoint outcome: {other:?}"),
    }
}

fn mode(path: &std::path::Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode()
}

fn link_target(path: &std::path::Path) -> String {
    fs::read_link(path).unwrap().to_str().unwrap().to_owned()
}

#[test]
fn executable_mode_round_trips_through_checkpoint_materialization_status_and_undo() {
    let script = b"#!/bin/sh\n";
    let (directory, engine, base, change) = kinds_fixture(&[
        entry("run.sh", script, EntryKind::Executable),
        entry("plain.txt", b"plain", EntryKind::Regular),
    ]);
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    assert_ne!(mode(&root.join("run.sh")) & 0o100, 0);
    assert_eq!(mode(&root.join("plain.txt")) & 0o111, 0);
    assert!(workspace.status(&engine, 10).unwrap().changes.is_empty());

    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(root.join("plain.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        workspace.status(&engine, 10).unwrap().changes,
        vec![
            WorkspaceChange::KindChanged {
                path: "plain.txt".to_owned(),
                before: EntryKind::Regular,
                after: EntryKind::Executable,
            },
            WorkspaceChange::KindChanged {
                path: "run.sh".to_owned(),
                before: EntryKind::Executable,
                after: EntryKind::Regular,
            },
        ]
    );
    let revision = checkpointed(workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap());
    assert_eq!(
        engine.read_entry(&revision, "run.sh").unwrap(),
        (EntryKind::Regular, script.to_vec())
    );
    assert_eq!(
        engine.read_entry(&revision, "plain.txt").unwrap(),
        (EntryKind::Executable, b"plain".to_vec())
    );
    assert!(workspace.status(&engine, 10).unwrap().changes.is_empty());

    // Undo materializes the original modes over same-content files.
    workspace
        .undo(&engine, "undo-1", 1, "checkpoint-1")
        .unwrap();
    assert_ne!(mode(&root.join("run.sh")) & 0o100, 0);
    assert_eq!(mode(&root.join("plain.txt")) & 0o111, 0);
    assert_eq!(fs::read(root.join("run.sh")).unwrap(), script);
    assert!(workspace.status(&engine, 10).unwrap().changes.is_empty());
}

#[test]
fn symlinks_round_trip_without_being_followed() {
    let (directory, engine, base, change) = kinds_fixture(&[
        entry("target.txt", b"target", EntryKind::Regular),
        entry("dir/inner.txt", b"inner", EntryKind::Regular),
        entry("file-link", b"target.txt", EntryKind::Symlink),
        entry("dir-link", b"dir", EntryKind::Symlink),
        entry("dangling", b"missing", EntryKind::Symlink),
        entry("dir/up", b"../target.txt", EntryKind::Symlink),
    ]);
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    assert_eq!(link_target(&root.join("file-link")), "target.txt");
    assert_eq!(link_target(&root.join("dir-link")), "dir");
    assert_eq!(link_target(&root.join("dangling")), "missing");
    assert_eq!(link_target(&root.join("dir/up")), "../target.txt");
    // The directory link is one entry; its contents are not scanned twice.
    assert!(workspace.status(&engine, 10).unwrap().changes.is_empty());

    fs::remove_file(root.join("file-link")).unwrap();
    symlink("dir/inner.txt", root.join("file-link")).unwrap();
    assert_eq!(
        workspace.status(&engine, 10).unwrap().changes,
        vec![WorkspaceChange::Modified {
            path: "file-link".to_owned()
        }]
    );
    let revision = checkpointed(workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap());
    assert_eq!(
        engine.read_entry(&revision, "file-link").unwrap(),
        (EntryKind::Symlink, b"dir/inner.txt".to_vec())
    );

    // A regular file replaced by a link is a kind change, and undo restores it.
    fs::remove_file(root.join("target.txt")).unwrap();
    symlink("dir/inner.txt", root.join("target.txt")).unwrap();
    assert_eq!(
        workspace.status(&engine, 10).unwrap().changes,
        vec![WorkspaceChange::KindChanged {
            path: "target.txt".to_owned(),
            before: EntryKind::Regular,
            after: EntryKind::Symlink,
        }]
    );
    checkpointed(workspace.checkpoint(&engine, "checkpoint-2", 1).unwrap());
    workspace
        .undo(&engine, "undo-2", 2, "checkpoint-2")
        .unwrap();
    assert_eq!(fs::read(root.join("target.txt")).unwrap(), b"target");
    assert!(
        !fs::symlink_metadata(root.join("target.txt"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(root.join("dir/inner.txt")).unwrap(), b"inner");
    workspace
        .undo(&engine, "undo-1", 3, "checkpoint-1")
        .unwrap();
    assert_eq!(link_target(&root.join("file-link")), "target.txt");
    assert!(workspace.status(&engine, 10).unwrap().changes.is_empty());
}

#[test]
fn hostile_symlink_targets_are_rejected_by_status_and_checkpoint() {
    let (directory, engine, base, change) = fixture();
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    let outside = directory.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let absolute = outside.to_str().unwrap().to_owned();
    let hostile = [
        ("link", absolute.as_str()),
        ("link", "/etc/passwd"),
        ("link", "../outside"),
        ("link", "x/../.."),
        ("link", "dir/../../outside"),
        ("link", ".agentscm-workspace/state.sqlite"),
        ("link", ".git/config"),
        ("link", "dir/.GIT/config"),
        ("link", "./a.txt"),
        ("link", "dir//a.txt"),
        ("dir/link", "../../outside"),
        ("link", "back\\slash"),
    ];
    for (path, target) in hostile {
        symlink(target, root.join(path)).unwrap();
        match workspace.status(&engine, 10) {
            Err(Error::Invalid(message)) => assert!(message.contains(path), "{target}: {message}"),
            other => panic!("{target}: unexpected status {other:?}"),
        }
        assert!(
            matches!(
                workspace.checkpoint(&engine, "checkpoint-1", 0),
                Err(Error::Invalid(_))
            ),
            "{target}"
        );
        fs::remove_file(root.join(path)).unwrap();
    }
    let state = workspace.state().unwrap();
    assert_eq!((state.generation, state.pending), (0, None));
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert!(workspace.status(&engine, 10).unwrap().changes.is_empty());
}

#[test]
fn materializing_over_a_symlink_replaces_the_link_not_its_target() {
    let (directory, engine, base, change) = kinds_fixture(&[
        entry("one.txt", b"one", EntryKind::Regular),
        entry("two.txt", b"two", EntryKind::Regular),
        entry("link", b"one.txt", EntryKind::Symlink),
    ]);
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::remove_file(root.join("link")).unwrap();
    symlink("two.txt", root.join("link")).unwrap();
    checkpointed(workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap());

    // Undo replaces link -> two.txt with link -> one.txt.
    workspace
        .undo(&engine, "undo-1", 1, "checkpoint-1")
        .unwrap();
    assert_eq!(link_target(&root.join("link")), "one.txt");
    assert_eq!(fs::read(root.join("one.txt")).unwrap(), b"one");
    assert_eq!(fs::read(root.join("two.txt")).unwrap(), b"two");

    // Replacing a regular file with a link and undoing writes nothing through it.
    fs::remove_file(root.join("one.txt")).unwrap();
    symlink("two.txt", root.join("one.txt")).unwrap();
    checkpointed(workspace.checkpoint(&engine, "checkpoint-2", 2).unwrap());
    workspace
        .undo(&engine, "undo-2", 3, "checkpoint-2")
        .unwrap();
    assert_eq!(fs::read(root.join("one.txt")).unwrap(), b"one");
    assert_eq!(fs::read(root.join("two.txt")).unwrap(), b"two");
    assert!(workspace.status(&engine, 10).unwrap().changes.is_empty());
}

#[test]
fn recovery_completes_an_interrupted_symlink_replacement_from_its_marker() {
    let (directory, engine, base, change) = kinds_fixture(&[
        entry("one.txt", b"one", EntryKind::Regular),
        entry("two.txt", b"two", EntryKind::Regular),
        entry("link", b"one.txt", EntryKind::Symlink),
    ]);
    let root = directory.path().join("workspace");
    let mut workspace = Workspace::create(&engine, &root, change, &base).unwrap();
    fs::remove_file(root.join("link")).unwrap();
    symlink("two.txt", root.join("link")).unwrap();
    let current = checkpointed(workspace.checkpoint(&engine, "checkpoint-1", 0).unwrap());
    let connection = database(&root);
    connection
        .execute(
            "INSERT INTO operations(operation_id, kind, request_digest, status, expected_generation, before_revision, after_revision, checkpoint_operation_id) VALUES ('pending-undo', 'undo', ?1, 'pending', 1, ?2, ?3, 'checkpoint-1')",
            params!["0".repeat(64), current, base],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO staged_paths(operation_id, path, before_content, after_content, before_kind, after_kind) VALUES ('pending-undo', 'link', ?1, ?2, 'symlink', 'symlink')",
            params![b"two.txt".as_slice(), b"one.txt".as_slice()],
        )
        .unwrap();
    drop(connection);

    // Interrupted after removing the old link, before installing the new one.
    fs::remove_file(root.join("link")).unwrap();
    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::Complete)
            .unwrap(),
        OperationOutcome::Blocked {
            paths: vec!["link".to_owned()]
        }
    );
    let marker = recovery_temp_of_kind(&root, "pending-undo", "link", "symlink", b"one.txt");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(&marker, b"one.txt").unwrap();
    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::Complete)
            .unwrap(),
        OperationOutcome::Undone {
            revision: base,
            generation: 2
        }
    );
    assert_eq!(link_target(&root.join("link")), "one.txt");
    assert!(!marker.exists());
    assert!(workspace.status(&engine, 10).unwrap().changes.is_empty());
}
