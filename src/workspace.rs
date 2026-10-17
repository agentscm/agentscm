use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::context::ChangeId;
use crate::model::{EntryChange, EntryKind};
use crate::store::{Engine, digest, validate_id, validate_symlink_target};
use crate::sync;
use crate::{Error, Result};

pub const METADATA_DIRECTORY: &str = crate::local_recovery::WORKSPACE_METADATA_DIRECTORY;

const DATABASE_FILE: &str = "state.sqlite";
const APPLICATION_ID: i64 = 0x4153_574b;
const SCHEMA_VERSION: i64 = 3;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const LIST_PAGE_SIZE: usize = 4_096;
const MAX_ENTRIES: usize = 100_000;
const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 128 * 1024 * 1024;
const MAX_DEPTH: usize = 128;
const MAX_CHECKPOINT_CHANGES: usize = 4_096;
const MAX_OPERATION_ID_BYTES: usize = 128;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkspaceState {
    pub change: ChangeId,
    pub base_revision: String,
    pub generation: u64,
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingOperation>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PendingOperation {
    pub operation_id: String,
    pub kind: OperationKind,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Checkpoint,
    Undo,
    Apply,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Status {
    pub base_revision: String,
    pub generation: u64,
    pub changes: Vec<WorkspaceChange>,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingOperation>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceChange {
    Added {
        path: String,
    },
    Modified {
        path: String,
    },
    Deleted {
        path: String,
    },
    /// Same path on both sides with a different entry kind: a mode-only change,
    /// or a regular file replaced by a symbolic link or the reverse.
    KindChanged {
        path: String,
        before: EntryKind,
        after: EntryKind,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryChoice {
    Complete,
    RollBack,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OperationOutcome {
    Checkpointed { revision: String, generation: u64 },
    Undone { revision: String, generation: u64 },
    Applied { revision: String, generation: u64 },
    Recovered { generation: u64 },
    Cancelled { generation: u64 },
    Blocked { paths: Vec<String> },
}

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    _access: crate::local_recovery::AccessGuard,
}

impl Workspace {
    pub fn create(
        engine: &Engine,
        root: impl AsRef<Path>,
        change: ChangeId,
        revision: &str,
    ) -> Result<Self> {
        change.validate()?;
        validate_id(revision)?;
        let root = absolute_path(root.as_ref())?;
        crate::local_recovery::check_before_create(&root.join(METADATA_DIRECTORY), true)?;
        let parent = root
            .parent()
            .ok_or_else(|| Error::Invalid("workspace root has no parent".to_owned()))?;
        require_directory(parent, "workspace parent")?;
        let snapshot = require_complete_snapshot(source_snapshot(engine, revision)?)?;
        fs::create_dir(&root).map_err(|error| {
            if error.kind() == ErrorKind::AlreadyExists {
                Error::Conflict(format!(
                    "workspace destination already exists: {}",
                    root.display()
                ))
            } else {
                Error::Io(error)
            }
        })?;
        sync_directory(parent)?;

        let metadata = root.join(METADATA_DIRECTORY);
        fs::create_dir(&metadata)?;
        // Probe inside the new root so the filesystem under test is the one
        // that will hold the tree; undo what create made if it is refused.
        if let Err(error) = require_filesystem_support(&metadata) {
            let _ = fs::remove_dir(&metadata);
            let _ = fs::remove_dir(&root);
            let _ = sync_directory(parent);
            return Err(error);
        }
        let access = crate::local_recovery::AccessGuard::workspace(&metadata)?;
        sync_directory(&root)?;
        let mut connection = open_connection(&metadata.join(DATABASE_FILE), true)?;
        initialize_schema(&mut connection)?;
        connection.execute(
            "INSERT INTO workspace_state(singleton, lifecycle, repository, change_id, base_revision, generation) VALUES (1, 'initializing', ?1, ?2, ?3, 0)",
            params![change.repository, change.id, revision],
        )?;
        drop(connection);

        let workspace = Self {
            root,
            _access: access,
        };
        workspace.finish_initialization_snapshot(snapshot)?;
        Ok(workspace)
    }

    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute_path(root.as_ref())?;
        require_directory(&root, "workspace root")?;
        require_directory(&root.join(METADATA_DIRECTORY), "workspace metadata")?;
        let access = crate::local_recovery::AccessGuard::workspace(&root.join(METADATA_DIRECTORY))?;
        let mut connection =
            open_connection(&root.join(METADATA_DIRECTORY).join(DATABASE_FILE), false)?;
        migrate_schema(&mut connection)?;
        validate_schema(&connection)?;
        read_state(&connection)?;
        Ok(Self {
            root,
            _access: access,
        })
    }

    pub fn state(&self) -> Result<WorkspaceState> {
        let connection = self.connection()?;
        state_with_pending(&connection)
    }

    pub fn status(&self, engine: &Engine, limit: usize) -> Result<Status> {
        if limit == 0 {
            return Err(Error::Limit(
                "workspace status limit must be positive".to_owned(),
            ));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = state_with_pending(&tx)?;
        let status = if !state.ready {
            Status {
                base_revision: state.base_revision,
                generation: state.generation,
                changes: Vec::new(),
                complete: false,
                pending: state.pending,
            }
        } else if let Some(base) = source_snapshot(engine, &state.base_revision)? {
            if let Some(working) = scan_workspace(&self.root)? {
                let mut changes = changes_between(&base.files, &working.files);
                let complete = changes.len() <= limit;
                changes.truncate(limit);
                Status {
                    base_revision: state.base_revision,
                    generation: state.generation,
                    changes,
                    complete,
                    pending: state.pending,
                }
            } else {
                Status {
                    base_revision: state.base_revision,
                    generation: state.generation,
                    changes: Vec::new(),
                    complete: false,
                    pending: state.pending,
                }
            }
        } else {
            Status {
                base_revision: state.base_revision,
                generation: state.generation,
                changes: Vec::new(),
                complete: false,
                pending: state.pending,
            }
        };
        tx.commit()?;
        Ok(status)
    }

    pub fn checkpoint(
        &mut self,
        engine: &Engine,
        operation_id: &str,
        expected_generation: u64,
    ) -> Result<OperationOutcome> {
        validate_operation_id(operation_id)?;
        let request_digest =
            operation_digest("checkpoint", operation_id, expected_generation, None);
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = matching_operation(&tx, operation_id, &request_digest)? {
            let pending = existing.is_none();
            drop(tx);
            return if pending {
                self.finish_checkpoint(engine, operation_id)
            } else {
                Ok(existing.expect("completed outcome"))
            };
        }
        ensure_no_pending(&tx)?;
        let state = read_state(&tx)?;
        require_ready_generation(&state, expected_generation)?;
        let base = require_complete_snapshot(source_snapshot(engine, &state.base_revision)?)?;
        let working = require_complete_snapshot(scan_workspace(&self.root)?)?;
        let staged = staged_between(&base.files, &working.files);
        if staged.is_empty() {
            return Err(Error::Invalid(
                "workspace has no changes to checkpoint".to_owned(),
            ));
        }
        if staged.len() > MAX_CHECKPOINT_CHANGES {
            return Err(Error::Limit(format!(
                "workspace checkpoint has more than {MAX_CHECKPOINT_CHANGES} changes"
            )));
        }
        insert_operation(
            &tx,
            operation_id,
            OperationKind::Checkpoint,
            &request_digest,
            expected_generation,
            &state.base_revision,
            None,
        )?;
        insert_staged(&tx, operation_id, &staged)?;
        tx.commit()?;
        self.finish_checkpoint(engine, operation_id)
    }

    pub fn undo(
        &mut self,
        engine: &Engine,
        operation_id: &str,
        expected_generation: u64,
        checkpoint_operation_id: &str,
    ) -> Result<OperationOutcome> {
        validate_operation_id(operation_id)?;
        validate_operation_id(checkpoint_operation_id)?;
        let request_digest = operation_digest(
            "undo",
            operation_id,
            expected_generation,
            Some(checkpoint_operation_id),
        );
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = matching_operation(&tx, operation_id, &request_digest)? {
            return match existing {
                Some(outcome) => Ok(outcome),
                None => finish_undo(&self.root, tx, operation_id, RecoveryChoice::Complete),
            };
        }
        ensure_no_pending(&tx)?;
        let state = read_state(&tx)?;
        require_ready_generation(&state, expected_generation)?;
        let (before_revision, after_revision) = completed_checkpoint(&tx, checkpoint_operation_id)?;
        if after_revision != state.base_revision {
            return Err(Error::Conflict(format!(
                "checkpoint operation {checkpoint_operation_id} is not the current workspace base"
            )));
        }
        let current = require_complete_snapshot(source_snapshot(engine, &after_revision)?)?;
        let target = require_complete_snapshot(source_snapshot(engine, &before_revision)?)?;
        let working = require_complete_snapshot(scan_workspace(&self.root)?)?;
        let staged = staged_between(&current.files, &target.files);
        let blocked = blocked_paths(&working, &staged, &self.root, operation_id, true)?;
        if !blocked.is_empty() {
            return Ok(OperationOutcome::Blocked { paths: blocked });
        }
        require_staged_support(&self.root, &staged)?;
        insert_operation(
            &tx,
            operation_id,
            OperationKind::Undo,
            &request_digest,
            expected_generation,
            &after_revision,
            Some(checkpoint_operation_id),
        )?;
        insert_staged(&tx, operation_id, &staged)?;
        tx.execute(
            "UPDATE operations SET after_revision = ?1 WHERE operation_id = ?2",
            params![before_revision, operation_id],
        )?;
        tx.commit()?;

        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        finish_undo(&self.root, tx, operation_id, RecoveryChoice::Complete)
    }

    pub fn apply(
        &mut self,
        engine: &Engine,
        operation_id: &str,
        expected_generation: u64,
        completion: &crate::merge::MergeCompletion,
    ) -> Result<OperationOutcome> {
        validate_operation_id(operation_id)?;
        validate_merge_completion(completion)?;
        let request_digest = apply_operation_digest(operation_id, expected_generation, completion);
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = matching_operation(&tx, operation_id, &request_digest)? {
            let pending = existing.is_none();
            drop(tx);
            return if pending {
                self.finish_apply(engine, operation_id, RecoveryChoice::Complete)
            } else {
                Ok(existing.expect("completed outcome"))
            };
        }
        ensure_no_pending(&tx)?;
        let state = read_state(&tx)?;
        require_ready_generation(&state, expected_generation)?;
        validate_completion_binding(&state, completion)?;
        next_generation(state.generation)?;
        qualify_merge_candidate(engine, completion)?;

        let current = require_complete_snapshot(source_snapshot(engine, &state.base_revision)?)?;
        let target = require_complete_snapshot(source_snapshot(engine, &completion.revision)?)?;
        let working = require_complete_snapshot(scan_workspace(&self.root)?)?;
        let staged = staged_between(&current.files, &target.files);
        let blocked = blocked_paths(&working, &staged, &self.root, operation_id, true)?;
        if !blocked.is_empty() {
            return Ok(OperationOutcome::Blocked { paths: blocked });
        }
        require_staged_support(&self.root, &staged)?;
        insert_apply_operation(
            &tx,
            operation_id,
            &request_digest,
            expected_generation,
            &state.base_revision,
            completion,
        )?;
        insert_staged(&tx, operation_id, &staged)?;
        tx.commit()?;
        self.finish_apply(engine, operation_id, RecoveryChoice::Complete)
    }

    pub fn recover(&mut self, engine: &Engine, choice: RecoveryChoice) -> Result<OperationOutcome> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = read_state(&tx)?;
        if !state.ready {
            if choice == RecoveryChoice::RollBack {
                return Err(Error::Invalid(
                    "an initializing workspace can only be completed".to_owned(),
                ));
            }
            drop(tx);
            self.finish_initialization(engine)?;
            return Ok(OperationOutcome::Recovered { generation: 0 });
        }
        let pending = pending_operation(&tx)?.ok_or_else(|| {
            Error::NotFound("workspace has no interrupted operation to recover".to_owned())
        })?;
        match pending.kind {
            OperationKind::Checkpoint => {
                if choice == RecoveryChoice::RollBack {
                    let outcome = OperationOutcome::Cancelled {
                        generation: state.generation,
                    };
                    complete_operation(&tx, &pending.operation_id, "cancelled", &outcome)?;
                    tx.commit()?;
                    Ok(outcome)
                } else {
                    drop(tx);
                    self.finish_checkpoint(engine, &pending.operation_id)
                }
            }
            OperationKind::Undo => finish_undo(&self.root, tx, &pending.operation_id, choice),
            OperationKind::Apply => {
                drop(tx);
                self.finish_apply(engine, &pending.operation_id, choice)
            }
        }
    }

    fn connection(&self) -> Result<Connection> {
        let path = self.root.join(METADATA_DIRECTORY).join(DATABASE_FILE);
        let mut connection = open_connection(&path, false)?;
        migrate_schema(&mut connection)?;
        validate_schema(&connection)?;
        Ok(connection)
    }

    fn finish_initialization(&self, engine: &Engine) -> Result<()> {
        let connection = self.connection()?;
        let state = read_state(&connection)?;
        if state.ready {
            return Ok(());
        }
        let snapshot = require_complete_snapshot(source_snapshot(engine, &state.base_revision)?)?;
        self.finish_initialization_snapshot(snapshot)
    }

    fn finish_initialization_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = read_state(&tx)?;
        if state.ready {
            return Ok(());
        }
        let working = require_complete_snapshot(scan_workspace(&self.root)?)?;
        let staged = snapshot
            .files
            .into_iter()
            .map(|(path, item)| {
                (
                    path,
                    Stage {
                        before: None,
                        after: Some(item),
                    },
                )
            })
            .collect();
        let blocked = blocked_paths(&working, &staged, &self.root, "initialization", true)?;
        if !blocked.is_empty() {
            return Err(Error::Conflict(format!(
                "workspace initialization is blocked by: {}",
                blocked.join(", ")
            )));
        }
        apply_staged(&self.root, "initialization", &staged, true)?;
        tx.execute(
            "UPDATE workspace_state SET lifecycle = 'ready' WHERE singleton = 1 AND lifecycle = 'initializing'",
            [],
        )?;
        tx.commit()?;
        sync_directory(&self.root)?;
        Ok(())
    }

    fn finish_checkpoint(&self, engine: &Engine, operation_id: &str) -> Result<OperationOutcome> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation = read_operation(&tx, operation_id)?
            .ok_or_else(|| Error::NotFound(format!("workspace operation {operation_id}")))?;
        if let Some(outcome) = operation.outcome.clone() {
            return Ok(outcome);
        }
        if operation.kind != OperationKind::Checkpoint {
            return Err(Error::Conflict(format!(
                "workspace operation {operation_id} is not a checkpoint"
            )));
        }
        let state = read_state(&tx)?;
        require_ready_generation(&state, operation.expected_generation)?;
        if state.base_revision != operation.before_revision {
            return Err(Error::Conflict(
                "workspace base changed while checkpoint was pending".to_owned(),
            ));
        }
        let staged = read_staged(&tx, operation_id)?;
        let changes: Vec<_> = staged
            .into_iter()
            .map(|(path, stage)| EntryChange {
                path,
                kind: stage
                    .after
                    .as_ref()
                    .map_or(EntryKind::Regular, |item| item.kind),
                content: stage.after.map(|item| item.content),
            })
            .collect();
        let revision = engine.checkpoint_entries(Some(&state.base_revision), &changes)?;
        let generation = next_generation(state.generation)?;
        let outcome = OperationOutcome::Checkpointed {
            revision: revision.clone(),
            generation,
        };
        if tx.execute(
            "UPDATE workspace_state SET base_revision = ?1, generation = ?2 WHERE singleton = 1 AND generation = ?3 AND base_revision = ?4",
            params![revision, sql_i64("workspace generation", generation)?, sql_i64("workspace generation", state.generation)?, state.base_revision],
        )? != 1
        {
            return Err(Error::Conflict(
                "workspace changed while checkpoint was completing".to_owned(),
            ));
        }
        tx.execute(
            "UPDATE operations SET after_revision = ?1 WHERE operation_id = ?2",
            params![revision, operation_id],
        )?;
        complete_operation(&tx, operation_id, "completed", &outcome)?;
        tx.commit()?;
        Ok(outcome)
    }

    fn finish_apply(
        &self,
        engine: &Engine,
        operation_id: &str,
        choice: RecoveryChoice,
    ) -> Result<OperationOutcome> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation = read_operation(&tx, operation_id)?
            .ok_or_else(|| Error::NotFound(format!("workspace operation {operation_id}")))?;
        if let Some(outcome) = operation.outcome.clone() {
            return Ok(outcome);
        }
        if operation.kind != OperationKind::Apply {
            return Err(Error::Conflict(format!(
                "workspace operation {operation_id} is not an apply"
            )));
        }
        let completion = operation.merge_completion.clone().ok_or_else(|| {
            Error::Invalid(format!(
                "workspace apply operation {operation_id} lacks a merge receipt"
            ))
        })?;
        let state = read_state(&tx)?;
        require_ready_generation(&state, operation.expected_generation)?;
        validate_stored_completion_binding(&state, operation_id, &operation, &completion)?;
        let next = if choice == RecoveryChoice::Complete {
            let generation = next_generation(state.generation)?;
            qualify_stored_merge_candidate(engine, &completion)?;
            Some(generation)
        } else {
            None
        };

        let staged = read_staged_allow_empty(&tx, operation_id)?;
        let working = require_complete_snapshot(scan_workspace(&self.root)?)?;
        let desired_after = choice == RecoveryChoice::Complete;
        let blocked = blocked_paths(&working, &staged, &self.root, operation_id, desired_after)?;
        if !blocked.is_empty() {
            return Ok(OperationOutcome::Blocked { paths: blocked });
        }
        apply_staged(&self.root, operation_id, &staged, desired_after)?;
        if choice == RecoveryChoice::RollBack {
            let outcome = OperationOutcome::Cancelled {
                generation: state.generation,
            };
            complete_operation(&tx, operation_id, "cancelled", &outcome)?;
            tx.commit()?;
            return Ok(outcome);
        }

        let generation = next.expect("complete apply reserves its next generation");
        if tx.execute(
            "UPDATE workspace_state SET base_revision = ?1, generation = ?2 WHERE singleton = 1 AND generation = ?3 AND base_revision = ?4",
            params![completion.revision, sql_i64("workspace generation", generation)?, sql_i64("workspace generation", state.generation)?, state.base_revision],
        )? != 1
        {
            return Err(Error::Conflict(
                "workspace changed while merge application was completing".to_owned(),
            ));
        }
        let outcome = OperationOutcome::Applied {
            revision: completion.revision,
            generation,
        };
        complete_operation(&tx, operation_id, "completed", &outcome)?;
        tx.commit()?;
        Ok(outcome)
    }
}

#[derive(Clone, Debug)]
struct StoredState {
    change: ChangeId,
    base_revision: String,
    generation: u64,
    ready: bool,
}

#[derive(Clone, Debug)]
struct StoredOperation {
    kind: OperationKind,
    request_digest: String,
    expected_generation: u64,
    before_revision: String,
    after_revision: Option<String>,
    merge_completion: Option<StoredMergeCompletion>,
    outcome: Option<OperationOutcome>,
}

#[derive(Clone, Debug)]
struct StoredMergeCompletion {
    repository: String,
    session: String,
    generation: u64,
    left: String,
    revision: String,
}

#[derive(Clone, Debug)]
struct Stage {
    before: Option<Item>,
    after: Option<Item>,
}

/// One tracked workspace entry. For a symlink `content` is the link target.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    kind: EntryKind,
    content: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Snapshot {
    files: BTreeMap<String, Item>,
    directories: BTreeSet<String>,
}

type OperationRow = (
    String,
    String,
    i64,
    String,
    Option<String>,
    String,
    Option<Vec<u8>>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
);

fn open_connection(path: &Path, create: bool) -> Result<Connection> {
    if !create {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(Error::Invalid(format!(
                "workspace database is not a non-symlink regular file: {}",
                path.display()
            )));
        }
    }
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
    if create {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let connection = Connection::open_with_flags(path, flags)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        let selected: String =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if !selected.eq_ignore_ascii_case("wal") {
            return Err(Error::Invalid(format!(
                "SQLite refused WAL journal mode and selected {selected}"
            )));
        }
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    let synchronous: i64 = connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    let foreign_keys: i64 =
        connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if synchronous != 2 || foreign_keys != 1 {
        return Err(Error::Invalid(
            "SQLite refused workspace durability settings".to_owned(),
        ));
    }
    Ok(connection)
}

fn initialize_schema(connection: &mut Connection) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let application_id: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let schema_version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application_id != 0 || schema_version != 0 {
        return Err(Error::Conflict(
            "workspace metadata database already has a schema".to_owned(),
        ));
    }
    tx.execute_batch(
        "CREATE TABLE workspace_state (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            lifecycle TEXT NOT NULL CHECK (lifecycle IN ('initializing', 'ready')),
            repository TEXT NOT NULL,
            change_id TEXT NOT NULL,
            base_revision TEXT NOT NULL CHECK (length(base_revision) = 64),
            generation INTEGER NOT NULL CHECK (generation >= 0)
         ) STRICT;
         CREATE TABLE operations (
            operation_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL CHECK (kind IN ('checkpoint', 'undo', 'apply')),
            request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
            status TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'cancelled')),
            expected_generation INTEGER NOT NULL CHECK (expected_generation >= 0),
            before_revision TEXT NOT NULL CHECK (length(before_revision) = 64),
            after_revision TEXT CHECK (after_revision IS NULL OR length(after_revision) = 64),
            checkpoint_operation_id TEXT,
            merge_repository TEXT,
            merge_session TEXT CHECK (merge_session IS NULL OR length(merge_session) = 64),
            merge_generation INTEGER CHECK (merge_generation IS NULL OR merge_generation >= 0),
            merge_left TEXT CHECK (merge_left IS NULL OR length(merge_left) = 64),
            merge_revision TEXT CHECK (merge_revision IS NULL OR length(merge_revision) = 64),
            outcome_json BLOB,
            CHECK ((status = 'pending') = (outcome_json IS NULL)),
            CHECK (
                (kind = 'apply' AND merge_repository IS NOT NULL
                    AND merge_session IS NOT NULL AND merge_generation IS NOT NULL
                    AND merge_left IS NOT NULL AND merge_revision IS NOT NULL
                    AND after_revision IS NOT NULL AND before_revision = merge_left
                    AND after_revision = merge_revision
                    AND checkpoint_operation_id IS NULL)
                OR
                (kind != 'apply' AND merge_repository IS NULL
                    AND merge_session IS NULL AND merge_generation IS NULL
                    AND merge_left IS NULL AND merge_revision IS NULL)
            )
         ) STRICT;
         CREATE UNIQUE INDEX one_pending_workspace_operation
            ON operations(status) WHERE status = 'pending';
         CREATE TABLE staged_paths (
            operation_id TEXT NOT NULL,
            path TEXT NOT NULL,
            before_content BLOB,
            after_content BLOB,
            before_kind TEXT NOT NULL DEFAULT 'regular'
                CHECK (before_kind IN ('regular', 'executable', 'symlink')),
            after_kind TEXT NOT NULL DEFAULT 'regular'
                CHECK (after_kind IN ('regular', 'executable', 'symlink')),
            PRIMARY KEY(operation_id, path),
            FOREIGN KEY(operation_id) REFERENCES operations(operation_id)
         ) STRICT;",
    )?;
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn migrate_schema(connection: &mut Connection) -> Result<()> {
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let schema_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application_id != APPLICATION_ID || !matches!(schema_version, 1 | 2) {
        return Ok(());
    }

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let locked_application_id: i64 =
        tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let locked_schema_version: i64 =
        tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if locked_application_id == APPLICATION_ID && locked_schema_version == SCHEMA_VERSION {
        tx.commit()?;
        return Ok(());
    }
    if locked_application_id != APPLICATION_ID || !matches!(locked_schema_version, 1 | 2) {
        return Err(Error::Invalid(format!(
            "workspace schema changed during migration ({locked_application_id}/{locked_schema_version})"
        )));
    }
    if locked_schema_version == 1 {
        migrate_v1_to_v2(&tx)?;
    }
    // v3 adds the entry kind of each staged side; existing rows are regular files.
    tx.execute_batch(
        "ALTER TABLE staged_paths ADD COLUMN before_kind TEXT NOT NULL DEFAULT 'regular'
            CHECK (before_kind IN ('regular', 'executable', 'symlink'));
         ALTER TABLE staged_paths ADD COLUMN after_kind TEXT NOT NULL DEFAULT 'regular'
            CHECK (after_kind IN ('regular', 'executable', 'symlink'));",
    )?;
    let foreign_key_violation = {
        let mut statement = tx.prepare("PRAGMA foreign_key_check")?;
        let mut rows = statement.query([])?;
        rows.next()?.is_some()
    };
    if foreign_key_violation {
        return Err(Error::Invalid(
            "workspace schema migration found a foreign-key violation".to_owned(),
        ));
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn migrate_v1_to_v2(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE operations_v2 (
            operation_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL CHECK (kind IN ('checkpoint', 'undo', 'apply')),
            request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
            status TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'cancelled')),
            expected_generation INTEGER NOT NULL CHECK (expected_generation >= 0),
            before_revision TEXT NOT NULL CHECK (length(before_revision) = 64),
            after_revision TEXT CHECK (after_revision IS NULL OR length(after_revision) = 64),
            checkpoint_operation_id TEXT,
            merge_repository TEXT,
            merge_session TEXT CHECK (merge_session IS NULL OR length(merge_session) = 64),
            merge_generation INTEGER CHECK (merge_generation IS NULL OR merge_generation >= 0),
            merge_left TEXT CHECK (merge_left IS NULL OR length(merge_left) = 64),
            merge_revision TEXT CHECK (merge_revision IS NULL OR length(merge_revision) = 64),
            outcome_json BLOB,
            CHECK ((status = 'pending') = (outcome_json IS NULL)),
            CHECK (
                (kind = 'apply' AND merge_repository IS NOT NULL
                    AND merge_session IS NOT NULL AND merge_generation IS NOT NULL
                    AND merge_left IS NOT NULL AND merge_revision IS NOT NULL
                    AND after_revision IS NOT NULL AND before_revision = merge_left
                    AND after_revision = merge_revision
                    AND checkpoint_operation_id IS NULL)
                OR
                (kind != 'apply' AND merge_repository IS NULL
                    AND merge_session IS NULL AND merge_generation IS NULL
                    AND merge_left IS NULL AND merge_revision IS NULL)
            )
         ) STRICT;
         CREATE TABLE staged_paths_v2 (
            operation_id TEXT NOT NULL,
            path TEXT NOT NULL,
            before_content BLOB,
            after_content BLOB,
            PRIMARY KEY(operation_id, path),
            FOREIGN KEY(operation_id) REFERENCES operations_v2(operation_id)
         ) STRICT;
         INSERT INTO operations_v2(
            operation_id, kind, request_digest, status, expected_generation,
            before_revision, after_revision, checkpoint_operation_id, outcome_json
         )
         SELECT operation_id, kind, request_digest, status, expected_generation,
            before_revision, after_revision, checkpoint_operation_id, outcome_json
         FROM operations;
         INSERT INTO staged_paths_v2(operation_id, path, before_content, after_content)
         SELECT operation_id, path, before_content, after_content FROM staged_paths;
         DROP TABLE staged_paths;
         DROP INDEX one_pending_workspace_operation;
         DROP TABLE operations;
         ALTER TABLE operations_v2 RENAME TO operations;
         ALTER TABLE staged_paths_v2 RENAME TO staged_paths;
         CREATE UNIQUE INDEX one_pending_workspace_operation
            ON operations(status) WHERE status = 'pending';",
    )?;
    let foreign_key_violation = {
        let mut statement = tx.prepare("PRAGMA foreign_key_check")?;
        let mut rows = statement.query([])?;
        rows.next()?.is_some()
    };
    if foreign_key_violation {
        return Err(Error::Invalid(
            "workspace schema migration found a foreign-key violation".to_owned(),
        ));
    }
    Ok(())
}

fn validate_schema(connection: &Connection) -> Result<()> {
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let schema_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application_id != APPLICATION_ID {
        return Err(Error::Invalid(format!(
            "workspace metadata belongs to a different application ({application_id})"
        )));
    }
    if schema_version != SCHEMA_VERSION {
        return Err(Error::Invalid(format!(
            "unsupported workspace schema version {schema_version}"
        )));
    }
    Ok(())
}

fn read_state(connection: &Connection) -> Result<StoredState> {
    let (repository, id, base_revision, generation, lifecycle): (
        String,
        String,
        String,
        i64,
        String,
    ) = connection.query_row(
        "SELECT repository, change_id, base_revision, generation, lifecycle FROM workspace_state WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )?;
    let change = ChangeId { repository, id };
    change.validate()?;
    validate_id(&base_revision)?;
    let ready = match lifecycle.as_str() {
        "ready" => true,
        "initializing" => false,
        _ => return Err(Error::Invalid("invalid workspace lifecycle".to_owned())),
    };
    Ok(StoredState {
        change,
        base_revision,
        generation: sql_u64("workspace generation", generation)?,
        ready,
    })
}

fn state_with_pending(connection: &Connection) -> Result<WorkspaceState> {
    let state = read_state(connection)?;
    Ok(WorkspaceState {
        change: state.change,
        base_revision: state.base_revision,
        generation: state.generation,
        ready: state.ready,
        pending: pending_operation(connection)?,
    })
}

fn pending_operation(connection: &Connection) -> Result<Option<PendingOperation>> {
    let row: Option<(String, String)> = connection
        .query_row(
            "SELECT operation_id, kind FROM operations WHERE status = 'pending'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(operation_id, kind)| {
        Ok(PendingOperation {
            operation_id,
            kind: parse_operation_kind(&kind)?,
        })
    })
    .transpose()
}

fn parse_operation_kind(value: &str) -> Result<OperationKind> {
    match value {
        "checkpoint" => Ok(OperationKind::Checkpoint),
        "undo" => Ok(OperationKind::Undo),
        "apply" => Ok(OperationKind::Apply),
        _ => Err(Error::Invalid(format!(
            "invalid workspace operation kind {value}"
        ))),
    }
}

fn operation_kind_name(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Checkpoint => "checkpoint",
        OperationKind::Undo => "undo",
        OperationKind::Apply => "apply",
    }
}

fn read_operation(connection: &Connection, operation_id: &str) -> Result<Option<StoredOperation>> {
    let row: Option<OperationRow> = connection
        .query_row(
            "SELECT kind, request_digest, expected_generation, before_revision, after_revision, status,
                outcome_json, merge_repository, merge_session, merge_generation,
                merge_left, merge_revision
             FROM operations WHERE operation_id = ?1",
            [operation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(
            kind,
            request_digest,
            expected_generation,
            before_revision,
            after_revision,
            status,
            outcome_json,
            merge_repository,
            merge_session,
            merge_generation,
            merge_left,
            merge_revision,
        )| {
            let kind = parse_operation_kind(&kind)?;
            let outcome = match (status.as_str(), outcome_json) {
                ("pending", None) => None,
                ("completed" | "cancelled", Some(bytes)) => Some(serde_json::from_slice(&bytes)?),
                _ => {
                    return Err(Error::Invalid(format!(
                        "workspace operation {operation_id} has inconsistent status"
                    )));
                }
            };
            let merge_completion = match (
                kind,
                merge_repository,
                merge_session,
                merge_generation,
                merge_left,
                merge_revision,
            ) {
                (
                    OperationKind::Apply,
                    Some(repository),
                    Some(session),
                    Some(generation),
                    Some(left),
                    Some(revision),
                ) => {
                    validate_id(&session)?;
                    validate_id(&left)?;
                    validate_id(&revision)?;
                    Some(StoredMergeCompletion {
                        repository,
                        session,
                        generation: sql_u64("merge generation", generation)?,
                        left,
                        revision,
                    })
                }
                (OperationKind::Apply, ..) => {
                    return Err(Error::Invalid(format!(
                        "workspace apply operation {operation_id} has incomplete merge bindings"
                    )));
                }
                (_, None, None, None, None, None) => None,
                _ => {
                    return Err(Error::Invalid(format!(
                        "workspace operation {operation_id} has unexpected merge bindings"
                    )));
                }
            };
            Ok(StoredOperation {
                kind,
                request_digest,
                expected_generation: sql_u64("workspace generation", expected_generation)?,
                before_revision,
                after_revision,
                merge_completion,
                outcome,
            })
        },
    )
    .transpose()
}

fn matching_operation(
    connection: &Connection,
    operation_id: &str,
    request_digest: &str,
) -> Result<Option<Option<OperationOutcome>>> {
    let row: Option<(String, String, Option<Vec<u8>>)> = connection
        .query_row(
            "SELECT request_digest, status, outcome_json FROM operations WHERE operation_id = ?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((stored_digest, status, outcome_json)) = row else {
        return Ok(None);
    };
    if stored_digest != request_digest {
        return Err(Error::Conflict(format!(
            "workspace operation ID {operation_id} was reused with different input"
        )));
    }
    match (status.as_str(), outcome_json) {
        ("pending", None) => Ok(Some(None)),
        ("completed" | "cancelled", Some(bytes)) => Ok(Some(Some(serde_json::from_slice(&bytes)?))),
        _ => Err(Error::Invalid(format!(
            "workspace operation {operation_id} has inconsistent status"
        ))),
    }
}

fn ensure_no_pending(connection: &Connection) -> Result<()> {
    if let Some(pending) = pending_operation(connection)? {
        return Err(Error::Conflict(format!(
            "workspace operation {} is pending recovery",
            pending.operation_id
        )));
    }
    Ok(())
}

fn require_ready_generation(state: &StoredState, expected_generation: u64) -> Result<()> {
    if !state.ready {
        return Err(Error::Conflict(
            "workspace initialization requires recovery".to_owned(),
        ));
    }
    if state.generation != expected_generation {
        return Err(Error::Conflict(format!(
            "workspace generation is {}, expected {expected_generation}",
            state.generation
        )));
    }
    Ok(())
}

fn insert_operation(
    connection: &Connection,
    operation_id: &str,
    kind: OperationKind,
    request_digest: &str,
    expected_generation: u64,
    before_revision: &str,
    checkpoint_operation_id: Option<&str>,
) -> Result<()> {
    connection.execute(
        "INSERT INTO operations(operation_id, kind, request_digest, status, expected_generation, before_revision, checkpoint_operation_id) VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6)",
        params![operation_id, operation_kind_name(kind), request_digest, sql_i64("workspace generation", expected_generation)?, before_revision, checkpoint_operation_id],
    )?;
    Ok(())
}

fn insert_apply_operation(
    connection: &Connection,
    operation_id: &str,
    request_digest: &str,
    expected_generation: u64,
    before_revision: &str,
    completion: &crate::merge::MergeCompletion,
) -> Result<()> {
    connection.execute(
        "INSERT INTO operations(
            operation_id, kind, request_digest, status, expected_generation,
            before_revision, after_revision, merge_repository, merge_session,
            merge_generation, merge_left, merge_revision
         ) VALUES (?1, 'apply', ?2, 'pending', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            operation_id,
            request_digest,
            sql_i64("workspace generation", expected_generation)?,
            before_revision,
            completion.revision,
            completion.repository,
            completion.session,
            sql_i64("merge generation", completion.generation)?,
            completion.left,
            completion.revision,
        ],
    )?;
    Ok(())
}

fn insert_staged(
    connection: &Connection,
    operation_id: &str,
    staged: &BTreeMap<String, Stage>,
) -> Result<()> {
    let mut statement = connection.prepare(
        "INSERT INTO staged_paths(operation_id, path, before_content, after_content, before_kind, after_kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (path, stage) in staged {
        statement.execute(params![
            operation_id,
            path,
            stage.before.as_ref().map(|item| item.content.as_slice()),
            stage.after.as_ref().map(|item| item.content.as_slice()),
            stage
                .before
                .as_ref()
                .map_or("regular", |item| item.kind.as_str()),
            stage
                .after
                .as_ref()
                .map_or("regular", |item| item.kind.as_str()),
        ])?;
    }
    Ok(())
}

fn read_staged(connection: &Connection, operation_id: &str) -> Result<BTreeMap<String, Stage>> {
    read_staged_inner(connection, operation_id, false)
}

fn read_staged_allow_empty(
    connection: &Connection,
    operation_id: &str,
) -> Result<BTreeMap<String, Stage>> {
    read_staged_inner(connection, operation_id, true)
}

fn read_staged_inner(
    connection: &Connection,
    operation_id: &str,
    allow_empty: bool,
) -> Result<BTreeMap<String, Stage>> {
    let mut statement = connection.prepare(
        "SELECT path, before_content, after_content, before_kind, after_kind FROM staged_paths WHERE operation_id = ?1 ORDER BY path",
    )?;
    let rows = statement.query_map([operation_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<Vec<u8>>>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut staged = BTreeMap::new();
    for row in rows {
        let (path, before, after, before_kind, after_kind) = row?;
        let stage = Stage {
            before: stored_item(before, &before_kind)?,
            after: stored_item(after, &after_kind)?,
        };
        if staged.insert(path, stage).is_some() {
            return Err(Error::Invalid(
                "workspace operation has duplicate staged paths".to_owned(),
            ));
        }
    }
    if staged.is_empty() && !allow_empty {
        return Err(Error::Invalid(format!(
            "workspace operation {operation_id} has no staged paths"
        )));
    }
    Ok(staged)
}

fn stored_item(content: Option<Vec<u8>>, kind: &str) -> Result<Option<Item>> {
    let kind = match kind {
        "regular" => EntryKind::Regular,
        "executable" => EntryKind::Executable,
        "symlink" => EntryKind::Symlink,
        _ => {
            return Err(Error::Invalid(format!(
                "invalid staged workspace entry kind {kind}"
            )));
        }
    };
    Ok(content.map(|content| Item { kind, content }))
}

fn complete_operation(
    connection: &Connection,
    operation_id: &str,
    status: &str,
    outcome: &OperationOutcome,
) -> Result<()> {
    let outcome_json = serde_json::to_vec(outcome)?;
    connection.execute(
        "DELETE FROM staged_paths WHERE operation_id = ?1",
        [operation_id],
    )?;
    if connection.execute(
        "UPDATE operations SET status = ?1, outcome_json = ?2 WHERE operation_id = ?3 AND status = 'pending'",
        params![status, outcome_json, operation_id],
    )? != 1
    {
        return Err(Error::Conflict(format!(
            "workspace operation {operation_id} is no longer pending"
        )));
    }
    Ok(())
}

fn completed_checkpoint(connection: &Connection, operation_id: &str) -> Result<(String, String)> {
    let row: Option<(String, String, Option<String>, String)> = connection
        .query_row(
            "SELECT kind, before_revision, after_revision, status FROM operations WHERE operation_id = ?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((kind, before, after, status)) = row else {
        return Err(Error::NotFound(format!(
            "checkpoint operation {operation_id}"
        )));
    };
    if kind != "checkpoint" || status != "completed" {
        return Err(Error::Conflict(format!(
            "workspace operation {operation_id} is not a completed checkpoint"
        )));
    }
    let after = after.ok_or_else(|| {
        Error::Invalid(format!(
            "completed checkpoint operation {operation_id} has no revision"
        ))
    })?;
    Ok((before, after))
}

fn source_snapshot(engine: &Engine, revision: &str) -> Result<Option<Snapshot>> {
    let mut files = BTreeMap::new();
    let mut directories = BTreeSet::new();
    let mut cursor = None;
    let mut total_bytes = 0usize;
    let mut seen_cursors = BTreeSet::new();
    loop {
        let page = engine.list_files(revision, cursor.as_deref(), LIST_PAGE_SIZE)?;
        if page.entries.is_empty() && page.next.is_some() {
            return Err(Error::Invalid(
                "source listing returned an empty nonterminal page".to_owned(),
            ));
        }
        for entry in page.entries {
            if reserved_path(&entry.path) {
                return Err(Error::Conflict(format!(
                    "revision {revision} uses reserved workspace path {}",
                    entry.path
                )));
            }
            if files.contains_key(&entry.path) {
                return Err(Error::Invalid(
                    "source listing returned a duplicate path".to_owned(),
                ));
            }
            let parent_count = entry.path.bytes().filter(|byte| *byte == b'/').count();
            if parent_count > MAX_DEPTH {
                return Ok(None);
            }
            for (separator, _) in entry.path.match_indices('/') {
                directories.insert(entry.path[..separator].to_owned());
            }
            if files.len() + directories.len() >= MAX_ENTRIES {
                return Ok(None);
            }
            let content = engine.read_file(revision, &entry.path)?;
            if content.len() > MAX_FILE_BYTES {
                return Ok(None);
            }
            total_bytes = total_bytes
                .checked_add(content.len())
                .ok_or_else(|| Error::Limit("workspace byte count overflow".to_owned()))?;
            if total_bytes > MAX_TOTAL_BYTES {
                return Ok(None);
            }
            files.insert(
                entry.path,
                Item {
                    kind: entry.kind,
                    content,
                },
            );
        }
        match page.next {
            Some(next) => {
                if !seen_cursors.insert(next.clone()) {
                    return Err(Error::Invalid(
                        "source listing cursor did not advance".to_owned(),
                    ));
                }
                cursor = Some(next);
            }
            None => break,
        }
    }
    Ok(Some(Snapshot { files, directories }))
}

fn scan_workspace(root: &Path) -> Result<Option<Snapshot>> {
    // Kinds are recorded from file modes, so a mount that reports every file
    // executable must be refused rather than reported as phantom changes.
    require_executable_support(&root.join(METADATA_DIRECTORY))?;
    let mut snapshot = Snapshot {
        files: BTreeMap::new(),
        directories: BTreeSet::new(),
    };
    let mut portable = BTreeMap::new();
    let mut total_bytes = 0usize;
    let mut entry_count = 0usize;
    if !scan_directory(
        root,
        "",
        0,
        &mut snapshot,
        &mut portable,
        &mut total_bytes,
        &mut entry_count,
    )? {
        return Ok(None);
    }
    Ok(Some(snapshot))
}

fn scan_directory(
    directory: &Path,
    prefix: &str,
    depth: usize,
    snapshot: &mut Snapshot,
    portable: &mut BTreeMap<String, String>,
    total_bytes: &mut usize,
    entry_count: &mut usize,
) -> Result<bool> {
    if depth > MAX_DEPTH {
        return Ok(false);
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if prefix.is_empty() && entry.file_name() == METADATA_DIRECTORY {
            continue;
        }
        if *entry_count == MAX_ENTRIES {
            return Ok(false);
        }
        *entry_count += 1;
        entries.push(entry);
    }
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Invalid("workspace path is not UTF-8".to_owned()))?;
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        validate_workspace_path(&relative)?;
        let folded = relative.to_ascii_lowercase();
        if let Some(existing) = portable.insert(folded, relative.clone())
            && existing != relative
        {
            return Err(Error::Conflict(format!(
                "workspace paths collide on portable filesystems: {existing} and {relative}"
            )));
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            // Never followed: the link itself is the entry, so a link to a
            // directory is not descended into.
            let item = read_symlink_item(&path, &relative)?;
            *total_bytes = total_bytes
                .checked_add(item.content.len())
                .ok_or_else(|| Error::Limit("workspace byte count overflow".to_owned()))?;
            if *total_bytes > MAX_TOTAL_BYTES {
                return Ok(false);
            }
            snapshot.files.insert(relative, item);
            continue;
        }
        if file_type.is_dir() {
            snapshot.directories.insert(relative.clone());
            if !scan_directory(
                &path,
                &relative,
                depth + 1,
                snapshot,
                portable,
                total_bytes,
                entry_count,
            )? {
                return Ok(false);
            }
            continue;
        }
        if !file_type.is_file() {
            return Err(Error::Invalid(format!(
                "workspace contains a special file: {relative}"
            )));
        }
        if metadata.len() > MAX_FILE_BYTES as u64 {
            return Ok(false);
        }
        let item = read_regular_file(&path, &relative)?;
        *total_bytes = total_bytes
            .checked_add(item.content.len())
            .ok_or_else(|| Error::Limit("workspace byte count overflow".to_owned()))?;
        if *total_bytes > MAX_TOTAL_BYTES {
            return Ok(false);
        }
        snapshot.files.insert(relative, item);
    }
    Ok(true)
}

/// Owner execute bit on a regular file, as Git decides it.
fn kind_of_mode(mode: u32) -> EntryKind {
    if mode & 0o100 != 0 {
        EntryKind::Executable
    } else {
        EntryKind::Regular
    }
}

/// Read a symbolic link without following it and validate its target against
/// the store's canonical rule. A link swapped during the read is a conflict.
fn read_symlink_item(path: &Path, relative: &str) -> Result<Item> {
    let target = fs::read_link(path)?.into_os_string().into_vec();
    validate_symlink_target(relative, &target)?;
    let after = fs::symlink_metadata(path)?;
    if !after.file_type().is_symlink() || fs::read_link(path)?.as_os_str().as_bytes() != target {
        return Err(Error::Conflict(format!(
            "workspace symbolic link changed while it was read: {relative}"
        )));
    }
    Ok(Item {
        kind: EntryKind::Symlink,
        content: target,
    })
}

fn read_regular_file(path: &Path, relative: &str) -> Result<Item> {
    let before = fs::symlink_metadata(path)?;
    if !before.file_type().is_file() || before.file_type().is_symlink() {
        return Err(Error::Invalid(format!(
            "workspace path is not a regular file: {relative}"
        )));
    }
    let mut file = File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() > MAX_FILE_BYTES as u64 {
        return Err(Error::Limit(format!(
            "workspace file exceeds {MAX_FILE_BYTES} bytes: {relative}"
        )));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Error::Limit(format!(
            "workspace file exceeds {MAX_FILE_BYTES} bytes: {relative}"
        )));
    }
    let after = fs::symlink_metadata(path)?;
    if !after.file_type().is_file()
        || after.file_type().is_symlink()
        || before.len() != opened.len()
        || opened.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || before.permissions().mode() != opened.permissions().mode()
        || opened.permissions().mode() != after.permissions().mode()
    {
        return Err(Error::Conflict(format!(
            "workspace file changed while it was read: {relative}"
        )));
    }
    Ok(Item {
        kind: kind_of_mode(opened.permissions().mode()),
        content: bytes,
    })
}

fn changes_between(
    before: &BTreeMap<String, Item>,
    after: &BTreeMap<String, Item>,
) -> Vec<WorkspaceChange> {
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    paths
        .into_iter()
        .filter_map(|path| match (before.get(&path), after.get(&path)) {
            (None, Some(_)) => Some(WorkspaceChange::Added { path }),
            (Some(_), None) => Some(WorkspaceChange::Deleted { path }),
            (Some(left), Some(right)) if left.kind != right.kind => {
                Some(WorkspaceChange::KindChanged {
                    path,
                    before: left.kind,
                    after: right.kind,
                })
            }
            (Some(left), Some(right)) if left != right => Some(WorkspaceChange::Modified { path }),
            _ => None,
        })
        .collect()
}

fn staged_between(
    before: &BTreeMap<String, Item>,
    after: &BTreeMap<String, Item>,
) -> BTreeMap<String, Stage> {
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let left = before.get(&path).cloned();
            let right = after.get(&path).cloned();
            (left != right).then_some((
                path,
                Stage {
                    before: left,
                    after: right,
                },
            ))
        })
        .collect()
}

fn require_complete_snapshot(snapshot: Option<Snapshot>) -> Result<Snapshot> {
    snapshot.ok_or_else(|| {
        Error::Limit(format!(
            "workspace traversal exceeds {MAX_ENTRIES} entries or {MAX_TOTAL_BYTES} bytes"
        ))
    })
}

fn finish_undo(
    root: &Path,
    tx: Transaction<'_>,
    operation_id: &str,
    choice: RecoveryChoice,
) -> Result<OperationOutcome> {
    let operation = read_operation(&tx, operation_id)?
        .ok_or_else(|| Error::NotFound(format!("workspace operation {operation_id}")))?;
    if let Some(outcome) = operation.outcome {
        return Ok(outcome);
    }
    if operation.kind != OperationKind::Undo {
        return Err(Error::Conflict(format!(
            "workspace operation {operation_id} is not an undo"
        )));
    }
    let state = read_state(&tx)?;
    require_ready_generation(&state, operation.expected_generation)?;
    if state.base_revision != operation.before_revision {
        return Err(Error::Conflict(
            "workspace base changed while undo was pending".to_owned(),
        ));
    }
    let staged = read_staged(&tx, operation_id)?;
    let desired_after = choice == RecoveryChoice::Complete;
    let working = require_complete_snapshot(scan_workspace(root)?)?;
    let blocked = blocked_paths(&working, &staged, root, operation_id, desired_after)?;
    if !blocked.is_empty() {
        return Ok(OperationOutcome::Blocked { paths: blocked });
    }
    apply_staged(root, operation_id, &staged, desired_after)?;
    if choice == RecoveryChoice::RollBack {
        let outcome = OperationOutcome::Cancelled {
            generation: state.generation,
        };
        complete_operation(&tx, operation_id, "cancelled", &outcome)?;
        tx.commit()?;
        return Ok(outcome);
    }
    let target: String = tx.query_row(
        "SELECT after_revision FROM operations WHERE operation_id = ?1",
        [operation_id],
        |row| row.get(0),
    )?;
    validate_id(&target)?;
    let generation = next_generation(state.generation)?;
    if tx.execute(
        "UPDATE workspace_state SET base_revision = ?1, generation = ?2 WHERE singleton = 1 AND generation = ?3 AND base_revision = ?4",
        params![target, sql_i64("workspace generation", generation)?, sql_i64("workspace generation", state.generation)?, state.base_revision],
    )? != 1
    {
        return Err(Error::Conflict(
            "workspace changed while undo was completing".to_owned(),
        ));
    }
    let outcome = OperationOutcome::Undone {
        revision: target,
        generation,
    };
    complete_operation(&tx, operation_id, "completed", &outcome)?;
    tx.commit()?;
    Ok(outcome)
}

fn blocked_paths(
    working: &Snapshot,
    staged: &BTreeMap<String, Stage>,
    root: &Path,
    operation_id: &str,
    desired_after: bool,
) -> Result<Vec<String>> {
    let staged_paths: BTreeSet<&str> = staged.keys().map(String::as_str).collect();
    let mut blocked = BTreeSet::new();
    for (path, content) in &working.files {
        if let Some(stage) = staged.get(path) {
            if stage.before.as_ref() != Some(content) && stage.after.as_ref() != Some(content) {
                blocked.insert(path.clone());
            }
        } else if intersects_staged(path, &staged_paths) {
            blocked.insert(path.clone());
        }
    }
    for (path, stage) in staged {
        if working.files.contains_key(path) {
            continue;
        }
        let before_absent = stage.before.is_none();
        let after_absent = stage.after.is_none();
        if before_absent || after_absent {
            continue;
        }
        let desired = if desired_after {
            stage.after.as_ref()
        } else {
            stage.before.as_ref()
        };
        if !temp_exists(root, operation_id, path, desired)? {
            blocked.insert(path.clone());
        }
    }
    for path in &working.directories {
        if has_desired_file_ancestor(path, staged, desired_after)
            && !has_staged_descendant(path, staged)
        {
            blocked.insert(path.clone());
            continue;
        }
        let Some(stage) = staged.get(path) else {
            continue;
        };
        let desired = if desired_after {
            stage.after.as_ref()
        } else {
            stage.before.as_ref()
        };
        if desired.is_some() {
            let opposite = if desired_after {
                stage.before.as_ref()
            } else {
                stage.after.as_ref()
            };
            let had_tracked_descendants = opposite.is_none()
                && staged
                    .keys()
                    .any(|candidate| candidate.starts_with(&format!("{path}/")));
            if !had_tracked_descendants {
                blocked.insert(path.clone());
            }
        }
    }
    Ok(blocked.into_iter().collect())
}

fn intersects_staged(path: &str, staged: &BTreeSet<&str>) -> bool {
    if staged.contains(path) {
        return true;
    }
    let mut prefix = path;
    while let Some((parent, _)) = prefix.rsplit_once('/') {
        if staged.contains(parent) {
            return true;
        }
        prefix = parent;
    }
    let descendant_prefix = format!("{path}/");
    staged
        .range(descendant_prefix.as_str()..)
        .next()
        .is_some_and(|candidate| candidate.starts_with(&descendant_prefix))
}

fn has_desired_file_ancestor(
    path: &str,
    staged: &BTreeMap<String, Stage>,
    desired_after: bool,
) -> bool {
    let mut prefix = path;
    while let Some((parent, _)) = prefix.rsplit_once('/') {
        if staged.get(parent).is_some_and(|stage| {
            if desired_after {
                stage.after.is_some()
            } else {
                stage.before.is_some()
            }
        }) {
            return true;
        }
        prefix = parent;
    }
    false
}

fn has_staged_descendant(path: &str, staged: &BTreeMap<String, Stage>) -> bool {
    let prefix = format!("{path}/");
    staged
        .range(prefix.clone()..)
        .next()
        .is_some_and(|(candidate, _)| candidate.starts_with(&prefix))
}

fn apply_staged(
    root: &Path,
    operation_id: &str,
    staged: &BTreeMap<String, Stage>,
    desired_after: bool,
) -> Result<()> {
    // Defense in depth: the store validated these targets, but nothing is
    // written until every one passes again, and the filesystem is probed.
    for (path, stage) in staged {
        if let Some(item) = desired_item(stage, desired_after)
            && item.kind == EntryKind::Symlink
        {
            validate_symlink_target(path, &item.content)?;
        }
    }
    require_entry_support(
        &root.join(METADATA_DIRECTORY),
        staged
            .values()
            .filter_map(|stage| desired_item(stage, desired_after)),
    )?;
    for (path, stage) in staged {
        if let Some(item) = desired_item(stage, desired_after) {
            prepare_temp(root, operation_id, path, item)?;
        }
    }

    let mut removals: Vec<_> = staged.iter().collect();
    removals.sort_by(|(left, _), (right, _)| {
        path_depth(left)
            .cmp(&path_depth(right))
            .then_with(|| left.cmp(right))
    });
    for (path, stage) in removals {
        let desired = desired_item(stage, desired_after);
        reject_symlink_ancestors(root, path)?;
        let target = root.join(path);
        let metadata = match fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(error)
                if error.kind() == ErrorKind::NotFound
                    || (error.kind() == ErrorKind::NotADirectory
                        && has_desired_file_ancestor(path, staged, desired_after)) =>
            {
                continue;
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let actual = if metadata.file_type().is_symlink() {
            read_symlink_item(&target, path)?
        } else if metadata.file_type().is_file() {
            read_regular_file(&target, path)?
        } else {
            continue;
        };
        if desired == Some(&actual) {
            continue;
        }
        if stage.before.as_ref() != Some(&actual) && stage.after.as_ref() != Some(&actual) {
            return Err(Error::Conflict(format!(
                "workspace path diverged before replacement: {path}"
            )));
        }
        // Removes a symlink itself, never what it points at.
        fs::remove_file(&target)?;
        sync_directory(target.parent().expect("workspace file has parent"))?;
    }

    prune_empty_directories(root, staged, desired_after)?;

    for (path, stage) in staged {
        let Some(item) = desired_item(stage, desired_after) else {
            continue;
        };
        reject_symlink_ancestors(root, path)?;
        let target = root.join(path);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
                let actual = if metadata.file_type().is_symlink() {
                    read_symlink_item(&target, path)?
                } else {
                    read_regular_file(&target, path)?
                };
                if actual == *item {
                    remove_temp(root, operation_id, path, item)?;
                    continue;
                }
                return Err(Error::Conflict(format!(
                    "workspace path diverged before installation: {path}"
                )));
            }
            Ok(metadata) if metadata.file_type().is_dir() => {
                fs::remove_dir(&target).map_err(|error| {
                    if error.kind() == ErrorKind::DirectoryNotEmpty {
                        Error::Conflict(format!(
                            "workspace directory contains untracked entries: {path}"
                        ))
                    } else {
                        Error::Io(error)
                    }
                })?;
                sync_directory(target.parent().expect("workspace path has parent"))?;
            }
            Ok(_) => {
                return Err(Error::Conflict(format!(
                    "workspace path became a special file: {path}"
                )));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(Error::Io(error)),
        }
        create_parent_directories(root, path)?;
        let temporary = temp_path(root, operation_id, path, item);
        // Both installs are exclusive: they fail rather than replace a path
        // that appeared after the check above.
        let installed = if item.kind == EntryKind::Symlink {
            symlink(std::ffi::OsStr::from_bytes(&item.content), &target)
        } else {
            fs::hard_link(&temporary, &target)
        };
        match installed {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return Err(Error::Conflict(format!(
                    "workspace path appeared before installation: {path}"
                )));
            }
            Err(error) => return Err(Error::Io(error)),
        }
        sync_directory(target.parent().expect("workspace path has parent"))?;
        fs::remove_file(&temporary)?;
        sync_directory(temporary.parent().expect("workspace temp has parent"))?;
    }

    sync_directory(root)?;
    Ok(())
}

/// Refusal point before an undo or apply is journaled: its after side is what
/// would be materialized.
fn require_staged_support(root: &Path, staged: &BTreeMap<String, Stage>) -> Result<()> {
    require_entry_support(
        &root.join(METADATA_DIRECTORY),
        staged.values().filter_map(|stage| stage.after.as_ref()),
    )
}

fn desired_item(stage: &Stage, desired_after: bool) -> Option<&Item> {
    if desired_after {
        stage.after.as_ref()
    } else {
        stage.before.as_ref()
    }
}

/// Refuse to resolve `relative` through a symbolic link in any parent
/// directory, so a link in the workspace cannot redirect a write or removal
/// outside it. Absent parents are fine; the caller's own lstat reports them.
fn reject_symlink_ancestors(root: &Path, relative: &str) -> Result<()> {
    let mut current = root.to_path_buf();
    let Some(parent) = Path::new(relative).parent() else {
        return Ok(());
    };
    for component in parent.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::Conflict(format!(
                    "workspace path ancestor is a symbolic link: {relative}"
                )));
            }
            Ok(_) => {}
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                return Ok(());
            }
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(())
}

/// Probe `directory`'s filesystem for the entry kinds about to be written, so
/// an unsupported one is refused before anything is journaled or installed
/// instead of being flattened to a regular file or dropped.
fn require_entry_support<'a>(
    directory: &Path,
    items: impl IntoIterator<Item = &'a Item>,
) -> Result<()> {
    let (mut symlinks, mut executables) = (false, false);
    for item in items {
        match item.kind {
            EntryKind::Symlink => symlinks = true,
            EntryKind::Executable => executables = true,
            EntryKind::Regular => {}
        }
    }
    if symlinks && !probe_symlinks(directory)? {
        return Err(Error::Invalid(
            "unsupported filesystem: it does not support symbolic links".to_owned(),
        ));
    }
    if executables {
        require_executable_support(directory)?;
    }
    Ok(())
}

/// Full probe run when a workspace is created, whatever the tree holds.
fn require_filesystem_support(directory: &Path) -> Result<()> {
    if !probe_symlinks(directory)? {
        return Err(Error::Invalid(
            "unsupported filesystem: it does not support symbolic links".to_owned(),
        ));
    }
    require_executable_support(directory)
}

fn require_executable_support(directory: &Path) -> Result<()> {
    if probe_executable_bit(directory)? {
        Ok(())
    } else {
        Err(Error::Invalid(
            "unsupported filesystem: it does not preserve executable modes".to_owned(),
        ))
    }
}

/// The owner execute bit is preserved only if it reads back clear after
/// `chmod 0600` and set after `chmod 0700`.
fn executable_bit_preserved(clear_mode: u32, set_mode: u32) -> bool {
    kind_of_mode(clear_mode) == EntryKind::Regular
        && kind_of_mode(set_mode) == EntryKind::Executable
}

const PROBE_PREFIX: &str = ".agentscm-probe-";

/// Remove probe leftovers from a process killed mid-probe. Probes run under
/// the workspace's immediate transaction (and creation owns the new metadata
/// directory), so no live probe shares the directory. Only direct entries with
/// the probe prefix that are files or symbolic links are removed; links are
/// never followed and nothing is recursed into.
fn sweep_stale_probes(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_bytes()
            .starts_with(PROBE_PREFIX.as_bytes())
        {
            continue;
        }
        let path = entry.path();
        let file_type = fs::symlink_metadata(&path)?.file_type();
        if file_type.is_file() || file_type.is_symlink() {
            match fs::remove_file(&path) {
                Err(error) if error.kind() != ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            }
        }
    }
    Ok(())
}

fn probe_path(directory: &Path, label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    directory.join(format!(
        "{PROBE_PREFIX}{label}-{}-{nanos}",
        std::process::id()
    ))
}

/// `Ok(false)` only when the filesystem itself refuses or misreports; other
/// failures stay errors.
fn probe_symlinks(directory: &Path) -> Result<bool> {
    sweep_stale_probes(directory)?;
    let path = probe_path(directory, "link");
    match symlink("probe-target", &path) {
        Ok(()) => {
            let read_back = fs::read_link(&path);
            fs::remove_file(&path)?;
            Ok(read_back.is_ok_and(|target| target.as_os_str().as_bytes() == b"probe-target"))
        }
        Err(error) if is_unsupported(&error) => Ok(false),
        Err(error) => Err(Error::Io(error)),
    }
}

fn probe_executable_bit(directory: &Path) -> Result<bool> {
    sweep_stale_probes(directory)?;
    let path = probe_path(directory, "mode");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let mode_after = |mode: u32| {
        file.set_permissions(fs::Permissions::from_mode(mode))
            .and_then(|()| file.metadata())
            .map(|metadata| metadata.permissions().mode())
    };
    let outcome = mode_after(0o600).and_then(|clear| Ok((clear, mode_after(0o700)?)));
    drop(file);
    fs::remove_file(&path)?;
    match outcome {
        Ok((clear, set)) => Ok(executable_bit_preserved(clear, set)),
        Err(error) if is_unsupported(&error) => Ok(false),
        Err(error) => Err(Error::Io(error)),
    }
}

/// Errors a filesystem uses to say it lacks a feature. Plain permission
/// failures (`EACCES`) stay errors: they say nothing about the filesystem.
fn is_unsupported(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::Unsupported
        || matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EPERM || code == libc::ENOTSUP || code == libc::ENOSYS
        )
}

fn prepare_temp(root: &Path, operation_id: &str, path: &str, item: &Item) -> Result<()> {
    let temporary = temp_path(root, operation_id, path, item);
    let directory = temporary
        .parent()
        .ok_or_else(|| Error::Invalid("workspace temporary path has no parent".to_owned()))?;
    fs::create_dir_all(directory)?;
    sync_directory(&root.join(METADATA_DIRECTORY))?;
    // A symlink's temporary is a regular marker holding its target, so
    // interrupted installs recover the same way as for files and no link is
    // ever created where a later hard link could follow it.
    let temp_kind = temp_file_kind(item.kind);
    loop {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        if temp_kind == EntryKind::Executable {
            // 0o777 minus the process umask, like any new executable.
            options.mode(0o777);
        }
        match options.open(&temporary) {
            Ok(mut file) => {
                if temp_kind == EntryKind::Executable
                    && kind_of_mode(file.metadata()?.permissions().mode()) != EntryKind::Executable
                {
                    return Err(Error::Invalid(format!(
                        "process umask clears the owner execute bit for executable: {path}"
                    )));
                }
                file.write_all(&item.content)?;
                file.sync_all()?;
                sync_directory(directory)?;
                break;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&temporary)?;
                if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                    return Err(Error::Invalid(format!(
                        "workspace recovery temporary path is not a regular file: {path}"
                    )));
                }
                let existing = read_regular_file(&temporary, path)?;
                if existing.content == item.content && existing.kind == temp_kind {
                    break;
                }
                fs::remove_file(&temporary)?;
                sync_directory(directory)?;
            }
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(())
}

fn temp_file_kind(kind: EntryKind) -> EntryKind {
    if kind == EntryKind::Executable {
        EntryKind::Executable
    } else {
        EntryKind::Regular
    }
}

fn remove_temp(root: &Path, operation_id: &str, path: &str, item: &Item) -> Result<()> {
    let temporary = temp_path(root, operation_id, path, item);
    match fs::remove_file(&temporary) {
        Ok(()) => sync_directory(temporary.parent().expect("workspace temp has parent")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

fn temp_exists(
    root: &Path,
    operation_id: &str,
    path: &str,
    desired: Option<&Item>,
) -> Result<bool> {
    let Some(item) = desired else {
        return Ok(false);
    };
    let temporary = temp_path(root, operation_id, path, item);
    match fs::symlink_metadata(&temporary) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(true)
        }
        Ok(_) => Err(Error::Invalid(format!(
            "workspace recovery temporary path is not a regular file: {path}"
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::Io(error)),
    }
}

fn temp_path(root: &Path, operation_id: &str, path: &str, item: &Item) -> PathBuf {
    let operation = digest(operation_id.as_bytes());
    let mut hasher = Sha256::new();
    // Regular files keep the original TEMP1 identity (path and content) so
    // temporaries written before executables and links existed are still
    // found by recovery. TEMP2 adds the kind for the new entry kinds only.
    if item.kind == EntryKind::Regular {
        hasher.update(b"AGSCM-WORKSPACE-TEMP1\0");
    } else {
        hasher.update(b"AGSCM-WORKSPACE-TEMP2\0");
    }
    hasher.update((path.len() as u64).to_be_bytes());
    hasher.update(path.as_bytes());
    if item.kind != EntryKind::Regular {
        hasher.update((item.kind.as_str().len() as u64).to_be_bytes());
        hasher.update(item.kind.as_str().as_bytes());
    }
    hasher.update((item.content.len() as u64).to_be_bytes());
    hasher.update(&item.content);
    let identity = format!("{:x}", hasher.finalize());
    root.join(METADATA_DIRECTORY)
        .join("temps")
        .join(&operation[..16])
        .join(&identity[..32])
}

fn create_parent_directories(root: &Path, relative: &str) -> Result<()> {
    let parent = Path::new(relative)
        .parent()
        .ok_or_else(|| Error::Invalid("workspace path has no parent".to_owned()))?;
    let mut current = root.to_path_buf();
    for component in parent.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => {
                return Err(Error::Conflict(format!(
                    "workspace path ancestor is not a directory: {}",
                    current.display()
                )));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                sync_directory(current.parent().expect("workspace directory has parent"))?;
            }
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(())
}

fn prune_empty_directories(
    root: &Path,
    staged: &BTreeMap<String, Stage>,
    desired_after: bool,
) -> Result<()> {
    let mut directories = BTreeSet::new();
    for path in staged.keys() {
        let mut parent = Path::new(path).parent();
        while let Some(value) = parent {
            if value.as_os_str().is_empty() {
                break;
            }
            directories.insert(value.to_path_buf());
            parent = value.parent();
        }
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| right.cmp(left))
    });
    for relative in directories {
        let relative_string = relative
            .to_str()
            .expect("validated workspace path is UTF-8");
        if staged.get(relative_string).is_some_and(|stage| {
            if desired_after {
                stage.after.is_some()
            } else {
                stage.before.is_some()
            }
        }) {
            continue;
        }
        let path = root.join(&relative);
        match fs::remove_dir(&path) {
            Ok(()) => sync_directory(path.parent().expect("workspace directory has parent"))?,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                ) || (error.kind() == ErrorKind::NotADirectory
                    && has_desired_file_ancestor(relative_string, staged, desired_after)) => {}
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(())
}

fn validate_merge_completion(completion: &crate::merge::MergeCompletion) -> Result<()> {
    validate_id(&completion.session)?;
    validate_id(&completion.left)?;
    validate_id(&completion.revision)
}

fn validate_completion_binding(
    state: &StoredState,
    completion: &crate::merge::MergeCompletion,
) -> Result<()> {
    if completion.repository != state.change.repository {
        return Err(Error::Conflict(
            "merge completion belongs to another repository".to_owned(),
        ));
    }
    if completion.left != state.base_revision {
        return Err(Error::Conflict(
            "merge left revision is not the current workspace base".to_owned(),
        ));
    }
    Ok(())
}

fn validate_stored_completion_binding(
    state: &StoredState,
    operation_id: &str,
    operation: &StoredOperation,
    completion: &StoredMergeCompletion,
) -> Result<()> {
    if completion.repository != state.change.repository {
        return Err(Error::Conflict(
            "stored merge completion belongs to another repository".to_owned(),
        ));
    }
    if completion.left != operation.before_revision || completion.left != state.base_revision {
        return Err(Error::Conflict(
            "stored merge left revision is not the current workspace base".to_owned(),
        ));
    }
    if operation.after_revision.as_deref() != Some(completion.revision.as_str()) {
        return Err(Error::Invalid(
            "workspace apply target differs from its stored merge completion".to_owned(),
        ));
    }
    let expected_digest = apply_operation_digest_fields(
        operation_id,
        operation.expected_generation,
        &completion.repository,
        &completion.session,
        completion.generation,
        &completion.left,
        &completion.revision,
    );
    if operation.request_digest != expected_digest {
        return Err(Error::Invalid(
            "workspace apply request digest does not match its stored merge completion".to_owned(),
        ));
    }
    Ok(())
}

fn qualify_merge_candidate(
    engine: &Engine,
    completion: &crate::merge::MergeCompletion,
) -> Result<()> {
    qualify_candidate(engine, &completion.left, &completion.revision)
}

fn qualify_stored_merge_candidate(
    engine: &Engine,
    completion: &StoredMergeCompletion,
) -> Result<()> {
    qualify_candidate(engine, &completion.left, &completion.revision)
}

fn qualify_candidate(engine: &Engine, left: &str, revision: &str) -> Result<()> {
    sync::qualify_source(engine, &[revision.to_owned()])?;
    let actual = engine.load_revision(revision)?;
    if revision != left && actual.parents.first().map(String::as_str) != Some(left) {
        return Err(Error::Conflict(
            "merge result does not have the workspace base as its first parent".to_owned(),
        ));
    }
    Ok(())
}

fn apply_operation_digest(
    operation_id: &str,
    expected_generation: u64,
    completion: &crate::merge::MergeCompletion,
) -> String {
    apply_operation_digest_fields(
        operation_id,
        expected_generation,
        &completion.repository,
        &completion.session,
        completion.generation,
        &completion.left,
        &completion.revision,
    )
}

fn apply_operation_digest_fields(
    operation_id: &str,
    expected_generation: u64,
    repository: &str,
    session: &str,
    merge_generation: u64,
    left: &str,
    revision: &str,
) -> String {
    let mut bytes = b"AGSCM-WORKSPACE-APPLY1\0".to_vec();
    push_field(&mut bytes, operation_id.as_bytes());
    bytes.extend_from_slice(&expected_generation.to_be_bytes());
    push_field(&mut bytes, repository.as_bytes());
    push_field(&mut bytes, session.as_bytes());
    bytes.extend_from_slice(&merge_generation.to_be_bytes());
    push_field(&mut bytes, left.as_bytes());
    push_field(&mut bytes, revision.as_bytes());
    digest(&bytes)
}

fn operation_digest(
    kind: &str,
    operation_id: &str,
    expected_generation: u64,
    checkpoint_operation_id: Option<&str>,
) -> String {
    let mut bytes = b"AGSCM-WORKSPACE-OP1\0".to_vec();
    push_field(&mut bytes, kind.as_bytes());
    push_field(&mut bytes, operation_id.as_bytes());
    bytes.extend_from_slice(&expected_generation.to_be_bytes());
    match checkpoint_operation_id {
        Some(value) => {
            bytes.push(1);
            push_field(&mut bytes, value.as_bytes());
        }
        None => bytes.push(0),
    }
    digest(&bytes)
}

fn push_field(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

fn validate_operation_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_OPERATION_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !byte.is_ascii_control())
    {
        return Err(Error::Invalid(format!(
            "workspace operation ID must be 1 to {MAX_OPERATION_ID_BYTES} printable ASCII bytes"
        )));
    }
    Ok(())
}

fn validate_workspace_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > 1_024 {
        return Err(Error::Invalid(
            "workspace path length must be between 1 and 1024 bytes".to_owned(),
        ));
    }
    if !path.is_ascii() || path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(Error::Invalid(
            "workspace path must be relative portable ASCII".to_owned(),
        ));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.len() > 255 {
            return Err(Error::Invalid(format!(
                "workspace path has an invalid component: {path}"
            )));
        }
        if segment.ends_with(['.', ' '])
            || segment.bytes().any(|byte| {
                byte < 0x20 || matches!(byte, b'<' | b'>' | b':' | b'"' | b'|' | b'?' | b'*')
            })
        {
            return Err(Error::Invalid(format!(
                "workspace path is not portable: {path}"
            )));
        }
        let stem = segment
            .split('.')
            .next()
            .unwrap_or(segment)
            .to_ascii_lowercase();
        if matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
            || (stem.len() == 4
                && (stem.starts_with("com") || stem.starts_with("lpt"))
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0')
        {
            return Err(Error::Invalid(format!(
                "workspace path contains a reserved device name: {path}"
            )));
        }
    }
    if reserved_path(path) {
        return Err(Error::Invalid(format!(
            "workspace path uses reserved metadata namespace: {path}"
        )));
    }
    Ok(())
}

/// Any component naming workspace or Git metadata, in any case: a case-insensitive filesystem
/// would otherwise resolve `.AGENTSCM-WORKSPACE/...` into the workspace database directory.
fn reserved_path(path: &str) -> bool {
    crate::git_path_profile::uses_reserved_namespace(path)
}

fn require_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            Error::NotFound(format!("{label}: {}", path.display()))
        } else {
            Error::Io(error)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(Error::Invalid(format!(
            "{label} is not a non-symlink directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn path_depth(path: &str) -> usize {
    path.bytes().filter(|byte| *byte == b'/').count()
}

fn next_generation(value: u64) -> Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| Error::Limit("workspace generation exhausted".to_owned()))
}

fn sql_i64(label: &str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Limit(format!("{label} does not fit SQLite")))
}

fn sql_u64(label: &str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Invalid(format!("{label} is negative")))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::FileChange;
    use tempfile::TempDir;

    fn put(path: &str, content: &[u8]) -> FileChange {
        FileChange {
            path: path.to_owned(),
            content: Some(content.to_vec()),
        }
    }

    fn delete(path: &str) -> FileChange {
        FileChange {
            path: path.to_owned(),
            content: None,
        }
    }

    struct ActualApplyFixture {
        _fixtures: TempDir,
        store_root: PathBuf,
        engine: Engine,
        workspace_root: PathBuf,
        workspace: Workspace,
        completion: crate::merge::MergeCompletion,
    }

    impl ActualApplyFixture {
        fn new() -> Self {
            let fixtures = TempDir::new().unwrap();
            let store_root = fixtures.path().join("store");
            let engine = Engine::open(&store_root).unwrap();
            let base = engine
                .checkpoint(
                    None,
                    &[
                        put("changed.txt", b"base"),
                        put("delete.txt", b"delete-base"),
                        put("dirty.txt", b"clean"),
                    ],
                )
                .unwrap();
            let left = engine
                .checkpoint(Some(&base), &[put("left.txt", b"left")])
                .unwrap();
            let right = engine
                .checkpoint(
                    Some(&base),
                    &[
                        put("changed.txt", b"right"),
                        delete("delete.txt"),
                        put("added.txt", b"added"),
                    ],
                )
                .unwrap();
            let mut journal =
                crate::merge::MergeJournal::open(&store_root, "repository-apply").unwrap();
            let (session, generation) = match journal
                .begin(&engine, "begin-apply-fixture", &left, &right)
                .unwrap()
            {
                crate::merge::MergeOutcome::Prepared {
                    session,
                    generation,
                    phase: crate::merge::MergePhase::Ready,
                } => (session, generation),
                outcome => panic!("expected ready merge fixture, got {outcome:?}"),
            };
            let completed_generation = match journal
                .complete(&engine, &session, "complete-apply-fixture", generation)
                .unwrap()
            {
                crate::merge::MergeOutcome::Completed { generation, .. } => generation,
                outcome => panic!("expected completed merge fixture, got {outcome:?}"),
            };
            let completion = journal.completion(&session, completed_generation).unwrap();
            let workspace_root = fixtures.path().join("workspace");
            let workspace = Workspace::create(
                &engine,
                &workspace_root,
                ChangeId {
                    repository: "repository-apply".to_owned(),
                    id: "change-apply".to_owned(),
                },
                &left,
            )
            .unwrap();
            Self {
                _fixtures: fixtures,
                store_root,
                engine,
                workspace_root,
                workspace,
                completion,
            }
        }

        fn stage_interrupted_apply(&self, operation_id: &str) -> BTreeMap<String, Stage> {
            let current = require_complete_snapshot(
                source_snapshot(&self.engine, &self.completion.left).unwrap(),
            )
            .unwrap();
            let target = require_complete_snapshot(
                source_snapshot(&self.engine, &self.completion.revision).unwrap(),
            )
            .unwrap();
            let staged = staged_between(&current.files, &target.files);
            let request = apply_operation_digest(operation_id, 0, &self.completion);
            let mut connection = self.workspace.connection().unwrap();
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            insert_apply_operation(
                &tx,
                operation_id,
                &request,
                0,
                &self.completion.left,
                &self.completion,
            )
            .unwrap();
            insert_staged(&tx, operation_id, &staged).unwrap();
            tx.commit().unwrap();
            staged
        }

        fn object_path(&self, id: &str) -> PathBuf {
            self.store_root
                .join("objects")
                .join(&id[..2])
                .join(&id[2..])
        }
    }

    #[test]
    fn recovers_simulated_mixed_apply_and_preserves_unrelated_dirty_content() {
        let mut fixture = ActualApplyFixture::new();
        let staged = fixture.stage_interrupted_apply("recover-mixed");
        fixture.workspace = Workspace::open(&fixture.workspace_root).unwrap();
        // Simulate interruption after one replacement reached the working tree.
        fs::write(
            fixture.workspace_root.join("changed.txt"),
            staged["changed.txt"]
                .after
                .as_ref()
                .unwrap()
                .content
                .as_slice(),
        )
        .unwrap();
        fs::write(fixture.workspace_root.join("dirty.txt"), b"local-dirty").unwrap();

        assert_eq!(
            fixture
                .workspace
                .recover(&fixture.engine, RecoveryChoice::Complete)
                .unwrap(),
            OperationOutcome::Applied {
                revision: fixture.completion.revision.clone(),
                generation: 1,
            }
        );
        assert_eq!(
            fs::read(fixture.workspace_root.join("changed.txt")).unwrap(),
            b"right"
        );
        assert_eq!(
            fs::read(fixture.workspace_root.join("added.txt")).unwrap(),
            b"added"
        );
        assert!(!fixture.workspace_root.join("delete.txt").exists());
        assert_eq!(
            fs::read(fixture.workspace_root.join("dirty.txt")).unwrap(),
            b"local-dirty"
        );
    }

    #[test]
    fn simulated_apply_recovery_blocks_divergent_affected_content() {
        let mut fixture = ActualApplyFixture::new();
        fixture.stage_interrupted_apply("recover-blocked");
        fixture.workspace = Workspace::open(&fixture.workspace_root).unwrap();
        fs::write(
            fixture.workspace_root.join("changed.txt"),
            b"local-conflict",
        )
        .unwrap();

        assert_eq!(
            fixture
                .workspace
                .recover(&fixture.engine, RecoveryChoice::Complete)
                .unwrap(),
            OperationOutcome::Blocked {
                paths: vec!["changed.txt".to_owned()],
            }
        );
        let state = fixture.workspace.state().unwrap();
        assert_eq!(state.base_revision, fixture.completion.left);
        assert_eq!(state.generation, 0);
        assert_eq!(
            fs::read(fixture.workspace_root.join("changed.txt")).unwrap(),
            b"local-conflict"
        );
    }

    #[test]
    fn simulated_apply_recovery_rejects_changed_frozen_session_digest() {
        let mut fixture = ActualApplyFixture::new();
        fixture.stage_interrupted_apply("recover-digest");
        fixture.workspace = Workspace::open(&fixture.workspace_root).unwrap();
        let connection = fixture.workspace.connection().unwrap();
        connection
            .execute(
                "UPDATE operations SET merge_session = ?1 WHERE operation_id = 'recover-digest'",
                params!["f".repeat(64)],
            )
            .unwrap();

        assert!(matches!(
            fixture
                .workspace
                .recover(&fixture.engine, RecoveryChoice::Complete),
            Err(Error::Invalid(message)) if message.contains("request digest")
        ));
        let state = fixture.workspace.state().unwrap();
        assert_eq!(state.base_revision, fixture.completion.left);
        assert_eq!(state.generation, 0);
        assert_eq!(
            fs::read(fixture.workspace_root.join("changed.txt")).unwrap(),
            b"base"
        );
        assert!(!fixture.workspace_root.join("added.txt").exists());
        assert_eq!(
            fs::read(fixture.workspace_root.join("delete.txt")).unwrap(),
            b"delete-base"
        );
    }

    #[test]
    fn simulated_apply_can_roll_back_frozen_bytes_without_candidate_root() {
        let mut fixture = ActualApplyFixture::new();
        let staged = fixture.stage_interrupted_apply("recover-missing");
        fixture.workspace = Workspace::open(&fixture.workspace_root).unwrap();
        // Simulate interruption with two candidate states already installed.
        fs::write(
            fixture.workspace_root.join("changed.txt"),
            staged["changed.txt"]
                .after
                .as_ref()
                .unwrap()
                .content
                .as_slice(),
        )
        .unwrap();
        fs::write(
            fixture.workspace_root.join("added.txt"),
            staged["added.txt"]
                .after
                .as_ref()
                .unwrap()
                .content
                .as_slice(),
        )
        .unwrap();
        fs::write(fixture.workspace_root.join("dirty.txt"), b"local-dirty").unwrap();
        fs::remove_file(fixture.object_path(&fixture.completion.revision)).unwrap();

        let missing = fixture
            .workspace
            .recover(&fixture.engine, RecoveryChoice::Complete)
            .unwrap_err();
        assert_eq!(
            missing.missing_object_id(),
            Some(fixture.completion.revision.as_str())
        );
        let state = fixture.workspace.state().unwrap();
        assert_eq!(state.base_revision, fixture.completion.left);
        assert_eq!(state.generation, 0);
        assert_eq!(
            fs::read(fixture.workspace_root.join("changed.txt")).unwrap(),
            b"right"
        );
        assert_eq!(
            fs::read(fixture.workspace_root.join("added.txt")).unwrap(),
            b"added"
        );
        assert_eq!(
            fs::read(fixture.workspace_root.join("delete.txt")).unwrap(),
            b"delete-base"
        );
        assert_eq!(
            fs::read(fixture.workspace_root.join("dirty.txt")).unwrap(),
            b"local-dirty"
        );

        assert_eq!(
            fixture
                .workspace
                .recover(&fixture.engine, RecoveryChoice::RollBack)
                .unwrap(),
            OperationOutcome::Cancelled { generation: 0 }
        );
        assert_eq!(
            fs::read(fixture.workspace_root.join("changed.txt")).unwrap(),
            b"base"
        );
        assert!(!fixture.workspace_root.join("added.txt").exists());
        assert_eq!(
            fs::read(fixture.workspace_root.join("delete.txt")).unwrap(),
            b"delete-base"
        );
        assert_eq!(
            fs::read(fixture.workspace_root.join("dirty.txt")).unwrap(),
            b"local-dirty"
        );
        let state = fixture.workspace.state().unwrap();
        assert_eq!(state.base_revision, fixture.completion.left);
        assert_eq!(state.generation, 0);
    }

    #[test]
    fn applies_merge_result_and_preserves_unrelated_dirty_content() {
        let fixtures = TempDir::new().unwrap();
        let engine = Engine::open(fixtures.path().join("store")).unwrap();
        let base = engine
            .checkpoint(
                None,
                &[put("changed.txt", b"base"), put("keep.txt", b"original")],
            )
            .unwrap();
        let right = engine
            .checkpoint(Some(&base), &[put("changed.txt", b"right")])
            .unwrap();
        let merged = engine
            .checkpoint_with_parents(&[base.clone(), right], &[put("changed.txt", b"merged")])
            .unwrap();
        let workspace_root = fixtures.path().join("workspace");
        let mut workspace = Workspace::create(
            &engine,
            &workspace_root,
            ChangeId {
                repository: "repository-1".to_owned(),
                id: "change-1".to_owned(),
            },
            &base,
        )
        .unwrap();
        fs::write(workspace_root.join("keep.txt"), b"dirty").unwrap();
        let completion = crate::merge::MergeCompletion {
            repository: "repository-1".to_owned(),
            session: "a".repeat(64),
            generation: 7,
            left: base,
            revision: merged.clone(),
        };

        let expected = OperationOutcome::Applied {
            revision: merged.clone(),
            generation: 1,
        };
        assert_eq!(
            workspace.apply(&engine, "apply-1", 0, &completion).unwrap(),
            expected
        );
        assert_eq!(
            fs::read(workspace_root.join("changed.txt")).unwrap(),
            b"merged"
        );
        assert_eq!(fs::read(workspace_root.join("keep.txt")).unwrap(), b"dirty");
        assert_eq!(workspace.state().unwrap().base_revision, merged);
        assert_eq!(
            workspace.apply(&engine, "apply-1", 0, &completion).unwrap(),
            expected
        );

        let mut changed_receipt = completion;
        changed_receipt.generation += 1;
        assert!(matches!(
            workspace.apply(&engine, "apply-1", 0, &changed_receipt),
            Err(Error::Conflict(message)) if message.contains("reused with different input")
        ));
    }

    #[test]
    fn migrates_v1_rows_and_staged_path_foreign_keys() {
        let fixtures = TempDir::new().unwrap();
        let workspace_root = fixtures.path().join("workspace");
        fs::create_dir(&workspace_root).unwrap();
        fs::create_dir(workspace_root.join(METADATA_DIRECTORY)).unwrap();
        let database = workspace_root.join(METADATA_DIRECTORY).join(DATABASE_FILE);
        let connection = open_connection(&database, true).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE workspace_state (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('initializing', 'ready')),
                    repository TEXT NOT NULL,
                    change_id TEXT NOT NULL,
                    base_revision TEXT NOT NULL CHECK (length(base_revision) = 64),
                    generation INTEGER NOT NULL CHECK (generation >= 0)
                 ) STRICT;
                 CREATE TABLE operations (
                    operation_id TEXT PRIMARY KEY,
                    kind TEXT NOT NULL CHECK (kind IN ('checkpoint', 'undo')),
                    request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
                    status TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'cancelled')),
                    expected_generation INTEGER NOT NULL CHECK (expected_generation >= 0),
                    before_revision TEXT NOT NULL CHECK (length(before_revision) = 64),
                    after_revision TEXT CHECK (after_revision IS NULL OR length(after_revision) = 64),
                    checkpoint_operation_id TEXT,
                    outcome_json BLOB,
                    CHECK ((status = 'pending') = (outcome_json IS NULL))
                 ) STRICT;
                 CREATE UNIQUE INDEX one_pending_workspace_operation
                    ON operations(status) WHERE status = 'pending';
                 CREATE TABLE staged_paths (
                    operation_id TEXT NOT NULL,
                    path TEXT NOT NULL,
                    before_content BLOB,
                    after_content BLOB,
                    PRIMARY KEY(operation_id, path),
                    FOREIGN KEY(operation_id) REFERENCES operations(operation_id)
                 ) STRICT;",
            )
            .unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();

        let before = "b".repeat(64);
        let after = "c".repeat(64);
        connection
            .execute(
                "INSERT INTO workspace_state VALUES (1, 'ready', 'repository-1', 'change-1', ?1, 1)",
                [&after],
            )
            .unwrap();
        let checkpoint_outcome = serde_json::to_vec(&OperationOutcome::Checkpointed {
            revision: after.clone(),
            generation: 1,
        })
        .unwrap();
        connection
            .execute(
                "INSERT INTO operations VALUES ('checkpoint-1', 'checkpoint', ?1, 'completed', 0, ?2, ?3, NULL, ?4)",
                params!["d".repeat(64), before, after, checkpoint_outcome],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO operations VALUES ('undo-1', 'undo', ?1, 'pending', 1, ?2, ?3, 'checkpoint-1', NULL)",
                params!["e".repeat(64), after, before],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO staged_paths VALUES ('undo-1', 'file.txt', ?1, ?2)",
                params![b"after".as_slice(), b"before".as_slice()],
            )
            .unwrap();

        drop(connection);
        let workspace = Workspace::open(&workspace_root).unwrap();
        let connection = workspace.connection().unwrap();
        validate_schema(&connection).unwrap();
        assert_eq!(
            read_operation(&connection, "checkpoint-1")
                .unwrap()
                .unwrap()
                .outcome,
            Some(OperationOutcome::Checkpointed {
                revision: "c".repeat(64),
                generation: 1,
            })
        );
        assert_eq!(
            pending_operation(&connection).unwrap().unwrap().kind,
            OperationKind::Undo
        );
        assert_eq!(read_staged(&connection, "undo-1").unwrap().len(), 1);
        let merge_bindings: i64 = connection
            .query_row(
                "SELECT count(*) FROM operations
                 WHERE merge_repository IS NOT NULL OR merge_session IS NOT NULL
                    OR merge_generation IS NOT NULL OR merge_left IS NOT NULL
                    OR merge_revision IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(merge_bindings, 0);
        assert!(
            connection
                .execute("DELETE FROM operations WHERE operation_id = 'undo-1'", [])
                .is_err()
        );
    }

    #[test]
    fn migrates_v2_staged_rows_to_regular_kinds() {
        let fixtures = TempDir::new().unwrap();
        let workspace_root = fixtures.path().join("workspace");
        fs::create_dir_all(workspace_root.join(METADATA_DIRECTORY)).unwrap();
        let database = workspace_root.join(METADATA_DIRECTORY).join(DATABASE_FILE);
        let connection = open_connection(&database, true).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE workspace_state (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('initializing', 'ready')),
                    repository TEXT NOT NULL,
                    change_id TEXT NOT NULL,
                    base_revision TEXT NOT NULL CHECK (length(base_revision) = 64),
                    generation INTEGER NOT NULL CHECK (generation >= 0)
                 ) STRICT;
                 CREATE TABLE operations (
                    operation_id TEXT PRIMARY KEY,
                    kind TEXT NOT NULL CHECK (kind IN ('checkpoint', 'undo', 'apply')),
                    request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
                    status TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'cancelled')),
                    expected_generation INTEGER NOT NULL CHECK (expected_generation >= 0),
                    before_revision TEXT NOT NULL CHECK (length(before_revision) = 64),
                    after_revision TEXT CHECK (after_revision IS NULL OR length(after_revision) = 64),
                    checkpoint_operation_id TEXT,
                    merge_repository TEXT,
                    merge_session TEXT,
                    merge_generation INTEGER,
                    merge_left TEXT,
                    merge_revision TEXT,
                    outcome_json BLOB,
                    CHECK ((status = 'pending') = (outcome_json IS NULL))
                 ) STRICT;
                 CREATE UNIQUE INDEX one_pending_workspace_operation
                    ON operations(status) WHERE status = 'pending';
                 CREATE TABLE staged_paths (
                    operation_id TEXT NOT NULL,
                    path TEXT NOT NULL,
                    before_content BLOB,
                    after_content BLOB,
                    PRIMARY KEY(operation_id, path),
                    FOREIGN KEY(operation_id) REFERENCES operations(operation_id)
                 ) STRICT;",
            )
            .unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        connection
            .execute(
                "INSERT INTO workspace_state VALUES (1, 'ready', 'repository-1', 'change-1', ?1, 1)",
                ["c".repeat(64)],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO operations(operation_id, kind, request_digest, status, expected_generation, before_revision, after_revision, checkpoint_operation_id) VALUES ('undo-1', 'undo', ?1, 'pending', 1, ?2, ?3, 'checkpoint-1')",
                params!["e".repeat(64), "c".repeat(64), "b".repeat(64)],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO staged_paths VALUES ('undo-1', 'file.txt', ?1, NULL)",
                params![b"before".as_slice()],
            )
            .unwrap();
        drop(connection);

        let workspace = Workspace::open(&workspace_root).unwrap();
        let connection = workspace.connection().unwrap();
        validate_schema(&connection).unwrap();
        assert_eq!(
            read_staged(&connection, "undo-1").unwrap()["file.txt"].before,
            Some(Item {
                kind: EntryKind::Regular,
                content: b"before".to_vec()
            })
        );
        assert!(
            connection
                .execute(
                    "UPDATE staged_paths SET after_kind = 'bogus' WHERE operation_id = 'undo-1'",
                    []
                )
                .is_err()
        );
    }

    fn item(kind: EntryKind, content: &[u8]) -> Item {
        Item {
            kind,
            content: content.to_vec(),
        }
    }

    #[test]
    fn materialization_never_writes_or_removes_through_a_symlinked_parent() {
        let fixtures = TempDir::new().unwrap();
        let root = fixtures.path().join("workspace");
        let outside = fixtures.path().join("outside");
        fs::create_dir_all(root.join(METADATA_DIRECTORY)).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("x"), b"victim").unwrap();
        symlink(&outside, root.join("a")).unwrap();

        // Replacement of a/x would remove, then recreate, the outside file.
        let replace = BTreeMap::from([(
            "a/x".to_owned(),
            Stage {
                before: Some(item(EntryKind::Regular, b"victim")),
                after: Some(item(EntryKind::Regular, b"new")),
            },
        )]);
        assert!(matches!(
            apply_staged(&root, "op", &replace, true),
            Err(Error::Conflict(message)) if message.contains("symbolic link")
        ));
        // Creation of a/y must not land outside.
        let create = BTreeMap::from([(
            "a/y".to_owned(),
            Stage {
                before: None,
                after: Some(item(EntryKind::Executable, b"new")),
            },
        )]);
        assert!(matches!(
            apply_staged(&root, "op", &create, true),
            Err(Error::Conflict(_))
        ));
        assert_eq!(fs::read(outside.join("x")).unwrap(), b"victim");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }

    #[test]
    fn materialization_revalidates_symlink_targets_before_writing() {
        let fixtures = TempDir::new().unwrap();
        let root = fixtures.path().join("workspace");
        fs::create_dir_all(root.join(METADATA_DIRECTORY)).unwrap();
        let staged = BTreeMap::from([
            (
                "fine.txt".to_owned(),
                Stage {
                    before: None,
                    after: Some(item(EntryKind::Regular, b"fine")),
                },
            ),
            (
                "link".to_owned(),
                Stage {
                    before: None,
                    after: Some(item(EntryKind::Symlink, b"../escape")),
                },
            ),
        ]);
        assert!(matches!(
            apply_staged(&root, "op", &staged, true),
            Err(Error::Invalid(_))
        ));
        assert!(!root.join("fine.txt").exists());
        assert!(fs::symlink_metadata(root.join("link")).is_err());
    }

    #[test]
    fn probes_pass_on_a_supporting_filesystem_and_clean_up() {
        let fixtures = TempDir::new().unwrap();
        require_entry_support(
            fixtures.path(),
            [
                &item(EntryKind::Symlink, b"x"),
                &item(EntryKind::Executable, b"y"),
            ],
        )
        .unwrap();
        assert_eq!(fs::read_dir(fixtures.path()).unwrap().count(), 0);
    }

    #[test]
    fn probes_remove_stale_probe_entries_only() {
        let fixtures = TempDir::new().unwrap();
        let dir = fixtures.path();
        symlink("probe-target", dir.join(".agentscm-probe-link-1-2")).unwrap();
        fs::write(dir.join(".agentscm-probe-mode-1-2"), b"").unwrap();
        fs::write(dir.join("unrelated"), b"keep").unwrap();
        for probe in [probe_symlinks, probe_executable_bit] {
            symlink("probe-target", dir.join(".agentscm-probe-link-3-4")).unwrap();
            assert!(probe(dir).unwrap());
            let names: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, ["unrelated"]);
            symlink("probe-target", dir.join(".agentscm-probe-link-1-2")).unwrap();
            fs::write(dir.join(".agentscm-probe-mode-1-2"), b"").unwrap();
        }
    }

    #[test]
    fn executable_probe_needs_both_sides() {
        assert!(executable_bit_preserved(0o100600, 0o100700));
        // A mount reporting every file executable fails the clear check.
        assert!(!executable_bit_preserved(0o100755, 0o100755));
        // A mount that drops the bit fails the set check.
        assert!(!executable_bit_preserved(0o100644, 0o100644));
    }

    #[test]
    fn only_feature_errors_count_as_an_unsupported_filesystem() {
        for code in [libc::EPERM, libc::ENOTSUP, libc::ENOSYS] {
            assert!(is_unsupported(&std::io::Error::from_raw_os_error(code)));
        }
        for code in [libc::EACCES, libc::ENOENT, libc::EROFS, libc::ENOSPC] {
            assert!(!is_unsupported(&std::io::Error::from_raw_os_error(code)));
        }
    }
}
