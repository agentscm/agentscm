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

#[test]
fn scoped_start_and_expansion_retain_applicable_agent_instructions() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("AGENTS.md", EntryKind::Regular, b"root instructions"),
                put("AGENTS.override.md", EntryKind::Regular, b"higher priority"),
                put(
                    ".cursor/rules/agentscm.mdc",
                    EntryKind::Regular,
                    b"cursor instructions",
                ),
                put(
                    ".github/copilot-instructions.md",
                    EntryKind::Regular,
                    b"copilot instructions",
                ),
                put("CLAUDE.md", EntryKind::Symlink, b"AGENTS.md"),
                put("src/AGENTS.md", EntryKind::Regular, b"source instructions"),
                put("src/task/input.txt", EntryKind::Regular, b"before"),
                put(
                    "deps/GEMINI.md",
                    EntryKind::Regular,
                    b"dependency instructions",
                ),
                put("deps/value.txt", EntryKind::Regular, b"dependency"),
                put(
                    "unseen/AGENTS.md",
                    EntryKind::Regular,
                    b"unrelated instructions",
                ),
                put("unseen/keep.txt", EntryKind::Regular, b"preserve"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let mut workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("agent-instructions"),
        &base,
        &["src/task/input.txt".into()],
    )
    .unwrap();
    assert_eq!(
        fs::read(root.join("AGENTS.md")).unwrap(),
        b"root instructions"
    );
    assert_eq!(
        fs::read_link(root.join("CLAUDE.md")).unwrap(),
        Path::new("AGENTS.md")
    );
    assert!(root.join("src/AGENTS.md").is_file());
    assert_eq!(
        fs::read(root.join("AGENTS.override.md")).unwrap(),
        b"higher priority"
    );
    assert!(root.join(".cursor/rules/agentscm.mdc").is_file());
    assert!(root.join(".github/copilot-instructions.md").is_file());
    assert!(!root.join("deps/GEMINI.md").exists());
    assert!(!root.join("unseen/AGENTS.md").exists());
    workspace
        .expand(&engine, "add-dependency", 0, &["deps/value.txt".into()])
        .unwrap();
    assert_eq!(
        fs::read(root.join("deps/GEMINI.md")).unwrap(),
        b"dependency instructions"
    );
    let scope = workspace.state().unwrap().scope.unwrap();
    for path in ["AGENTS.md", "CLAUDE.md", "src/AGENTS.md", "deps/GEMINI.md"] {
        assert!(scope.iter().any(|root| root == path), "{scope:?}");
    }
    fs::write(root.join("src/task/input.txt"), b"after").unwrap();
    let revision = match workspace.checkpoint(&engine, "save", 1).unwrap() {
        OperationOutcome::Checkpointed { revision, .. } => revision,
        other => panic!("unexpected checkpoint: {other:?}"),
    };
    assert_eq!(
        engine.read_file(&revision, "unseen/keep.txt").unwrap(),
        b"preserve"
    );
    assert_eq!(
        engine.read_file(&revision, "unseen/AGENTS.md").unwrap(),
        b"unrelated instructions"
    );
}

#[test]
fn instruction_links_require_selected_targets_and_reject_cycles_before_creation() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("CLAUDE.md", EntryKind::Symlink, b"policy/link"),
                put("policy/link", EntryKind::Symlink, b"claude.md"),
                put(
                    "policy/claude.md",
                    EntryKind::Regular,
                    b"actual instructions",
                ),
                put("task/input.txt", EntryKind::Regular, b"before"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    assert!(
        matches!(Workspace::create_scoped(&engine, &root, change("links"), &base,
        &["task".into()]), Err(Error::Conflict(message)) if message.contains("policy/link"))
    );
    assert!(!root.exists());
    let workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("links"),
        &base,
        &["task".into(), "policy".into()],
    )
    .unwrap();
    assert_eq!(
        fs::read(root.join("CLAUDE.md")).unwrap(),
        b"actual instructions"
    );
    let target = engine
        .checkpoint_entries(
            Some(&base),
            &[put("policy/claude.md", EntryKind::Symlink, b"link")],
        )
        .unwrap();
    let other = fixture.path().join("cycle");
    assert!(
        matches!(Workspace::create_scoped(&engine, &other, change("cycle"), &target,
        &["task".into(), "policy".into()]), Err(Error::Invalid(message)) if message.contains("cycle"))
    );
    assert!(!other.exists());
    // Expansion uses the same qualification and preserves the old state on failure.
    let clean = engine
        .checkpoint_entries(
            None,
            &[
                put("task/input.txt", EntryKind::Regular, b"before"),
                put("deps/CLAUDE.md", EntryKind::Symlink, b"../policy/claude.md"),
                put("deps/value.txt", EntryKind::Regular, b"dependency"),
                put("policy/claude.md", EntryKind::Regular, b"instructions"),
            ],
        )
        .unwrap();
    let expand_root = fixture.path().join("expand");
    drop(workspace);
    let mut workspace = Workspace::create_scoped(
        &engine,
        &expand_root,
        change("expand-links"),
        &clean,
        &["task".into()],
    )
    .unwrap();
    assert!(
        matches!(workspace.expand(&engine, "add-deps", 0, &["deps/value.txt".into()]),
        Err(Error::Conflict(message)) if message.contains("policy/claude.md"))
    );
    assert_eq!(workspace.state().unwrap().generation, 0);
    assert!(!expand_root.join("deps").exists());
    workspace
        .expand(
            &engine,
            "add-deps-with-policy",
            0,
            &["deps/value.txt".into(), "policy/claude.md".into()],
        )
        .unwrap();
    assert_eq!(
        fs::read(expand_root.join("deps/CLAUDE.md")).unwrap(),
        b"instructions"
    );
}

#[test]
fn scoped_checkpoint_with_deleted_instruction_target_can_be_inspected_and_repaired() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("CLAUDE.md", EntryKind::Symlink, b"policy/claude.md"),
                put("policy/claude.md", EntryKind::Regular, b"instructions"),
                put("task/input.txt", EntryKind::Regular, b"input"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let mut workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("repair-instruction-link"),
        &base,
        &["policy".into(), "task".into()],
    )
    .unwrap();

    fs::remove_file(root.join("policy/claude.md")).unwrap();
    let broken = match workspace
        .checkpoint(&engine, "delete-instruction-target", 0)
        .unwrap()
    {
        OperationOutcome::Checkpointed {
            revision,
            generation: 1,
        } => revision,
        other => panic!("unexpected checkpoint: {other:?}"),
    };
    assert!(
        engine
            .stat_file(&broken, "policy/claude.md")
            .unwrap()
            .is_none()
    );
    assert_eq!(workspace.status(&engine, 32).unwrap().generation, 1);
    assert_eq!(workspace.preview(&engine, 32).unwrap().generation, 1);

    fs::write(root.join("policy/claude.md"), b"repaired instructions").unwrap();
    let repaired = match workspace
        .checkpoint(&engine, "restore-instruction-target", 1)
        .unwrap()
    {
        OperationOutcome::Checkpointed {
            revision,
            generation: 2,
        } => revision,
        other => panic!("unexpected repair checkpoint: {other:?}"),
    };
    assert_eq!(
        engine.read_file(&repaired, "policy/claude.md").unwrap(),
        b"repaired instructions"
    );
}

#[test]
fn legacy_initialization_with_unselected_instruction_target_can_complete() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("CLAUDE.md", EntryKind::Symlink, b"policy/claude.md"),
                put("policy/claude.md", EntryKind::Regular, b"instructions"),
                put("task/input.txt", EntryKind::Regular, b"input"),
            ],
        )
        .unwrap();
    let legacy = engine
        .checkpoint_entries(
            Some(&base),
            &[EntryChange {
                path: "policy/claude.md".into(),
                kind: EntryKind::Regular,
                content: None,
            }],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("legacy-initialization"),
        &base,
        &["policy".into(), "task".into()],
    )
    .unwrap();
    drop(workspace);

    fs::remove_file(root.join("CLAUDE.md")).unwrap();
    fs::remove_file(root.join("policy/claude.md")).unwrap();
    fs::remove_file(root.join("task/input.txt")).unwrap();
    let connection = Connection::open(root.join(METADATA_DIRECTORY).join("state.sqlite")).unwrap();
    connection
        .execute(
            "UPDATE workspace_state SET lifecycle = 'initializing', base_revision = ?1 WHERE singleton = 1",
            [&legacy],
        )
        .unwrap();
    drop(connection);

    let mut workspace = Workspace::open(&root).unwrap();
    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::Complete)
            .unwrap(),
        OperationOutcome::Recovered { generation: 0 }
    );
    assert!(workspace.state().unwrap().ready);
    assert_eq!(
        fs::read_link(root.join("CLAUDE.md")).unwrap(),
        Path::new("policy/claude.md")
    );
    assert!(!root.join("policy/claude.md").exists());
    assert_eq!(fs::read(root.join("task/input.txt")).unwrap(), b"input");
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

fn seed_pending_apply(
    root: &Path,
    operation: &str,
    base: &str,
    target: &str,
    session: &str,
    merge_generation: u64,
) {
    let mut request = b"AGSCM-WORKSPACE-APPLY1\0".to_vec();
    push_digest_field(&mut request, operation);
    request.extend_from_slice(&0_u64.to_be_bytes());
    push_digest_field(&mut request, "task-workspace-repository");
    push_digest_field(&mut request, session);
    request.extend_from_slice(&merge_generation.to_be_bytes());
    push_digest_field(&mut request, base);
    push_digest_field(&mut request, target);
    let connection = Connection::open(root.join(METADATA_DIRECTORY).join("state.sqlite")).unwrap();
    connection
        .execute(
            "INSERT INTO operations(
                operation_id, kind, request_digest, status, expected_generation,
                before_revision, after_revision, merge_repository, merge_session,
                merge_generation, merge_left, merge_revision
             ) VALUES (?1, 'apply', ?2, 'pending', 0, ?3, ?4, ?5, ?6, ?7, ?3, ?4)",
            params![
                operation,
                digest(&request),
                base,
                target,
                "task-workspace-repository",
                session,
                merge_generation,
            ],
        )
        .unwrap();
}

fn push_digest_field(output: &mut Vec<u8>, value: &str) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value.as_bytes());
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
fn legacy_pending_expansion_revalidates_instruction_links_only_on_complete() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                put("task/input.txt", EntryKind::Regular, b"input"),
                put("deps/CLAUDE.md", EntryKind::Symlink, b"../policy/claude.md"),
                put("policy/claude.md", EntryKind::Regular, b"instructions"),
            ],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("legacy-expand"),
        &base,
        &["task".into()],
    )
    .unwrap();
    drop(workspace);
    seed_pending_expansion(
        &root,
        "legacy-expand",
        &["task"],
        Some(&["deps", "task"]),
        &[("deps/CLAUDE.md", EntryKind::Symlink, b"../policy/claude.md")],
    );

    let mut workspace = Workspace::open(&root).unwrap();
    assert!(matches!(
        workspace.recover(&engine, RecoveryChoice::Complete),
        Err(Error::Conflict(message)) if message.contains("policy/claude.md")
    ));
    assert_eq!(workspace.state().unwrap().scope, Some(vec!["task".into()]));
    assert!(!root.join("deps/CLAUDE.md").exists());
    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::RollBack)
            .unwrap(),
        OperationOutcome::Cancelled { generation: 0 }
    );
    assert_eq!(workspace.state().unwrap().scope, Some(vec!["task".into()]));
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
    for dependency in [".gitignore", "AGENTS.md", "task/CLAUDE.md"] {
        let fixture = TempDir::new().unwrap();
        let engine = Engine::open(fixture.path().join("store")).unwrap();
        let base = engine
            .checkpoint_entries(None, &[put("task/file.txt", EntryKind::Regular, b"base")])
            .unwrap();
        let target = engine
            .checkpoint_entries(
                Some(&base),
                &[put(dependency, EntryKind::Regular, b"task/file.txt\n")],
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
                .expand(&engine, "expand-target-ignore", 0, &[dependency.into()])
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
        assert_eq!(fs::read(root.join(dependency)).unwrap(), b"task/file.txt\n");
    }
}

#[test]
fn legacy_pending_apply_revalidates_target_dependencies_only_on_complete() {
    let fixture = TempDir::new().unwrap();
    let engine = Engine::open(fixture.path().join("store")).unwrap();
    let base = engine
        .checkpoint_entries(None, &[put("task/file.txt", EntryKind::Regular, b"base")])
        .unwrap();
    let target = engine
        .checkpoint_entries(
            Some(&base),
            &[put("AGENTS.md", EntryKind::Regular, b"instructions")],
        )
        .unwrap();
    let root = fixture.path().join("workspace");
    let workspace = Workspace::create_scoped(
        &engine,
        &root,
        change("legacy-apply"),
        &base,
        &["task/file.txt".into()],
    )
    .unwrap();
    drop(workspace);
    seed_pending_apply(&root, "legacy-apply", &base, &target, &"a".repeat(64), 1);

    let mut workspace = Workspace::open(&root).unwrap();
    assert!(matches!(
        workspace.recover(&engine, RecoveryChoice::Complete),
        Err(Error::Conflict(message)) if message.contains("selection dependencies")
    ));
    assert_eq!(workspace.state().unwrap().base_revision, base);
    assert!(!root.join("AGENTS.md").exists());
    assert_eq!(
        workspace
            .recover(&engine, RecoveryChoice::RollBack)
            .unwrap(),
        OperationOutcome::Cancelled { generation: 0 }
    );
    assert_eq!(workspace.state().unwrap().base_revision, base);
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
