#![cfg(unix)]

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use agentscm::Error;
use agentscm::context::ChangeId;
use agentscm::model::{EntryChange, EntryKind};
use agentscm::store::{Engine, digest};
use agentscm::workspace::{
    METADATA_DIRECTORY, OperationOutcome, RecoveryChoice, Workspace, WorkspacePathState,
};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn put(path: &str, kind: EntryKind, content: &[u8]) -> EntryChange {
    EntryChange {
        path: path.to_owned(),
        content: Some(content.to_vec()),
        kind,
    }
}

fn change(id: &str) -> ChangeId {
    ChangeId {
        repository: "task-workspace-repository".into(),
        id: id.into(),
    }
}

fn seed_pending_expansion(
    root: &Path,
    operation: &str,
    before: &[&str],
    after: Option<&[&str]>,
    entries: &[(&str, EntryKind, &[u8])],
) {
    let connection = Connection::open(root.join(METADATA_DIRECTORY).join("state.sqlite")).unwrap();
    let before = serde_json::to_vec(before).unwrap();
    let after = serde_json::to_vec(&after).unwrap();
    connection
        .execute(
            "INSERT INTO scope_expansions(
                operation_id, request_digest, status, expected_generation,
                before_roots_json, after_roots_json
             ) VALUES (?1, ?2, 'pending', 0, ?3, ?4)",
            params![operation, "0".repeat(64), before, after],
        )
        .unwrap();
    for (path, kind, content) in entries {
        connection
            .execute(
                "INSERT INTO scope_expansion_paths(
                    operation_id, path, after_content, after_kind
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![operation, path, content, kind.as_str()],
            )
            .unwrap();
    }
}

fn expansion_witness(
    root: &Path,
    operation: &str,
    path: &str,
    kind: EntryKind,
    content: &[u8],
) -> PathBuf {
    let mut identity = Sha256::new();
    identity.update(b"AGSCM-WORKSPACE-EXPANSION-WITNESS1\0");
    identity.update((path.len() as u64).to_be_bytes());
    identity.update(path.as_bytes());
    identity.update((kind.as_str().len() as u64).to_be_bytes());
    identity.update(kind.as_str().as_bytes());
    identity.update((content.len() as u64).to_be_bytes());
    identity.update(content);
    root.join(METADATA_DIRECTORY)
        .join("expansion-witnesses")
        .join(digest(operation.as_bytes()))
        .join(format!("{:x}", identity.finalize()))
}

#[test]
fn scoped_checkpoint_preserves_unmaterialized_base_and_ignores_outside_local_files() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("task/input.txt", EntryKind::Regular, b"before"),
                put("other/keep.txt", EntryKind::Regular, b"preserve"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let mut workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("preservation"),
        &base,
        &["task".into()],
    )
    .unwrap();

    assert!(!root.join("other/keep.txt").exists());
    assert_eq!(
        workspace.path_state(&engine, "other/keep.txt").unwrap(),
        WorkspacePathState::Unmaterialized
    );
    fs::create_dir_all(root.join("other")).unwrap();
    fs::write(root.join("other/local.txt"), b"local occupant").unwrap();
    assert_eq!(
        workspace.path_state(&engine, "other/local.txt").unwrap(),
        WorkspacePathState::OutsideScopeLocal
    );
    fs::write(root.join("task/input.txt"), b"after").unwrap();

    let revision = match workspace.checkpoint(&engine, "checkpoint-task", 0).unwrap() {
        OperationOutcome::Checkpointed {
            revision,
            generation: 1,
        } => revision,
        outcome => panic!("unexpected checkpoint outcome: {outcome:?}"),
    };
    assert_eq!(
        engine.read_file(&revision, "task/input.txt").unwrap(),
        b"after"
    );
    assert_eq!(
        engine.read_file(&revision, "other/keep.txt").unwrap(),
        b"preserve"
    );
    assert!(
        engine
            .stat_file(&revision, "other/local.txt")
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        workspace.expand(
            &engine,
            "checkpoint-task",
            1,
            &["other/keep.txt".into()]
        ),
        Err(Error::Conflict(message)) if message.contains("another operation")
    ));
}

#[test]
fn expansion_is_idempotent_across_reopen_blocks_occupants_and_preserves_kinds() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("task/a.txt", EntryKind::Regular, b"a"),
                put("tools/run.sh", EntryKind::Executable, b"#!/bin/sh\n"),
                put("tools/run-link", EntryKind::Symlink, b"run.sh"),
                put("later.txt", EntryKind::Regular, b"later"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let mut workspace =
        Workspace::create_scoped(&engine, &root, change("expansion"), &base, &["task".into()])
            .unwrap();

    let expanded = OperationOutcome::Expanded { generation: 1 };
    assert_eq!(
        workspace
            .expand(&engine, "expand-tools", 0, &["tools".into()])
            .unwrap(),
        expanded
    );
    drop(workspace);
    let mut workspace = Workspace::open(&root).unwrap();
    assert_eq!(
        workspace
            .expand(&engine, "expand-tools", 0, &["tools".into()])
            .unwrap(),
        expanded
    );
    assert!(matches!(
        workspace.expand(&engine, "expand-tools", 0, &["later.txt".into()]),
        Err(Error::Conflict(message)) if message.contains("reused with different input")
    ));
    assert_ne!(
        fs::metadata(root.join("tools/run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100,
        0
    );
    assert_eq!(
        fs::read_link(root.join("tools/run-link")).unwrap(),
        std::path::PathBuf::from("run.sh")
    );

    fs::write(root.join("later.txt"), b"later").unwrap();
    assert_eq!(
        workspace
            .expand(&engine, "expand-later-blocked", 1, &["later.txt".into()])
            .unwrap(),
        OperationOutcome::Blocked {
            paths: vec!["later.txt".into()]
        }
    );
    assert_eq!(workspace.state().unwrap().generation, 1);
    fs::remove_file(root.join("later.txt")).unwrap();
    assert_eq!(
        workspace
            .expand(&engine, "expand-later", 1, &["later.txt".into()])
            .unwrap(),
        OperationOutcome::Expanded { generation: 2 }
    );
}

#[test]
fn pending_expansion_can_roll_back_after_reopen_without_changing_scope() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("task/a.txt", EntryKind::Regular, b"a"),
                put("later.txt", EntryKind::Regular, b"later"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let workspace =
        Workspace::create_scoped(&engine, &root, change("rollback"), &base, &["task".into()])
            .unwrap();
    drop(workspace);

    seed_pending_expansion(
        &root,
        "expand-interrupted",
        &["task"],
        Some(&["later.txt", "task"]),
        &[("later.txt", EntryKind::Regular, b"later")],
    );

    let mut workspace = Workspace::open(&root).unwrap();
    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::RollBack)
            .unwrap(),
        OperationOutcome::Cancelled { generation: 0 }
    );
    assert_eq!(workspace.state().unwrap().scope, Some(vec!["task".into()]));
    assert!(!root.join("later.txt").exists());
    assert_eq!(
        workspace.path_state(&engine, "later.txt").unwrap(),
        WorkspacePathState::Unmaterialized
    );
}

#[test]
fn identical_local_occupant_never_becomes_expansion_installation_evidence() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("task/a.txt", EntryKind::Regular, b"a"),
                put("later.txt", EntryKind::Regular, b"same"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let workspace =
        Workspace::create_scoped(&engine, &root, change("occupant"), &base, &["task".into()])
            .unwrap();
    drop(workspace);
    seed_pending_expansion(
        &root,
        "expand-identical",
        &["task"],
        Some(&["later.txt", "task"]),
        &[("later.txt", EntryKind::Regular, b"same")],
    );
    fs::write(root.join("later.txt"), b"same").unwrap();

    let mut workspace = Workspace::open(&root).unwrap();
    for choice in [RecoveryChoice::Complete, RecoveryChoice::RollBack] {
        assert_eq!(
            workspace.recover(&engine, choice).unwrap(),
            OperationOutcome::Blocked {
                paths: vec!["later.txt".into()]
            }
        );
        assert_eq!(fs::read(root.join("later.txt")).unwrap(), b"same");
    }
}

#[test]
fn expansion_rollback_preserves_preexisting_empty_ancestors() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("task/a.txt", EntryKind::Regular, b"a"),
                put("empty/child/file.txt", EntryKind::Regular, b"base"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("empty-ancestors"),
        &base,
        &["task".into()],
    )
    .unwrap();
    drop(workspace);
    seed_pending_expansion(
        &root,
        "expand-empty",
        &["task"],
        Some(&["empty", "task"]),
        &[("empty/child/file.txt", EntryKind::Regular, b"base")],
    );
    fs::create_dir_all(root.join("empty/child")).unwrap();

    let mut workspace = Workspace::open(&root).unwrap();
    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::RollBack)
            .unwrap(),
        OperationOutcome::Cancelled { generation: 0 }
    );
    assert!(root.join("empty/child").is_dir());
}

#[test]
fn hard_link_witness_recovers_partial_regular_executable_and_symlink_installation() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let entries = [
        ("more/file.txt", EntryKind::Regular, &b"file"[..]),
        ("more/run.sh", EntryKind::Executable, &b"#!/bin/sh\n"[..]),
        ("more/link", EntryKind::Symlink, &b"file.txt"[..]),
    ];
    let mut base_entries = vec![put("task/a.txt", EntryKind::Regular, b"a")];
    base_entries.extend(
        entries
            .iter()
            .map(|(path, kind, content)| put(path, *kind, content)),
    );
    let base = engine.checkpoint_entries(None, &base_entries).unwrap();
    let root = fixture.path().join("workspace");
    let workspace =
        Workspace::create_scoped(&engine, &root, change("witness"), &base, &["task".into()])
            .unwrap();
    drop(workspace);
    seed_pending_expansion(
        &root,
        "expand-witnessed",
        &["task"],
        Some(&["more", "task"]),
        &entries,
    );

    for (path, kind, content) in entries {
        let witness = expansion_witness(&root, "expand-witnessed", path, kind, content);
        fs::create_dir_all(witness.parent().unwrap()).unwrap();
        if kind == EntryKind::Symlink {
            symlink(std::ffi::OsStr::from_bytes(content), &witness).unwrap();
        } else {
            fs::write(&witness, content).unwrap();
            if kind == EntryKind::Executable {
                let mut permissions = fs::metadata(&witness).unwrap().permissions();
                permissions.set_mode(0o700);
                fs::set_permissions(&witness, permissions).unwrap();
            }
        }
        let target = root.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::hard_link(&witness, &target).unwrap();
        let witness_metadata = fs::symlink_metadata(&witness).unwrap();
        let target_metadata = fs::symlink_metadata(&target).unwrap();
        assert_eq!(witness_metadata.dev(), target_metadata.dev());
        assert_eq!(witness_metadata.ino(), target_metadata.ino());
        assert_eq!(
            witness_metadata.file_type().is_symlink(),
            target_metadata.file_type().is_symlink()
        );
        assert_eq!(
            witness_metadata.file_type().is_file(),
            target_metadata.file_type().is_file()
        );
    }

    let mut workspace = Workspace::open(&root).unwrap();
    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::Complete)
            .unwrap(),
        OperationOutcome::Expanded { generation: 1 }
    );
    assert_eq!(fs::read(root.join("more/file.txt")).unwrap(), b"file");
    assert_ne!(
        fs::metadata(root.join("more/run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100,
        0
    );
    assert_eq!(
        fs::read_link(root.join("more/link")).unwrap(),
        PathBuf::from("file.txt")
    );
}

#[test]
fn witnessed_partial_full_fallback_can_roll_back_without_pruning_directories() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("task/a.txt", EntryKind::Regular, b"a"),
                put("outside/deep/file.txt", EntryKind::Regular, b"outside"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("full-rollback"),
        &base,
        &["task".into()],
    )
    .unwrap();
    drop(workspace);
    let operation = "expand-full-interrupted";
    let path = "outside/deep/file.txt";
    seed_pending_expansion(
        &root,
        operation,
        &["task"],
        None,
        &[(path, EntryKind::Regular, b"outside")],
    );
    let witness = expansion_witness(&root, operation, path, EntryKind::Regular, b"outside");
    fs::create_dir_all(witness.parent().unwrap()).unwrap();
    fs::write(&witness, b"outside").unwrap();
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::hard_link(&witness, &target).unwrap();

    let mut workspace = Workspace::open(&root).unwrap();
    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::RollBack)
            .unwrap(),
        OperationOutcome::Cancelled { generation: 0 }
    );
    assert!(!target.exists());
    assert!(root.join("outside/deep").is_dir());
    assert_eq!(workspace.state().unwrap().scope, Some(vec!["task".into()]));
}

#[test]
fn apply_requires_explicit_scope_for_new_target_selection_dependency() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(None, &[put("task/file.txt", EntryKind::Regular, b"base")])
        .unwrap();
    let target = engine
        .checkpoint_entries(
            Some(&base),
            &[put(".gitignore", EntryKind::Regular, b"task/file.txt\n")],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let mut workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("target-ignore"),
        &base,
        &["task/file.txt".into()],
    )
    .unwrap();
    let mut journal = agentscm::merge::MergeJournal::open(
        fixture.path().join("merge"),
        "task-workspace-repository",
    )
    .unwrap();
    let (session, generation) = match journal
        .begin(&engine, "begin-target", &base, &target)
        .unwrap()
    {
        agentscm::merge::MergeOutcome::Prepared {
            session,
            generation,
            ..
        } => (session, generation),
        other => panic!("unexpected merge preparation: {other:?}"),
    };
    let generation = match journal
        .complete(&engine, &session, "complete-target", generation)
        .unwrap()
    {
        agentscm::merge::MergeOutcome::Completed { generation, .. } => generation,
        other => panic!("unexpected merge completion: {other:?}"),
    };
    let completion = journal.completion(&session, generation).unwrap();
    let target = completion.revision().to_owned();
    assert!(matches!(
        workspace.apply(&engine, "apply-target", 0, &completion),
        Err(Error::Conflict(message)) if message.contains("selection dependencies")
    ));
    assert_eq!(
        workspace
            .expand(&engine, "expand-target-ignore", 0, &[".gitignore".into()])
            .unwrap(),
        OperationOutcome::Expanded { generation: 1 }
    );
    assert_eq!(
        workspace
            .apply(&engine, "apply-target", 1, &completion)
            .unwrap(),
        OperationOutcome::Applied {
            revision: target,
            generation: 2
        }
    );
    assert_eq!(
        fs::read(root.join(".gitignore")).unwrap(),
        b"task/file.txt\n"
    );
}

#[test]
fn oversized_persisted_scope_metadata_is_rejected_before_json_decode() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(None, &[put("task/a.txt", EntryKind::Regular, b"a")])
        .unwrap();
    let root = fixture.path().join("workspace");
    let workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("bounded-scope"),
        &base,
        &["task".into()],
    )
    .unwrap();
    drop(workspace);

    let connection = Connection::open(root.join(METADATA_DIRECTORY).join("state.sqlite")).unwrap();
    connection
        .execute(
            "UPDATE workspace_scope SET roots_json = zeroblob(131073) WHERE singleton = 1",
            [],
        )
        .unwrap();
    drop(connection);

    let workspace = Workspace::open(&root).unwrap();
    assert!(matches!(workspace.state(), Err(Error::Limit(_))));
}

#[test]
fn completed_expansion_cleanup_preserves_edits_after_lost_acknowledgement() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("task/a", EntryKind::Regular, b"a"),
                put("later.txt", EntryKind::Regular, b"later"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let mut workspace =
        Workspace::create_scoped(&engine, &root, change("cleanup"), &base, &["task".into()])
            .unwrap();
    let expected = workspace
        .expand(&engine, "expand", 0, &["later.txt".into()])
        .unwrap();
    // Model the terminal scope transaction surviving a crash before metadata cleanup.
    let witness = expansion_witness(&root, "expand", "later.txt", EntryKind::Regular, b"later");
    fs::create_dir_all(witness.parent().unwrap()).unwrap();
    fs::hard_link(root.join("later.txt"), &witness).unwrap();
    let db = Connection::open(root.join(METADATA_DIRECTORY).join("state.sqlite")).unwrap();
    db.execute("INSERT INTO scope_expansion_paths(operation_id,path,after_content,after_kind) VALUES ('expand','later.txt',?1,'regular')", [&b"later"[..]]).unwrap();
    drop(db);
    fs::write(root.join("later.txt"), b"first user edit").unwrap();
    // An editor may subsequently replace its target atomically, leaving a dirty old witness.
    fs::write(root.join("replacement"), b"latest user edit").unwrap();
    fs::rename(root.join("replacement"), root.join("later.txt")).unwrap();
    assert_eq!(
        workspace
            .expand(&engine, "expand", 0, &["later.txt".into()])
            .unwrap(),
        expected
    );
    assert_eq!(
        fs::read(root.join("later.txt")).unwrap(),
        b"latest user edit"
    );
    assert!(!witness.exists());
}

#[test]
fn retained_expansion_cannot_shrink_the_projection() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("task/a", EntryKind::Regular, b"a"),
                put("later", EntryKind::Regular, b"later"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let mut workspace =
        Workspace::create_scoped(&engine, &root, change("shrink"), &base, &["task".into()])
            .unwrap();
    seed_pending_expansion(
        &root,
        "bad-expansion",
        &["task"],
        Some(&["later"]),
        &[("later", EntryKind::Regular, b"later")],
    );
    assert!(
        matches!(workspace.recover(&engine, RecoveryChoice::Complete), Err(Error::Invalid(message)) if message.contains("does not extend"))
    );
    assert_eq!(workspace.state().unwrap().scope, Some(vec!["task".into()]));
    assert_eq!(fs::read(root.join("task/a")).unwrap(), b"a");
    assert!(!root.join("later").exists());
}
