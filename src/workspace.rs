use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::context::ChangeId;
use crate::model::{EntryChange, EntryKind};
use crate::store::{Engine, SourceReader, digest, validate_id, validate_symlink_target};
use crate::sync;
use crate::{Error, Result};

mod git_config;
mod glob;
mod scope;
mod selection;
mod snapshots;

use scope::Scope;
pub use selection::Rule;
use selection::{Classified, Selection, Selector, Settings, Tracked};
pub use snapshots::{
    CapturePause, CaptureResult, CaptureSkip, CheckEvidenceCoverage, CheckLinkEntry,
    CheckLinkRequest, CheckLinkResult, SnapshotEntry, SnapshotTrigger, TimelineCoverage,
    TimelineEntry, TimelinePage,
};
pub(crate) use snapshots::{TIMELINE_LEASE_FILE, TIMELINE_PROBES_DIRECTORY};

pub const METADATA_DIRECTORY: &str = crate::local_recovery::WORKSPACE_METADATA_DIRECTORY;

const DATABASE_FILE: &str = "state.sqlite";
const APPLICATION_ID: i64 = 0x4153_574b;
const SCHEMA_VERSION: i64 = 5;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const LIST_PAGE_SIZE: usize = 4_096;
const MAX_ENTRIES: usize = 100_000;
/// Bound on everything read from directories, selected or not.
const MAX_VISITED_ENTRIES: usize = 1_000_000;
const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 128 * 1024 * 1024;
const MAX_DEPTH: usize = 128;
const MAX_CHECKPOINT_CHANGES: usize = 4_096;
const MAX_OPERATION_ID_BYTES: usize = 128;
const MAX_SCOPE_JSON_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkspaceState {
    pub change: ChangeId,
    pub base_revision: String,
    pub generation: u64,
    pub ready: bool,
    /// Canonical effective path roots for a sparse workspace. `None` means the
    /// legacy whole-tree workspace profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
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
    Expand,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Status {
    pub base_revision: String,
    pub generation: u64,
    pub changes: Vec<WorkspaceChange>,
    pub complete: bool,
    /// Entries left out by exclusion rules. A directory counts once; its descendants, which are
    /// never visited, are not counted.
    pub excluded_total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingOperation>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Preview {
    pub base_revision: String,
    pub generation: u64,
    /// Captured files and symbolic links, by path bytes, up to the requested limit.
    pub included: Vec<IncludedPath>,
    /// Paths left out by an exclusion rule, by path bytes, up to the requested limit.
    pub excluded: Vec<ExcludedPath>,
    pub included_total: usize,
    pub excluded_total: usize,
    /// False when a list was cut short or the working tree could not be fully scanned.
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingOperation>,
}

/// Result of capturing explicitly selected paths from an existing working tree.
/// Paths outside `selected` inherit the immutable base revision.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SelectedCapture {
    pub revision: String,
    pub changed_paths: Vec<String>,
    pub excluded_total: usize,
    pub fingerprint: String,
}

/// Capture selected working-tree paths with the workspace selector and limits, without
/// materializing or mutating the tree. The caller is responsible for fencing ambient Git state.
pub(crate) fn validate_attachment_paths(selected: &[String]) -> Result<()> {
    Scope::from_requested(selected)?;
    Ok(())
}

pub(crate) struct AttachedScan<'a> {
    pub root: &'a Path,
    pub probe: &'a Path,
    pub git: &'a Path,
    pub tracked: &'a [String],
}

pub(crate) fn capture_selected_worktree(
    engine: &Engine,
    scan: AttachedScan<'_>,
    base_revision: &str,
    selected: &[String],
) -> Result<SelectedCapture> {
    let scope = Scope::from_requested(selected)?;
    if fs::metadata(scan.root)?.dev() != fs::metadata(scan.probe)?.dev() {
        return Err(Error::Invalid(
            "attachment probe and worktree must be on the same filesystem".into(),
        ));
    }
    require_executable_support(scan.probe)?;
    let settings = git_config::selected_settings(scan.root, scan.git)?;
    let base = require_complete_snapshot(source_snapshot(engine, base_revision)?)?;
    let tracked = Tracked::from_paths(
        scan.tracked
            .iter()
            .map(String::as_str)
            .chain(base.files.keys().map(String::as_str)),
    );
    let (working, report) = require_complete_scan(scan_with_settings_scoped(
        scan.root,
        &tracked,
        &settings,
        scope.as_ref(),
        true,
    )?)?;
    let includes = |path: &str| scope.as_ref().is_none_or(|s| s.includes(path));
    let changes: Vec<_> = staged_between(&base.files, &working.files)
        .into_iter()
        .filter(|(path, _)| includes(path))
        .map(|(path, stage)| EntryChange {
            path,
            kind: stage
                .after
                .as_ref()
                .map_or(EntryKind::Regular, |item| item.kind),
            content: stage.after.map(|item| item.content),
        })
        .collect();
    let changed_paths = changes.iter().map(|c| c.path.clone()).collect();
    let mut fingerprint = Sha256::new();
    fingerprint.update(serde_json::to_vec(&(
        selected,
        scan.tracked,
        settings.case_insensitive,
        report.selection_digest,
    ))?);
    for (path, item) in &working.files {
        fingerprint.update(serde_json::to_vec(&(
            path,
            item.kind,
            digest(&item.content),
        ))?);
    }
    let revision = if changes.is_empty() {
        base_revision.to_owned()
    } else {
        engine.checkpoint_entries(Some(base_revision), &changes)?
    };
    Ok(SelectedCapture {
        revision,
        changed_paths,
        excluded_total: report.excluded_total,
        fingerprint: format!("{:x}", fingerprint.finalize()),
    })
}

/// Conservative three-way overlay using the caller's recorded, verified base.
/// A same-path divergence blocks; no unrelated native ancestry is invented.
pub(crate) fn merge_attached(
    engine: &Engine,
    base: &str,
    left: &str,
    right: &str,
) -> Result<String> {
    if left == right || base == right {
        return Ok(left.into());
    }
    if base == left {
        return Ok(right.into());
    }
    let base = require_complete_snapshot(source_snapshot(engine, base)?)?;
    let ours = require_complete_snapshot(source_snapshot(engine, left)?)?;
    let theirs = require_complete_snapshot(source_snapshot(engine, right)?)?;
    let mut changes = Vec::new();
    for (path, stage) in staged_between(&base.files, &theirs.files) {
        let current = ours.files.get(&path);
        if current == stage.after.as_ref() {
            continue;
        }
        if current != base.files.get(&path) {
            return Err(Error::Conflict(format!(
                "attached refresh conflicts at {path}; no attachment state changed; automatic conflict resolution is unsupported in phase 1"
            )));
        }
        changes.push(EntryChange {
            path,
            kind: stage.after.as_ref().map_or(EntryKind::Regular, |i| i.kind),
            content: stage.after.map(|i| i.content),
        });
    }
    if changes.is_empty() {
        Ok(left.into())
    } else {
        engine.checkpoint_entries(Some(left), &changes)
    }
}

/// Confirm every native-only candidate difference is exactly supplied by the recorded target.
/// This lets an attached refresh adopt a Git merge resolution without dropping unrelated state.
pub(crate) fn attached_candidate_is_target_derived(
    engine: &Engine,
    source: &str,
    candidate: &str,
    target: &str,
) -> Result<bool> {
    let source = require_complete_snapshot(source_snapshot(engine, source)?)?;
    let candidate = require_complete_snapshot(source_snapshot(engine, candidate)?)?;
    let target = require_complete_snapshot(source_snapshot(engine, target)?)?;
    Ok(staged_between(&source.files, &candidate.files)
        .into_iter()
        .all(|(path, stage)| target.files.get(&path) == stage.after.as_ref()))
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
    Expanded { generation: u64 },
    Recovered { generation: u64 },
    Cancelled { generation: u64 },
    Blocked { paths: Vec<String> },
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePathState {
    Materialized,
    MissingLocal,
    Unmaterialized,
    OutsideScopeLocal,
    Nonexistent,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    _access: crate::local_recovery::AccessGuard,
    read_only: bool,
}

impl Workspace {
    pub fn create(
        engine: &Engine,
        root: impl AsRef<Path>,
        change: ChangeId,
        revision: &str,
    ) -> Result<Self> {
        Self::create_inner(engine, root, change, revision, None)
    }

    pub fn create_scoped(
        engine: &Engine,
        root: impl AsRef<Path>,
        change: ChangeId,
        revision: &str,
        paths: &[String],
    ) -> Result<Self> {
        let requested = Scope::from_requested(paths)?;
        Self::create_inner(engine, root, change, revision, requested)
    }

    fn create_inner(
        engine: &Engine,
        root: impl AsRef<Path>,
        change: ChangeId,
        revision: &str,
        requested_scope: Option<Scope>,
    ) -> Result<Self> {
        change.validate()?;
        validate_id(revision)?;
        let root = absolute_path(root.as_ref())?;
        crate::local_recovery::check_before_create(&root.join(METADATA_DIRECTORY), true)?;
        let parent = root
            .parent()
            .ok_or_else(|| Error::Invalid("workspace root has no parent".to_owned()))?;
        require_directory(parent, "workspace parent")?;
        let inventory = require_complete_inventory(source_inventory(engine, revision)?)?;
        let scope = requested_scope
            .map(|scope| {
                scope.with_selection_dependencies(inventory.entries.keys().map(String::as_str))
            })
            .transpose()?;
        let snapshot = require_complete_snapshot(snapshot_from_inventory(
            engine,
            revision,
            &inventory,
            scope.as_ref(),
        )?)?;
        if scope.is_some() {
            require_instruction_targets(&snapshot)?;
        }
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
        write_scope(&connection, scope.as_ref())?;
        drop(connection);

        let workspace = Self {
            root,
            _access: access,
            read_only: false,
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
            read_only: false,
        })
    }

    /// Inspect an existing v4 or current-schema workspace without migrating it, creating files
    /// or changing the journal mode. `state` and `status` work; mutating calls fail.
    pub fn open_read_only(root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute_path(root.as_ref())?;
        require_directory(&root, "workspace root")?;
        let metadata = root.join(METADATA_DIRECTORY);
        require_directory(&metadata, "workspace metadata")?;
        let access = crate::local_recovery::AccessGuard::workspace_existing(&metadata)?;
        let workspace = Self {
            root,
            _access: access,
            read_only: true,
        };
        read_state(&workspace.connection()?)?;
        Ok(workspace)
    }

    pub fn state(&self) -> Result<WorkspaceState> {
        let connection = self.connection()?;
        state_with_pending(&connection)
    }

    /// Resolve a creation request to effective roots without creating a workspace.
    /// Empty paths or a sole '.' select the full revision.
    pub fn scope_for_revision(
        engine: &Engine,
        revision: &str,
        paths: &[String],
    ) -> Result<Option<Vec<String>>> {
        validate_id(revision)?;
        engine.load_revision(revision)?;
        let requested = if paths.is_empty() {
            None
        } else {
            Scope::from_requested(paths)?
        };
        if let Some(requested) = requested {
            let inventory = require_complete_inventory(source_inventory(engine, revision)?)?;
            Ok(Some(
                requested
                    .with_selection_dependencies(inventory.entries.keys().map(String::as_str))?
                    .roots()
                    .to_vec(),
            ))
        } else {
            Ok(None)
        }
    }

    /// Resolve and fully qualify a creation projection without reserving a
    /// destination. The returned roots are canonical and selected blobs have
    /// passed the same aggregate limits used by workspace creation.
    pub fn preflight_creation_scope(
        engine: &Engine,
        revision: &str,
        paths: &[String],
    ) -> Result<Option<Vec<String>>> {
        validate_id(revision)?;
        engine.load_revision(revision)?;
        let requested = if paths.is_empty() {
            None
        } else {
            Scope::from_requested(paths)?
        };
        let inventory = require_complete_inventory(source_inventory(engine, revision)?)?;
        let scope = requested
            .map(|scope| {
                scope.with_selection_dependencies(inventory.entries.keys().map(String::as_str))
            })
            .transpose()?;
        let snapshot = require_complete_snapshot(snapshot_from_inventory(
            engine,
            revision,
            &inventory,
            scope.as_ref(),
        )?)?;
        if scope.is_some() {
            require_instruction_targets(&snapshot)?;
        }
        Ok(scope.map(|scope| scope.roots().to_vec()))
    }

    /// Compare the persisted projection with a creation request, including exact-base
    /// selection dependencies. This is used to fence interrupted task creation.
    pub fn scope_matches(&self, engine: &Engine, paths: &[String]) -> Result<bool> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let state = read_state(&tx)?;
        let actual = read_scope(&tx)?.map(|scope| scope.roots().to_vec());
        Ok(actual == Self::scope_for_revision(engine, &state.base_revision, paths)?)
    }

    pub fn path_state(&self, engine: &Engine, path: &str) -> Result<WorkspacePathState> {
        validate_workspace_path(path)?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let state = read_state(&tx)?;
        let scope = read_scope(&tx)?;
        let source_entry = SourceReader::new(engine).stat_file(&state.base_revision, path)?;
        reject_symlink_ancestors(&self.root, path)?;
        let local = match fs::symlink_metadata(self.root.join(path)) {
            Ok(_) => true,
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                false
            }
            Err(error) => return Err(Error::Io(error)),
        };
        if scope.as_ref().is_some_and(|scope| !scope.includes(path)) {
            return Ok(if local {
                WorkspacePathState::OutsideScopeLocal
            } else if source_entry.is_some() {
                WorkspacePathState::Unmaterialized
            } else {
                WorkspacePathState::Nonexistent
            });
        }
        if local {
            Ok(WorkspacePathState::Materialized)
        } else {
            Ok(if source_entry.is_some() {
                WorkspacePathState::MissingLocal
            } else {
                WorkspacePathState::Nonexistent
            })
        }
    }

    pub fn expand(
        &mut self,
        engine: &Engine,
        operation_id: &str,
        expected_generation: u64,
        paths: &[String],
    ) -> Result<OperationOutcome> {
        validate_operation_id(operation_id)?;
        let additions = Scope::from_requested(paths)?;
        let request_digest = scope_expansion_digest(
            operation_id,
            expected_generation,
            additions.as_ref().map(Scope::roots),
        );
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = matching_scope_expansion(&tx, operation_id, &request_digest)? {
            let pending = existing.is_none();
            drop(tx);
            return if pending {
                self.finish_expansion(engine, operation_id, RecoveryChoice::Complete)
            } else {
                self.cleanup_expansion(operation_id)?;
                Ok(existing.expect("completed outcome"))
            };
        }
        ensure_no_pending(&tx)?;
        let state = read_state(&tx)?;
        require_ready_generation(&state, expected_generation)?;
        let current = read_scope(&tx)?.ok_or_else(|| {
            Error::Invalid("a whole-tree workspace cannot expand its scope".into())
        })?;
        let requested = match additions {
            Some(additions) => current.expanded(additions.roots())?,
            None => None,
        };
        let inventory =
            require_complete_inventory(source_inventory(engine, &state.base_revision)?)?;
        let target = requested
            .map(|scope| {
                scope.with_selection_dependencies(inventory.entries.keys().map(String::as_str))
            })
            .transpose()?;
        if target.as_ref().is_some_and(|scope| scope == &current) {
            return Err(Error::Invalid("scope expansion adds no paths".into()));
        }
        // Validate the complete projected immutable base against the workspace
        // limits before journaling. Only newly scoped entries are staged, so
        // dirty edits and deletions under the old scope remain untouched.
        let target_snapshot = require_complete_snapshot(snapshot_from_inventory(
            engine,
            &state.base_revision,
            &inventory,
            target.as_ref(),
        )?)?;
        if target.is_some() {
            require_instruction_targets(&target_snapshot)?;
        }
        let staged: BTreeMap<String, Stage> = target_snapshot
            .files
            .iter()
            .filter(|(path, _)| !current.includes(path))
            .map(|(path, item)| {
                (
                    path.clone(),
                    Stage {
                        before: None,
                        after: Some(item.clone()),
                    },
                )
            })
            .collect();
        let target_tracked = Tracked::from_paths(staged.keys().map(String::as_str));
        let (working, report) = require_complete_scan(scan_workspace_full_scoped(
            &self.root,
            &target_tracked,
            target.as_ref(),
        )?)?;
        let mut occupants = newly_scoped_occupants(&working, &current, target.as_ref());
        occupants.extend(excluded_occupants(&report, &staged, &BTreeMap::new()));
        occupants.sort();
        occupants.dedup();
        if !occupants.is_empty() {
            return Ok(OperationOutcome::Blocked { paths: occupants });
        }
        require_staged_support(&self.root, &staged)?;
        insert_scope_expansion(
            &tx,
            operation_id,
            &request_digest,
            expected_generation,
            &current,
            target.as_ref(),
            &staged,
        )?;
        tx.commit()?;
        self.finish_expansion(engine, operation_id, RecoveryChoice::Complete)
    }

    pub fn status(&self, engine: &Engine, limit: usize) -> Result<Status> {
        if limit == 0 {
            return Err(Error::Limit(
                "workspace status limit must be positive".to_owned(),
            ));
        }
        let mut connection = self.connection()?;
        let behavior = if self.read_only {
            TransactionBehavior::Deferred
        } else {
            TransactionBehavior::Immediate
        };
        let tx = connection.transaction_with_behavior(behavior)?;
        let state = state_with_pending(&tx)?;
        let scope = read_scope(&tx)?;
        let status = if !state.ready {
            Status {
                base_revision: state.base_revision,
                generation: state.generation,
                changes: Vec::new(),
                complete: false,
                excluded_total: 0,
                pending: state.pending,
            }
        } else if let Some(base) =
            source_digests_scoped(engine, &state.base_revision, scope.as_ref())?
        {
            let tracked = Tracked::from_paths(base.files.keys().map(String::as_str));
            if let Some((working, report)) =
                scan_workspace_digests_scoped(&self.root, &tracked, scope.as_ref())?
            {
                let mut changes = changes_between(&base.files, &working.files);
                let complete = changes.len() <= limit;
                changes.truncate(limit);
                Status {
                    base_revision: state.base_revision,
                    generation: state.generation,
                    changes,
                    complete,
                    excluded_total: report.excluded_total,
                    pending: state.pending,
                }
            } else {
                Status {
                    base_revision: state.base_revision,
                    generation: state.generation,
                    changes: Vec::new(),
                    complete: false,
                    excluded_total: 0,
                    pending: state.pending,
                }
            }
        } else {
            Status {
                base_revision: state.base_revision,
                generation: state.generation,
                changes: Vec::new(),
                complete: false,
                excluded_total: 0,
                pending: state.pending,
            }
        };
        tx.commit()?;
        Ok(status)
    }

    /// List what the next status or checkpoint would capture and what the exclusion rules leave
    /// out, with the rule behind each exclusion. Uses the same selector and scan as `status`; it
    /// returns paths only, never file contents.
    pub fn preview(&self, engine: &Engine, limit: usize) -> Result<Preview> {
        if limit == 0 {
            return Err(Error::Limit(
                "workspace preview limit must be positive".to_owned(),
            ));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = state_with_pending(&tx)?;
        let scope = read_scope(&tx)?;
        let mut preview = Preview {
            base_revision: state.base_revision.clone(),
            generation: state.generation,
            included: Vec::new(),
            excluded: Vec::new(),
            included_total: 0,
            excluded_total: 0,
            complete: false,
            pending: state.pending,
        };
        if state.ready
            && let Some(base) = source_digests_scoped(engine, &state.base_revision, scope.as_ref())?
        {
            let tracked = Tracked::from_paths(base.files.keys().map(String::as_str));
            if let Some((working, mut report)) =
                scan_workspace_digests_scoped(&self.root, &tracked, scope.as_ref())?
            {
                preview.included_total = working.files.len();
                preview.excluded_total = report.excluded_total;
                preview.complete = working.files.len() <= limit
                    && report.excluded_total <= limit
                    && report.excluded_total == report.excluded.len()
                    && !report.included_by_rule_truncated;
                preview.included = working
                    .files
                    .keys()
                    .take(limit)
                    .map(|path| IncludedPath {
                        rule: report.included_by_rule.remove(path),
                        path: path.clone(),
                    })
                    .collect();
                report
                    .excluded
                    .sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
                report.excluded.truncate(limit);
                preview.excluded = report.excluded;
            }
        }
        tx.commit()?;
        Ok(preview)
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
        let scope = read_scope(&tx)?;
        let base = require_complete_snapshot(source_snapshot_scoped(
            engine,
            &state.base_revision,
            scope.as_ref(),
        )?)?;
        let tracked = Tracked::from_paths(base.files.keys().map(String::as_str));
        let working = require_complete_snapshot(scan_workspace_scoped(
            &self.root,
            &tracked,
            scope.as_ref(),
        )?)?;
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
        #[cfg(all(test, unix))]
        tests::checkpoint_boundary(&self.root)?;
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
        let scope = read_scope(&tx)?;
        let (before_revision, after_revision) = completed_checkpoint(&tx, checkpoint_operation_id)?;
        if after_revision != state.base_revision {
            return Err(Error::Conflict(format!(
                "checkpoint operation {checkpoint_operation_id} is not the current workspace base"
            )));
        }
        let current = require_complete_snapshot(source_snapshot_scoped(
            engine,
            &after_revision,
            scope.as_ref(),
        )?)?;
        let target = require_complete_snapshot(source_snapshot_scoped(
            engine,
            &before_revision,
            scope.as_ref(),
        )?)?;
        let tracked = Tracked::from_paths(
            current
                .files
                .keys()
                .chain(target.files.keys())
                .map(String::as_str),
        );
        let (working, report) = require_complete_scan(scan_workspace_full_scoped(
            &self.root,
            &tracked,
            scope.as_ref(),
        )?)?;
        let staged = staged_between(&current.files, &target.files);
        let mut blocked = blocked_paths(&working, &staged, &self.root, operation_id, true)?;
        blocked.extend(excluded_occupants(&report, &staged, &current.files));
        blocked.sort();
        blocked.dedup();
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
        let scope = read_scope(&tx)?;
        validate_completion_binding(&state, completion)?;
        require_scope_selection_dependencies(engine, &completion.revision, scope.as_ref())?;
        next_generation(state.generation)?;
        qualify_merge_candidate(engine, completion)?;

        let current = require_complete_snapshot(source_snapshot_scoped(
            engine,
            &state.base_revision,
            scope.as_ref(),
        )?)?;
        let target = require_complete_snapshot(source_snapshot_scoped(
            engine,
            &completion.revision,
            scope.as_ref(),
        )?)?;
        let tracked = Tracked::from_paths(
            current
                .files
                .keys()
                .chain(target.files.keys())
                .map(String::as_str),
        );
        let (working, report) = require_complete_scan(scan_workspace_full_scoped(
            &self.root,
            &tracked,
            scope.as_ref(),
        )?)?;
        let staged = staged_between(&current.files, &target.files);
        let mut blocked = blocked_paths(&working, &staged, &self.root, operation_id, true)?;
        blocked.extend(excluded_occupants(&report, &staged, &current.files));
        blocked.sort();
        blocked.dedup();
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
            OperationKind::Expand => {
                drop(tx);
                self.finish_expansion(engine, &pending.operation_id, choice)
            }
        }
    }

    fn connection(&self) -> Result<Connection> {
        let path = self.root.join(METADATA_DIRECTORY).join(DATABASE_FILE);
        if self.read_only {
            let connection = open_connection_read_only(&path)?;
            validate_schema(&connection)?;
            return Ok(connection);
        }
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
        let scope = read_scope(&connection)?;
        let snapshot = require_complete_snapshot(source_snapshot_scoped(
            engine,
            &state.base_revision,
            scope.as_ref(),
        )?)?;
        self.finish_initialization_snapshot(snapshot)
    }

    fn finish_initialization_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = read_state(&tx)?;
        if state.ready {
            return Ok(());
        }
        let scope = read_scope(&tx)?;
        let tracked = Tracked::from_paths(snapshot.files.keys().map(String::as_str));
        let working = require_complete_snapshot(scan_workspace_scoped(
            &self.root,
            &tracked,
            scope.as_ref(),
        )?)?;
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

    fn finish_expansion(
        &self,
        engine: &Engine,
        operation_id: &str,
        choice: RecoveryChoice,
    ) -> Result<OperationOutcome> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expansion = read_scope_expansion(&tx, operation_id)?
            .ok_or_else(|| Error::NotFound(format!("workspace scope expansion {operation_id}")))?;
        if let Some(outcome) = expansion.outcome {
            drop(tx);
            self.cleanup_expansion(operation_id)?;
            return Ok(outcome);
        }
        let state = read_state(&tx)?;
        require_ready_generation(&state, expansion.expected_generation)?;
        let current = read_scope(&tx)?.ok_or_else(|| {
            Error::Invalid("pending scope expansion belongs to a whole-tree workspace".into())
        })?;
        if current.roots() != expansion.before.roots() {
            return Err(Error::Conflict(
                "workspace scope changed while expansion was pending".into(),
            ));
        }
        let staged = read_scope_expansion_staged(&tx, operation_id)?;
        let target = expansion.after.as_ref();
        if choice == RecoveryChoice::Complete {
            require_scope_selection_dependencies(engine, &state.base_revision, target)?;
        }
        let tracked = Tracked::from_paths(staged.keys().map(String::as_str));
        let (working, report) =
            require_complete_scan(scan_workspace_full_scoped(&self.root, &tracked, target)?)?;
        let desired_after = choice == RecoveryChoice::Complete;
        let mut blocked =
            blocked_paths(&working, &staged, &self.root, operation_id, desired_after)?;
        blocked.extend(
            newly_scoped_occupants(&working, &current, target)
                .into_iter()
                .filter(|path| !staged.contains_key(path)),
        );
        blocked.extend(excluded_occupants(&report, &staged, &BTreeMap::new()));
        blocked.extend(unproven_expansion_targets(
            &self.root,
            operation_id,
            &staged,
        )?);
        blocked.sort();
        blocked.dedup();
        if !blocked.is_empty() {
            return Ok(OperationOutcome::Blocked { paths: blocked });
        }
        apply_expansion_staged(&self.root, operation_id, &staged, desired_after)?;
        if choice == RecoveryChoice::RollBack {
            let outcome = OperationOutcome::Cancelled {
                generation: state.generation,
            };
            complete_scope_expansion(&tx, operation_id, "cancelled", &outcome)?;
            tx.commit()?;
            self.cleanup_expansion(operation_id)?;
            return Ok(outcome);
        }
        let generation = next_generation(state.generation)?;
        tx.execute("DELETE FROM workspace_scope WHERE singleton = 1", [])?;
        write_scope(&tx, target)?;
        if tx.execute(
            "UPDATE workspace_state SET generation = ?1 WHERE singleton = 1 AND generation = ?2",
            params![
                sql_i64("workspace generation", generation)?,
                sql_i64("workspace generation", state.generation)?
            ],
        )? != 1
        {
            return Err(Error::Conflict(
                "workspace changed while scope expansion was completing".into(),
            ));
        }
        let outcome = OperationOutcome::Expanded { generation };
        complete_scope_expansion(&tx, operation_id, "completed", &outcome)?;
        tx.commit()?;
        self.cleanup_expansion(operation_id)?;
        Ok(outcome)
    }

    fn cleanup_expansion(&self, operation_id: &str) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expansion = read_scope_expansion(&tx, operation_id)?
            .ok_or_else(|| Error::NotFound(format!("workspace scope expansion {operation_id}")))?;
        if expansion.outcome.is_none() {
            return Err(Error::Conflict(format!(
                "workspace scope expansion {operation_id} is still pending"
            )));
        }
        let staged = read_scope_expansion_staged(&tx, operation_id)?;
        cleanup_expansion_witnesses(&self.root, operation_id, &staged).map_err(|error| {
            Error::Conflict(format!("workspace expansion has a durable outcome but metadata cleanup failed: {error}; inspect state and retry the same operation"))
        })?;
        tx.execute(
            "DELETE FROM scope_expansion_paths WHERE operation_id = ?1",
            [operation_id],
        )?;
        tx.commit()?;
        Ok(())
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
        let scope = read_scope(&tx)?;
        require_ready_generation(&state, operation.expected_generation)?;
        validate_stored_completion_binding(&state, operation_id, &operation, &completion)?;
        let next = if choice == RecoveryChoice::Complete {
            let generation = next_generation(state.generation)?;
            require_scope_selection_dependencies(engine, &completion.revision, scope.as_ref())?;
            qualify_stored_merge_candidate(engine, &completion)?;
            Some(generation)
        } else {
            None
        };

        let staged = read_staged_allow_empty(&tx, operation_id)?;
        let tracked = Tracked::from_paths(staged.keys().map(String::as_str));
        let working = require_complete_snapshot(scan_workspace_scoped(
            &self.root,
            &tracked,
            scope.as_ref(),
        )?)?;
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

#[derive(Clone, Debug, PartialEq, Eq)]
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

/// One tracked workspace entry. For a symlink `content` is the link target. In a digest-only
/// snapshot (status and preview) `content` is the SHA-256 of those bytes instead.
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

#[derive(Clone, Debug)]
struct SourceInventory {
    entries: BTreeMap<String, EntryKind>,
    directories: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct StoredScopeExpansion {
    expected_generation: u64,
    before: Scope,
    after: Option<Scope>,
    outcome: Option<OperationOutcome>,
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

fn open_connection_read_only(path: &Path) -> Result<Connection> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(Error::Invalid(
            "workspace database is not a non-symlink regular file".to_owned(),
        ));
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
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
         ) STRICT;
         CREATE TABLE workspace_scope (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            roots_json BLOB NOT NULL
         ) STRICT;
         CREATE TABLE scope_expansions (
            operation_id TEXT PRIMARY KEY,
            request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
            status TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'cancelled')),
            expected_generation INTEGER NOT NULL CHECK (expected_generation >= 0),
            before_roots_json BLOB NOT NULL,
            after_roots_json BLOB NOT NULL,
            outcome_json BLOB,
            CHECK ((status = 'pending') = (outcome_json IS NULL))
         ) STRICT;
         CREATE UNIQUE INDEX one_pending_scope_expansion
            ON scope_expansions(status) WHERE status = 'pending';
         CREATE TABLE scope_expansion_paths (
            operation_id TEXT NOT NULL,
            path TEXT NOT NULL,
            after_content BLOB NOT NULL,
            after_kind TEXT NOT NULL CHECK (after_kind IN ('regular', 'executable', 'symlink')),
            PRIMARY KEY(operation_id, path),
            FOREIGN KEY(operation_id) REFERENCES scope_expansions(operation_id)
         ) STRICT;
         CREATE TABLE timeline_entries (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK (sequence > 0),
            kind TEXT NOT NULL CHECK (kind IN ('snapshot', 'check_link')),
            trigger TEXT NOT NULL CHECK (trigger IN ('manual', 'turn_end', 'check_start', 'mcp_explicit')),
            recorded_at INTEGER NOT NULL CHECK (recorded_at >= 0),
            source_revision TEXT NOT NULL CHECK (length(source_revision) = 64),
            base_revision TEXT NOT NULL CHECK (length(base_revision) = 64),
            generation INTEGER NOT NULL CHECK (generation >= 0),
            scope_json BLOB NOT NULL,
            changed_paths INTEGER CHECK (changed_paths IS NULL OR changed_paths >= 0),
            admitted_bytes INTEGER CHECK (admitted_bytes IS NULL OR admitted_bytes >= 0),
            tested_revision TEXT CHECK (tested_revision IS NULL OR length(tested_revision) = 64),
            change_repository TEXT,
            change_id TEXT,
            change_version INTEGER CHECK (change_version IS NULL OR change_version >= 0),
            context_digest TEXT CHECK (context_digest IS NULL OR length(context_digest) = 64),
            policy_epoch INTEGER CHECK (policy_epoch IS NULL OR policy_epoch >= 0),
            target TEXT,
            target_revision TEXT CHECK (target_revision IS NULL OR length(target_revision) = 64),
            target_version INTEGER CHECK (target_version IS NULL OR target_version >= 0),
            requirement_digest TEXT CHECK (requirement_digest IS NULL OR length(requirement_digest) = 64),
            run_id TEXT CHECK (run_id IS NULL OR length(run_id) BETWEEN 1 AND 64),
            CHECK (
                (kind = 'snapshot' AND changed_paths IS NOT NULL AND admitted_bytes IS NOT NULL
                    AND tested_revision IS NULL AND change_repository IS NULL AND change_id IS NULL
                    AND change_version IS NULL AND context_digest IS NULL AND policy_epoch IS NULL
                    AND target IS NULL AND target_revision IS NULL AND target_version IS NULL
                    AND requirement_digest IS NULL AND run_id IS NULL)
                OR
                (kind = 'check_link' AND trigger = 'check_start' AND changed_paths IS NULL
                    AND admitted_bytes IS NULL AND tested_revision IS NOT NULL
                    AND change_repository IS NOT NULL AND change_id IS NOT NULL
                    AND change_version IS NOT NULL AND context_digest IS NOT NULL
                    AND policy_epoch IS NOT NULL AND target IS NOT NULL
                    AND ((target_revision IS NULL) = (target_version IS NULL))
                    AND requirement_digest IS NOT NULL AND run_id IS NOT NULL)
            )
         ) STRICT;
         CREATE UNIQUE INDEX timeline_check_link_identity
            ON timeline_entries(run_id, context_digest, requirement_digest)
            WHERE kind = 'check_link';",
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
    if application_id != APPLICATION_ID || !matches!(schema_version, 1..=4) {
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
    if locked_application_id != APPLICATION_ID || !matches!(locked_schema_version, 1..=4) {
        return Err(Error::Invalid(format!(
            "workspace schema changed during migration ({locked_application_id}/{locked_schema_version})"
        )));
    }
    if locked_schema_version == 1 {
        migrate_v1_to_v2(&tx)?;
    }
    if locked_schema_version <= 2 {
        // v3 adds the entry kind of each staged side; existing rows are regular files.
        tx.execute_batch(
            "ALTER TABLE staged_paths ADD COLUMN before_kind TEXT NOT NULL DEFAULT 'regular'
                CHECK (before_kind IN ('regular', 'executable', 'symlink'));
             ALTER TABLE staged_paths ADD COLUMN after_kind TEXT NOT NULL DEFAULT 'regular'
                CHECK (after_kind IN ('regular', 'executable', 'symlink'));",
        )?;
    }
    if locked_schema_version <= 3 {
        // v4 adds optional sparse scope and a separate durable expansion journal.
        tx.execute_batch(
            "CREATE TABLE workspace_scope (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            roots_json BLOB NOT NULL
         ) STRICT;
         CREATE TABLE scope_expansions (
            operation_id TEXT PRIMARY KEY,
            request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
            status TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'cancelled')),
            expected_generation INTEGER NOT NULL CHECK (expected_generation >= 0),
            before_roots_json BLOB NOT NULL,
            after_roots_json BLOB NOT NULL,
            outcome_json BLOB,
            CHECK ((status = 'pending') = (outcome_json IS NULL))
         ) STRICT;
         CREATE UNIQUE INDEX one_pending_scope_expansion
            ON scope_expansions(status) WHERE status = 'pending';
         CREATE TABLE scope_expansion_paths (
            operation_id TEXT NOT NULL,
            path TEXT NOT NULL,
            after_content BLOB NOT NULL,
            after_kind TEXT NOT NULL CHECK (after_kind IN ('regular', 'executable', 'symlink')),
            PRIMARY KEY(operation_id, path),
            FOREIGN KEY(operation_id) REFERENCES scope_expansions(operation_id)
         ) STRICT;",
        )?;
    }
    // v5 adds a bounded observational timeline separate from the workspace operation journal.
    tx.execute_batch(
        "CREATE TABLE timeline_entries (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK (sequence > 0),
            kind TEXT NOT NULL CHECK (kind IN ('snapshot', 'check_link')),
            trigger TEXT NOT NULL CHECK (trigger IN ('manual', 'turn_end', 'check_start', 'mcp_explicit')),
            recorded_at INTEGER NOT NULL CHECK (recorded_at >= 0),
            source_revision TEXT NOT NULL CHECK (length(source_revision) = 64),
            base_revision TEXT NOT NULL CHECK (length(base_revision) = 64),
            generation INTEGER NOT NULL CHECK (generation >= 0),
            scope_json BLOB NOT NULL,
            changed_paths INTEGER CHECK (changed_paths IS NULL OR changed_paths >= 0),
            admitted_bytes INTEGER CHECK (admitted_bytes IS NULL OR admitted_bytes >= 0),
            tested_revision TEXT CHECK (tested_revision IS NULL OR length(tested_revision) = 64),
            change_repository TEXT,
            change_id TEXT,
            change_version INTEGER CHECK (change_version IS NULL OR change_version >= 0),
            context_digest TEXT CHECK (context_digest IS NULL OR length(context_digest) = 64),
            policy_epoch INTEGER CHECK (policy_epoch IS NULL OR policy_epoch >= 0),
            target TEXT,
            target_revision TEXT CHECK (target_revision IS NULL OR length(target_revision) = 64),
            target_version INTEGER CHECK (target_version IS NULL OR target_version >= 0),
            requirement_digest TEXT CHECK (requirement_digest IS NULL OR length(requirement_digest) = 64),
            run_id TEXT CHECK (run_id IS NULL OR length(run_id) BETWEEN 1 AND 64),
            CHECK (
                (kind = 'snapshot' AND changed_paths IS NOT NULL AND admitted_bytes IS NOT NULL
                    AND tested_revision IS NULL AND change_repository IS NULL AND change_id IS NULL
                    AND change_version IS NULL AND context_digest IS NULL AND policy_epoch IS NULL
                    AND target IS NULL AND target_revision IS NULL AND target_version IS NULL
                    AND requirement_digest IS NULL AND run_id IS NULL)
                OR
                (kind = 'check_link' AND trigger = 'check_start' AND changed_paths IS NULL
                    AND admitted_bytes IS NULL AND tested_revision IS NOT NULL
                    AND change_repository IS NOT NULL AND change_id IS NOT NULL
                    AND change_version IS NOT NULL AND context_digest IS NOT NULL
                    AND policy_epoch IS NOT NULL AND target IS NOT NULL
                    AND ((target_revision IS NULL) = (target_version IS NULL))
                    AND requirement_digest IS NOT NULL AND run_id IS NOT NULL)
            )
         ) STRICT;
         CREATE UNIQUE INDEX timeline_check_link_identity
            ON timeline_entries(run_id, context_digest, requirement_digest)
            WHERE kind = 'check_link';",
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
    if !matches!(schema_version, 4 | SCHEMA_VERSION) {
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

fn write_scope(connection: &Connection, scope: Option<&Scope>) -> Result<()> {
    if let Some(scope) = scope {
        connection.execute(
            "INSERT INTO workspace_scope(singleton, roots_json) VALUES (1, ?1)",
            [serde_json::to_vec(scope.roots())?],
        )?;
    }
    Ok(())
}

fn read_scope(connection: &Connection) -> Result<Option<Scope>> {
    let row: Option<(i64, Option<Vec<u8>>)> = connection
        .query_row(
            "SELECT length(roots_json), CASE WHEN length(roots_json) <= ?1 THEN roots_json END
             FROM workspace_scope WHERE singleton = 1",
            [MAX_SCOPE_JSON_BYTES as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((length, bytes)) = row else {
        return Ok(None);
    };
    if length < 0 || length as usize > MAX_SCOPE_JSON_BYTES {
        return Err(Error::Limit("workspace scope metadata is too large".into()));
    }
    let bytes = bytes.ok_or_else(|| Error::Invalid("workspace scope metadata is null".into()))?;
    decode_required_scope(&bytes).map(Some)
}

fn encode_optional_scope(scope: Option<&Scope>) -> Result<Vec<u8>> {
    Ok(match scope {
        Some(scope) => serde_json::to_vec(scope.roots())?,
        None => serde_json::to_vec(&Option::<Vec<String>>::None)?,
    })
}

fn decode_optional_scope(bytes: &[u8]) -> Result<Option<Scope>> {
    if bytes.len() > MAX_SCOPE_JSON_BYTES {
        return Err(Error::Limit("workspace scope metadata is too large".into()));
    }
    let roots: Option<Vec<String>> = serde_json::from_slice(bytes)?;
    roots.map(Scope::from_effective).transpose()
}

fn decode_required_scope(bytes: &[u8]) -> Result<Scope> {
    if bytes.len() > MAX_SCOPE_JSON_BYTES {
        return Err(Error::Limit("workspace scope metadata is too large".into()));
    }
    Scope::from_effective(serde_json::from_slice(bytes)?)
}

/// Validate the workspace-specific portion of a current-schema recovery database
/// without migrating it or touching the working tree.
pub(crate) fn validate_recovery_schema(connection: &Connection) -> Result<()> {
    let schema_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    connection.prepare("SELECT singleton, roots_json FROM workspace_scope LIMIT 0")?;
    connection.prepare(
        "SELECT operation_id, request_digest, status, expected_generation,
                before_roots_json, after_roots_json, outcome_json
         FROM scope_expansions LIMIT 0",
    )?;
    connection.prepare(
        "SELECT operation_id, path, after_content, after_kind
         FROM scope_expansion_paths LIMIT 0",
    )?;
    if schema_version == SCHEMA_VERSION {
        snapshots::validate_recovery_schema(connection)?;
    }
    let state = read_state(connection)?;
    let scope = read_scope(connection)?;
    let pending = pending_operation(connection)?;
    if let Some(PendingOperation {
        operation_id,
        kind: OperationKind::Expand,
    }) = pending
    {
        let expansion = read_scope_expansion(connection, &operation_id)?.ok_or_else(|| {
            Error::Invalid("pending workspace expansion is missing its journal".into())
        })?;
        if expansion.outcome.is_some() || expansion.expected_generation != state.generation {
            return Err(Error::Invalid(
                "pending workspace expansion has inconsistent generation or outcome".into(),
            ));
        }
        let current = scope.ok_or_else(|| {
            Error::Invalid("whole-tree workspace has a pending scope expansion".into())
        })?;
        if current != expansion.before {
            return Err(Error::Invalid(
                "pending workspace expansion does not match the retained scope".into(),
            ));
        }
        let staged = read_scope_expansion_staged(connection, &operation_id)?;
        for (path, stage) in staged {
            if stage.before.is_some()
                || stage.after.is_none()
                || expansion
                    .after
                    .as_ref()
                    .is_some_and(|scope| !scope.includes(&path))
                || current.includes(&path)
            {
                return Err(Error::Invalid(
                    "pending workspace expansion has an invalid staged projection".into(),
                ));
            }
        }
    }
    Ok(())
}

fn state_with_pending(connection: &Connection) -> Result<WorkspaceState> {
    let state = read_state(connection)?;
    let scope = read_scope(connection)?;
    Ok(WorkspaceState {
        change: state.change,
        base_revision: state.base_revision,
        generation: state.generation,
        ready: state.ready,
        scope: scope.map(|scope| scope.roots().to_vec()),
        pending: pending_operation(connection)?,
    })
}

fn pending_operation(connection: &Connection) -> Result<Option<PendingOperation>> {
    let ordinary: Option<(String, String)> = connection
        .query_row(
            "SELECT operation_id, kind FROM operations WHERE status = 'pending'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let expansion: Option<String> = connection
        .query_row(
            "SELECT operation_id FROM scope_expansions WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if ordinary.is_some() && expansion.is_some() {
        return Err(Error::Invalid(
            "workspace has more than one pending operation".into(),
        ));
    }
    if let Some(operation_id) = expansion {
        return Ok(Some(PendingOperation {
            operation_id,
            kind: OperationKind::Expand,
        }));
    }
    ordinary
        .map(|(operation_id, kind)| {
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
        OperationKind::Expand => "expand",
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
    if connection
        .query_row(
            "SELECT 1 FROM scope_expansions WHERE operation_id = ?1",
            [operation_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some()
    {
        return Err(Error::Conflict(format!(
            "workspace operation ID {operation_id} belongs to a scope expansion"
        )));
    }
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

fn scope_expansion_digest(
    operation_id: &str,
    expected_generation: u64,
    additions: Option<&[String]>,
) -> String {
    let mut bytes = b"AGSCM-WORKSPACE-SCOPE-EXPANSION1\0".to_vec();
    push_field(&mut bytes, operation_id.as_bytes());
    bytes.extend_from_slice(&expected_generation.to_be_bytes());
    match additions {
        Some(additions) => {
            bytes.push(1);
            for path in additions {
                push_field(&mut bytes, path.as_bytes());
            }
        }
        None => bytes.push(0),
    }
    digest(&bytes)
}

fn matching_scope_expansion(
    connection: &Connection,
    operation_id: &str,
    request_digest: &str,
) -> Result<Option<Option<OperationOutcome>>> {
    if connection
        .query_row(
            "SELECT 1 FROM operations WHERE operation_id = ?1",
            [operation_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some()
    {
        return Err(Error::Conflict(format!(
            "workspace operation ID {operation_id} belongs to another operation"
        )));
    }
    let stored_digest: Option<String> = connection
        .query_row(
            "SELECT request_digest FROM scope_expansions WHERE operation_id = ?1",
            [operation_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored_digest) = stored_digest else {
        return Ok(None);
    };
    if stored_digest != request_digest {
        return Err(Error::Conflict(format!(
            "workspace operation ID {operation_id} was reused with different input"
        )));
    }
    let expansion = read_scope_expansion(connection, operation_id)?
        .ok_or_else(|| Error::Invalid("scope expansion disappeared".into()))?;
    Ok(Some(expansion.outcome))
}

fn insert_scope_expansion(
    connection: &Connection,
    operation_id: &str,
    request_digest: &str,
    expected_generation: u64,
    before: &Scope,
    after: Option<&Scope>,
    staged: &BTreeMap<String, Stage>,
) -> Result<()> {
    connection.execute(
        "INSERT INTO scope_expansions(
            operation_id, request_digest, status, expected_generation,
            before_roots_json, after_roots_json
         ) VALUES (?1, ?2, 'pending', ?3, ?4, ?5)",
        params![
            operation_id,
            request_digest,
            sql_i64("workspace generation", expected_generation)?,
            serde_json::to_vec(before.roots())?,
            encode_optional_scope(after)?,
        ],
    )?;
    let mut statement = connection.prepare(
        "INSERT INTO scope_expansion_paths(operation_id, path, after_content, after_kind)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (path, stage) in staged {
        let item = stage
            .after
            .as_ref()
            .ok_or_else(|| Error::Invalid("scope expansion cannot stage a deletion".into()))?;
        if stage.before.is_some() {
            return Err(Error::Invalid(
                "scope expansion cannot replace a materialized path".into(),
            ));
        }
        statement.execute(params![
            operation_id,
            path,
            &item.content,
            item.kind.as_str()
        ])?;
    }
    Ok(())
}

fn read_scope_expansion(
    connection: &Connection,
    operation_id: &str,
) -> Result<Option<StoredScopeExpansion>> {
    let mut statement = connection.prepare(
        "SELECT expected_generation, before_roots_json, after_roots_json,
                status, outcome_json FROM scope_expansions WHERE operation_id = ?1",
    )?;
    let mut rows = statement.query([operation_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let generation: i64 = row.get(0)?;
    let before = bounded_journal_blob(row, 1, MAX_SCOPE_JSON_BYTES)?
        .ok_or_else(|| Error::Invalid("missing prior workspace scope".into()))?;
    let after = bounded_journal_blob(row, 2, MAX_SCOPE_JSON_BYTES)?
        .ok_or_else(|| Error::Invalid("missing new workspace scope".into()))?;
    let status: String = row.get(3)?;
    let outcome_json = bounded_journal_blob(row, 4, 4096)?;
    let outcome = match (status.as_str(), outcome_json) {
        ("pending", None) => None,
        ("completed" | "cancelled", Some(bytes)) => Some(serde_json::from_slice(&bytes)?),
        _ => {
            return Err(Error::Invalid(format!(
                "workspace scope expansion {operation_id} has inconsistent status"
            )));
        }
    };
    let generation = sql_u64("workspace generation", generation)?;
    let before = decode_required_scope(&before)?;
    let after = decode_optional_scope(&after)?;
    if after.as_ref().is_some_and(|after| {
        after == &before || before.roots().iter().any(|root| !after.includes(root))
    }) {
        return Err(Error::Invalid(
            "retained scope expansion does not extend its prior scope".into(),
        ));
    }
    if let Some(outcome) = &outcome {
        let valid = match (status.as_str(), outcome) {
            ("completed", OperationOutcome::Expanded { generation: done }) => {
                *done == next_generation(generation)?
            }
            ("cancelled", OperationOutcome::Cancelled { generation: done }) => *done == generation,
            _ => false,
        };
        if !valid {
            return Err(Error::Invalid(
                "scope expansion has an inconsistent terminal outcome".into(),
            ));
        }
    }
    Ok(Some(StoredScopeExpansion {
        expected_generation: generation,
        before,
        after,
        outcome,
    }))
}

/// Inspect SQLite's borrowed value before allocating controller-owned bytes.
fn bounded_journal_blob(
    row: &rusqlite::Row<'_>,
    column: usize,
    maximum: usize,
) -> Result<Option<Vec<u8>>> {
    match row.get_ref(column)? {
        rusqlite::types::ValueRef::Null => Ok(None),
        rusqlite::types::ValueRef::Blob(bytes) => {
            if bytes.len() > maximum {
                return Err(Error::Limit(
                    "workspace journal bytes exceed their limit".into(),
                ));
            }
            Ok(Some(bytes.to_vec()))
        }
        _ => Err(Error::Invalid(
            "workspace journal field is not a blob".into(),
        )),
    }
}

fn read_scope_expansion_staged(
    connection: &Connection,
    operation_id: &str,
) -> Result<BTreeMap<String, Stage>> {
    let mut statement = connection.prepare(
        "SELECT path, after_content, after_kind FROM scope_expansion_paths
         WHERE operation_id = ?1 ORDER BY path",
    )?;
    let mut rows = statement.query([operation_id])?;
    let mut staged = BTreeMap::new();
    let mut total_bytes = 0usize;
    while let Some(row) = rows.next()? {
        if staged.len() == MAX_ENTRIES {
            return Err(Error::Limit(
                "scope expansion has too many staged paths".into(),
            ));
        }
        let path = match row.get_ref(0)? {
            rusqlite::types::ValueRef::Text(bytes) if bytes.len() <= 1024 => {
                std::str::from_utf8(bytes)
                    .map_err(|_| Error::Invalid("workspace path is not UTF-8".into()))?
                    .to_owned()
            }
            _ => return Err(Error::Invalid("invalid staged workspace path".into())),
        };
        validate_workspace_path(&path)?;
        let allowance = MAX_TOTAL_BYTES
            .saturating_sub(total_bytes)
            .min(MAX_FILE_BYTES);
        let content = bounded_journal_blob(row, 1, allowance)?
            .ok_or_else(|| Error::Invalid("missing expansion content".into()))?;
        total_bytes += content.len();
        let kind: String = row.get(2)?;
        let item = stored_item(Some(content), &kind)?.expect("content is present");
        if staged
            .insert(
                path,
                Stage {
                    before: None,
                    after: Some(item),
                },
            )
            .is_some()
        {
            return Err(Error::Invalid(
                "scope expansion has duplicate staged paths".into(),
            ));
        }
    }
    Ok(staged)
}

fn complete_scope_expansion(
    connection: &Connection,
    operation_id: &str,
    status: &str,
    outcome: &OperationOutcome,
) -> Result<()> {
    if connection.execute(
        "UPDATE scope_expansions SET status = ?1, outcome_json = ?2
         WHERE operation_id = ?3 AND status = 'pending'",
        params![status, serde_json::to_vec(outcome)?, operation_id],
    )? != 1
    {
        return Err(Error::Conflict(format!(
            "workspace scope expansion {operation_id} is no longer pending"
        )));
    }
    Ok(())
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

fn source_inventory(engine: &Engine, revision: &str) -> Result<Option<SourceInventory>> {
    let mut entries = BTreeMap::new();
    let mut directories = BTreeSet::new();
    let mut cursor = None;
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
            validate_workspace_path(&entry.path)?;
            if entries.contains_key(&entry.path) {
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
            if entries.len() + directories.len() >= MAX_ENTRIES {
                return Ok(None);
            }
            entries.insert(entry.path, entry.kind);
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
    Ok(Some(SourceInventory {
        entries,
        directories,
    }))
}

fn require_complete_inventory(inventory: Option<SourceInventory>) -> Result<SourceInventory> {
    inventory.ok_or_else(|| {
        Error::Limit(format!(
            "source inventory exceeds {MAX_ENTRIES} entries or {MAX_DEPTH} path components"
        ))
    })
}

fn snapshot_from_inventory(
    engine: &Engine,
    revision: &str,
    inventory: &SourceInventory,
    scope: Option<&Scope>,
) -> Result<Option<Snapshot>> {
    snapshot_from_inventory_where(
        engine,
        revision,
        inventory,
        |path| scope.is_none_or(|scope| scope.includes(path)),
        |path| scope.is_none_or(|scope| scope.traverses(path)),
        true,
    )
}

/// Do not start a fresh agent with a dangling instruction link. Imported text
/// resources remain explicit inputs; this only qualifies filesystem links.
fn require_instruction_targets(snapshot: &Snapshot) -> Result<()> {
    for path in snapshot
        .files
        .keys()
        .filter(|path| scope::is_agent_instruction(path))
    {
        let mut current = path.clone();
        let mut visited = BTreeSet::new();
        loop {
            let Some(item) = snapshot.files.get(&current) else {
                return Err(Error::Conflict(format!(
                    "instruction symlink target is not selected: {current}; include it with --path before starting the workspace"
                )));
            };
            if item.kind != EntryKind::Symlink {
                break;
            }
            if !visited.insert(current.clone()) || visited.len() > MAX_DEPTH {
                return Err(Error::Invalid(format!(
                    "instruction symlink cycle or depth limit: {path}"
                )));
            }
            validate_symlink_target(&current, &item.content)?;
            let target = std::str::from_utf8(&item.content)
                .map_err(|_| Error::Invalid("instruction symlink target is not UTF-8".into()))?;
            let mut parts: Vec<&str> = current.split('/').collect();
            parts.pop();
            for component in target.split('/') {
                if component == ".." {
                    parts.pop();
                } else {
                    parts.push(component);
                }
            }
            let target = parts.join("/");
            current = target;
        }
    }
    Ok(())
}

fn snapshot_from_inventory_where(
    engine: &Engine,
    revision: &str,
    inventory: &SourceInventory,
    mut selected: impl FnMut(&str) -> bool,
    mut traversed: impl FnMut(&str) -> bool,
    retain_content: bool,
) -> Result<Option<Snapshot>> {
    let mut files = BTreeMap::new();
    let mut directories = BTreeSet::new();
    let mut total_bytes = 0usize;
    for (path, kind) in &inventory.entries {
        if !selected(path) {
            continue;
        }
        let content = engine.read_file(revision, path)?;
        if content.len() > MAX_FILE_BYTES {
            return Ok(None);
        }
        total_bytes = total_bytes
            .checked_add(content.len())
            .ok_or_else(|| Error::Limit("workspace byte count overflow".to_owned()))?;
        if total_bytes > MAX_TOTAL_BYTES || files.len() + directories.len() >= MAX_ENTRIES {
            return Ok(None);
        }
        for (separator, _) in path.match_indices('/') {
            let directory = &path[..separator];
            if traversed(directory) {
                directories.insert(directory.to_owned());
            }
        }
        let content = if retain_content {
            content
        } else {
            Sha256::digest(&content).to_vec()
        };
        files.insert(
            path.clone(),
            Item {
                kind: *kind,
                content,
            },
        );
    }
    for directory in &inventory.directories {
        if traversed(directory) {
            directories.insert(directory.clone());
        }
    }
    Ok(Some(Snapshot { files, directories }))
}

fn source_snapshot_scoped(
    engine: &Engine,
    revision: &str,
    scope: Option<&Scope>,
) -> Result<Option<Snapshot>> {
    let Some(inventory) = source_inventory(engine, revision)? else {
        return Ok(None);
    };
    snapshot_from_inventory(engine, revision, &inventory, scope)
}

/// `source_snapshot_scoped` with digests in place of contents, under the same limits.
fn source_digests_scoped(
    engine: &Engine,
    revision: &str,
    scope: Option<&Scope>,
) -> Result<Option<Snapshot>> {
    let Some(inventory) = source_inventory(engine, revision)? else {
        return Ok(None);
    };
    snapshot_from_inventory_where(
        engine,
        revision,
        &inventory,
        |path| scope.is_none_or(|scope| scope.includes(path)),
        |path| scope.is_none_or(|scope| scope.traverses(path)),
        false,
    )
}

fn require_scope_selection_dependencies(
    engine: &Engine,
    revision: &str,
    scope: Option<&Scope>,
) -> Result<()> {
    let Some(scope) = scope else {
        return Ok(());
    };
    let inventory = require_complete_inventory(source_inventory(engine, revision)?)?;
    let required =
        scope.with_selection_dependencies(inventory.entries.keys().map(String::as_str))?;
    let missing: Vec<_> = required
        .roots()
        .iter()
        .filter(|path| !scope.includes(path))
        .cloned()
        .collect();
    if missing.is_empty() {
        let snapshot = require_complete_snapshot(snapshot_from_inventory(
            engine,
            revision,
            &inventory,
            Some(scope),
        )?)?;
        require_instruction_targets(&snapshot)
    } else {
        Err(Error::Conflict(format!(
            "workspace scope must expand to include target selection dependencies: {}",
            missing.join(", ")
        )))
    }
}

fn source_snapshot(engine: &Engine, revision: &str) -> Result<Option<Snapshot>> {
    source_snapshot_scoped(engine, revision, None)
}

/// Bound on the excluded-path bookkeeping kept for preview, so a huge ignored tree cannot grow
/// memory; entries past the bound are counted but not listed.
const MAX_REPORTED_PATHS: usize = 10_000;

/// A working-tree path left out of the source by an exclusion rule.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ExcludedPath {
    /// Relative path; non-UTF-8 names are shown lossily.
    pub path: String,
    pub directory: bool,
    pub rule: Rule,
}

/// A captured path that an explicit `!pattern` rule included.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IncludedPath {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
}

#[derive(Debug, Default)]
struct ScanReport {
    selection_digest: String,
    excluded: Vec<ExcludedPath>,
    excluded_total: usize,
    included_by_rule: BTreeMap<String, Rule>,
    /// Set when `included_by_rule` hit its bound.
    included_by_rule_truncated: bool,
    /// Tracked files that an exclusion rule would have excluded.
    overridden: BTreeSet<String>,
}

impl ScanReport {
    fn record_excluded(&mut self, relative: &Path, directory: bool, rule: Rule) {
        self.excluded_total += 1;
        if self.excluded.len() < MAX_REPORTED_PATHS {
            self.excluded.push(ExcludedPath {
                path: relative.to_string_lossy().into_owned(),
                directory,
                rule,
            });
        }
    }

    fn record_included_by_rule(&mut self, relative: String, rule: Rule) {
        if self.included_by_rule.len() < MAX_REPORTED_PATHS {
            self.included_by_rule.insert(relative, rule);
        } else {
            self.included_by_rule_truncated = true;
        }
    }
}

struct Scan<'a> {
    selector: Selector<'a>,
    scope: Option<&'a Scope>,
    snapshot: Snapshot,
    portable: BTreeMap<String, String>,
    total_bytes: usize,
    /// Selected entries; bounded by `MAX_ENTRIES`.
    entry_count: usize,
    /// Everything read from directories, selected or not; bounded by `MAX_VISITED_ENTRIES`.
    visited: usize,
    report: ScanReport,
    /// False keeps only a SHA-256 per file, so status does not hold every working byte.
    retain_content: bool,
}

struct Candidate {
    entry: fs::DirEntry,
    metadata: fs::Metadata,
    classified: Classified,
    relative_path: PathBuf,
}

fn scan_workspace_scoped(
    root: &Path,
    tracked: &Tracked,
    scope: Option<&Scope>,
) -> Result<Option<Snapshot>> {
    Ok(scan_workspace_full_scoped(root, tracked, scope)?.map(|(snapshot, _)| snapshot))
}

fn scan_workspace_full_scoped<'a>(
    root: &Path,
    tracked: &'a Tracked,
    scope: Option<&'a Scope>,
) -> Result<Option<(Snapshot, ScanReport)>> {
    let metadata = root.join(METADATA_DIRECTORY);
    scan_workspace_full_scoped_with_probe(root, tracked, scope, &metadata, true)
}

/// `scan_workspace_full_scoped` with digests in place of contents, under the same limits.
fn scan_workspace_digests_scoped<'a>(
    root: &Path,
    tracked: &'a Tracked,
    scope: Option<&'a Scope>,
) -> Result<Option<(Snapshot, ScanReport)>> {
    let metadata = root.join(METADATA_DIRECTORY);
    scan_workspace_full_scoped_with_probe(root, tracked, scope, &metadata, false)
}

fn scan_workspace_full_scoped_with_probe<'a>(
    root: &Path,
    tracked: &'a Tracked,
    scope: Option<&'a Scope>,
    probe_directory: &Path,
    retain_content: bool,
) -> Result<Option<(Snapshot, ScanReport)>> {
    // Kinds are recorded from file modes, so a mount that reports every file
    // executable must be refused rather than reported as phantom changes.
    require_executable_support(probe_directory)?;
    let settings = git_config::ambient_settings(root, || probe_case_insensitive(probe_directory))?;
    scan_with_settings_scoped(root, tracked, &settings, scope, retain_content)
}

#[cfg(test)]
/// The scan with explicit settings: the seam for tests that must not depend on the ambient Git
/// configuration of the machine.
fn scan_workspace_report(
    root: &Path,
    tracked: &Tracked,
    settings: &Settings,
) -> Result<Option<(Snapshot, ScanReport)>> {
    require_executable_support(&root.join(METADATA_DIRECTORY))?;
    scan_with_settings(root, tracked, settings)
}

#[cfg(test)]
fn scan_with_settings(
    root: &Path,
    tracked: &Tracked,
    settings: &Settings,
) -> Result<Option<(Snapshot, ScanReport)>> {
    scan_with_settings_scoped(root, tracked, settings, None, true)
}

fn scan_with_settings_scoped<'a>(
    root: &Path,
    tracked: &'a Tracked,
    settings: &'a Settings,
    scope: Option<&'a Scope>,
    retain_content: bool,
) -> Result<Option<(Snapshot, ScanReport)>> {
    let mut scan = Scan {
        selector: Selector::open(root, tracked, settings)?,
        scope,
        snapshot: Snapshot {
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
        },
        portable: BTreeMap::new(),
        total_bytes: 0,
        entry_count: 0,
        visited: 0,
        report: ScanReport::default(),
        retain_content,
    };
    if !scan.directory(root, Path::new(""), 0, None)? {
        return Ok(None);
    }
    scan.report.selection_digest = scan.selector.input_digest();
    Ok(Some((scan.snapshot, scan.report)))
}

impl Scan<'_> {
    fn directory(
        &mut self,
        directory: &Path,
        prefix: &Path,
        depth: usize,
        inherited: Option<&Rule>,
    ) -> Result<bool> {
        if depth > MAX_DEPTH {
            return Ok(false);
        }
        let prefix_text = prefix.to_str().unwrap_or_default().to_owned();
        // An invalid or oversize `.gitignore` in an included directory is an error rather than
        // silently selecting everything.
        let pushed = self.selector.enter_directory(directory, &prefix_text)?;
        let result = self
            .candidates(directory, prefix, inherited)
            .and_then(|candidates| match candidates {
                Some(candidates) => self.entries(candidates, depth),
                None => Ok(false),
            });
        if pushed {
            self.selector.leave_directory();
        }
        result
    }

    /// Classify every entry as it is read, so excluded entries are never held in memory.
    /// `None` means a bound was exceeded.
    fn candidates(
        &mut self,
        directory: &Path,
        prefix: &Path,
        inherited: Option<&Rule>,
    ) -> Result<Option<Vec<Candidate>>> {
        let mut selected = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if prefix.as_os_str().is_empty()
                && (entry.file_name() == METADATA_DIRECTORY || entry.file_name() == ".git")
            {
                continue;
            }
            if self.visited == MAX_VISITED_ENTRIES {
                return Ok(None);
            }
            self.visited += 1;
            let relative_path = prefix.join(entry.file_name());
            let Some(relative_text) = relative_path.to_str() else {
                if self.scope.is_some() {
                    continue;
                }
                return Err(Error::Invalid("workspace path is not UTF-8".to_owned()));
            };
            if self
                .scope
                .is_some_and(|scope| !scope.traverses(relative_text))
            {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            let is_dir = metadata.file_type().is_dir();
            // Selection comes first: an excluded path is never validated, opened or read, so
            // ignored non-portable, oversize or reserved-name paths do not fail the scan.
            let classified = self.selector.classify(&relative_path, is_dir, inherited);
            self.selector.check_work()?;
            match &classified.selection {
                Selection::Excluded(rule) => {
                    self.report
                        .record_excluded(&relative_path, is_dir, rule.clone());
                    continue;
                }
                Selection::IncludedByRule(rule) => {
                    self.report.record_included_by_rule(
                        relative_path.to_string_lossy().into_owned(),
                        rule.clone(),
                    );
                }
                Selection::Included => {}
            }
            if classified.overridden.is_some() && !is_dir {
                self.report
                    .overridden
                    .insert(relative_path.to_string_lossy().into_owned());
            }
            if self.entry_count == MAX_ENTRIES {
                return Ok(None);
            }
            self.entry_count += 1;
            selected.push(Candidate {
                entry,
                metadata,
                classified,
                relative_path,
            });
        }
        selected.sort_by_key(|candidate| candidate.entry.file_name());
        Ok(Some(selected))
    }

    fn entries(&mut self, candidates: Vec<Candidate>, depth: usize) -> Result<bool> {
        for candidate in candidates {
            let Candidate {
                entry,
                metadata,
                classified,
                relative_path,
            } = candidate;
            let path = entry.path();
            let file_type = metadata.file_type();
            let relative = relative_path
                .into_os_string()
                .into_string()
                .map_err(|_| Error::Invalid("workspace path is not UTF-8".to_owned()))?;
            validate_workspace_path(&relative)?;
            let folded = relative.to_ascii_lowercase();
            if let Some(existing) = self.portable.insert(folded, relative.clone())
                && existing != relative
            {
                return Err(Error::Conflict(format!(
                    "workspace paths collide on portable filesystems: {existing} and {relative}"
                )));
            }
            if file_type.is_symlink() {
                if self.scope.is_some_and(|scope| !scope.includes(&relative)) {
                    return Err(Error::Conflict(format!(
                        "workspace scope crosses a symbolic link: {relative}"
                    )));
                }
                // Never followed: the link itself is the entry, so a link to a
                // directory is not descended into.
                let mut item = read_symlink_item(&path, &relative)?;
                self.total_bytes = self
                    .total_bytes
                    .checked_add(item.content.len())
                    .ok_or_else(|| Error::Limit("workspace byte count overflow".to_owned()))?;
                if self.total_bytes > MAX_TOTAL_BYTES {
                    return Ok(false);
                }
                if !self.retain_content {
                    item.content = Sha256::digest(&item.content).to_vec();
                }
                self.snapshot.files.insert(relative, item);
                continue;
            }
            if file_type.is_dir() {
                // Scoped scans retain traversed ancestors as structural state,
                // even though files outside the exact roots stay invisible.
                self.snapshot.directories.insert(relative.clone());
                if !self.directory(
                    &path,
                    Path::new(&relative),
                    depth + 1,
                    classified.child_exclusion.as_ref(),
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
            if self.scope.is_some_and(|scope| !scope.includes(&relative)) {
                return Err(Error::Conflict(format!(
                    "workspace scope crosses a non-directory entry: {relative}"
                )));
            }
            if metadata.len() > MAX_FILE_BYTES as u64 {
                return Ok(false);
            }
            let (item, length) = if self.retain_content {
                let item = read_regular_file(&path, &relative)?;
                let length = item.content.len();
                (item, length)
            } else {
                // Streamed through the hasher so no whole file is buffered.
                let mut hasher = Sha256::new();
                let (kind, length) = read_regular_into(&path, &relative, &mut hasher)?;
                let content = hasher.finalize().to_vec();
                (Item { kind, content }, length)
            };
            self.total_bytes = self
                .total_bytes
                .checked_add(length)
                .ok_or_else(|| Error::Limit("workspace byte count overflow".to_owned()))?;
            if self.total_bytes > MAX_TOTAL_BYTES {
                return Ok(false);
            }
            self.snapshot.files.insert(relative, item);
        }
        Ok(true)
    }
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
    // Preallocated as before so a large file is not regrown by doubling; the size is rechecked.
    let size = fs::symlink_metadata(path)?.len().min(MAX_FILE_BYTES as u64);
    let mut content = Vec::with_capacity(size as usize);
    let (kind, _) = read_regular_into(path, relative, &mut content)?;
    Ok(Item { kind, content })
}

/// Copy a regular file into `sink` under the size bound, refusing a file that changes while it
/// is read. Returns its kind and byte length.
fn read_regular_into(
    path: &Path,
    relative: &str,
    sink: &mut impl Write,
) -> Result<(EntryKind, usize)> {
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
    let length = std::io::copy(
        &mut Read::by_ref(&mut file).take(MAX_FILE_BYTES as u64 + 1),
        sink,
    )?;
    if length > MAX_FILE_BYTES as u64 {
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
    // The bound check above keeps `length` within `MAX_FILE_BYTES`.
    Ok((kind_of_mode(opened.permissions().mode()), length as usize))
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

fn require_complete_scan(scan: Option<(Snapshot, ScanReport)>) -> Result<(Snapshot, ScanReport)> {
    match scan {
        Some((snapshot, report)) => Ok((require_complete_snapshot(Some(snapshot))?, report)),
        None => require_complete_snapshot(None).map(|snapshot| (snapshot, ScanReport::default())),
    }
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
    let scope = read_scope(&tx)?;
    if state.base_revision != operation.before_revision {
        return Err(Error::Conflict(
            "workspace base changed while undo was pending".to_owned(),
        ));
    }
    let staged = read_staged(&tx, operation_id)?;
    let desired_after = choice == RecoveryChoice::Complete;
    let tracked = Tracked::from_paths(staged.keys().map(String::as_str));
    let working =
        require_complete_snapshot(scan_workspace_scoped(root, &tracked, scope.as_ref())?)?;
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

/// Staged destinations that hold a local file only because the target revision names the path:
/// a rule excludes them and the current base does not track them. They are the user's own
/// files, so the operation must not adopt, overwrite or later roll back over them, even when
/// the bytes happen to match.
fn excluded_occupants(
    report: &ScanReport,
    staged: &BTreeMap<String, Stage>,
    current: &BTreeMap<String, Item>,
) -> Vec<String> {
    staged
        .keys()
        .filter(|path| report.overridden.contains(*path) && !current.contains_key(*path))
        .cloned()
        .collect()
}

fn newly_scoped_occupants(
    working: &Snapshot,
    current: &Scope,
    target: Option<&Scope>,
) -> Vec<String> {
    working
        .files
        .keys()
        .filter(|path| !current.includes(path) && target.is_none_or(|scope| scope.includes(path)))
        .cloned()
        .collect()
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

const EXPANSION_WITNESSES_DIRECTORY: &str = "expansion-witnesses";

fn expansion_witness_path(root: &Path, operation_id: &str, path: &str, item: &Item) -> PathBuf {
    let operation = digest(operation_id.as_bytes());
    let mut identity = Sha256::new();
    identity.update(b"AGSCM-WORKSPACE-EXPANSION-WITNESS1\0");
    identity.update((path.len() as u64).to_be_bytes());
    identity.update(path.as_bytes());
    identity.update((item.kind.as_str().len() as u64).to_be_bytes());
    identity.update(item.kind.as_str().as_bytes());
    identity.update((item.content.len() as u64).to_be_bytes());
    identity.update(&item.content);
    root.join(METADATA_DIRECTORY)
        .join(EXPANSION_WITNESSES_DIRECTORY)
        .join(operation)
        .join(format!("{:x}", identity.finalize()))
}

fn read_expansion_item(path: &Path, relative: &str) -> Result<Item> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        read_symlink_item(path, relative)
    } else if metadata.file_type().is_file() {
        read_regular_file(path, relative)
    } else {
        Err(Error::Conflict(format!(
            "workspace expansion path is not a file: {relative}"
        )))
    }
}

fn expansion_target_is_witnessed(
    root: &Path,
    operation_id: &str,
    path: &str,
    item: &Item,
) -> Result<bool> {
    reject_symlink_ancestors(root, path)?;
    let target = root.join(path);
    let target_metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(false);
        }
        Err(error) => return Err(Error::Io(error)),
    };
    let witness = expansion_witness_path(root, operation_id, path, item);
    let witness_metadata = match fs::symlink_metadata(&witness) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::Io(error)),
    };
    if target_metadata.dev() != witness_metadata.dev()
        || target_metadata.ino() != witness_metadata.ino()
    {
        return Ok(false);
    }
    Ok(read_expansion_item(&target, path)? == *item
        && read_expansion_item(&witness, path)? == *item)
}

fn unproven_expansion_targets(
    root: &Path,
    operation_id: &str,
    staged: &BTreeMap<String, Stage>,
) -> Result<Vec<String>> {
    let mut blocked = Vec::new();
    for (path, stage) in staged {
        let item = stage
            .after
            .as_ref()
            .ok_or_else(|| Error::Invalid("scope expansion staged a deletion".into()))?;
        match fs::symlink_metadata(root.join(path)) {
            Ok(_) if !expansion_target_is_witnessed(root, operation_id, path, item)? => {
                blocked.push(path.clone());
            }
            Ok(_) => {}
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(blocked)
}

fn prepare_expansion_witness(
    root: &Path,
    operation_id: &str,
    path: &str,
    item: &Item,
) -> Result<PathBuf> {
    let witness = expansion_witness_path(root, operation_id, path, item);
    let directory = witness
        .parent()
        .ok_or_else(|| Error::Invalid("workspace expansion witness has no parent".into()))?;
    fs::create_dir_all(directory)?;
    require_directory(directory, "workspace expansion witness directory")?;
    let witness_parent = directory
        .parent()
        .expect("workspace expansion witness directory has a parent");
    sync_directory(&root.join(METADATA_DIRECTORY))?;
    sync_directory(witness_parent)?;
    sync_directory(directory)?;
    match fs::symlink_metadata(&witness) {
        Ok(_) => {
            if read_expansion_item(&witness, path)? != *item {
                return Err(Error::Conflict(format!(
                    "workspace expansion witness diverged: {path}"
                )));
            }
            return Ok(witness);
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Io(error)),
    }
    if item.kind == EntryKind::Symlink {
        validate_symlink_target(path, &item.content)?;
        symlink(std::ffi::OsStr::from_bytes(&item.content), &witness)?;
    } else {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        if item.kind == EntryKind::Executable {
            options.mode(0o777);
        }
        let mut file = options.open(&witness)?;
        file.write_all(&item.content)?;
        file.sync_all()?;
        if item.kind == EntryKind::Executable
            && kind_of_mode(file.metadata()?.permissions().mode()) != EntryKind::Executable
        {
            return Err(Error::Invalid(format!(
                "process umask clears the owner execute bit for executable: {path}"
            )));
        }
    }
    sync_directory(directory)?;
    Ok(witness)
}

fn apply_expansion_staged(
    root: &Path,
    operation_id: &str,
    staged: &BTreeMap<String, Stage>,
    desired_after: bool,
) -> Result<()> {
    if desired_after {
        require_staged_support(root, staged)?;
    }
    for (path, stage) in staged {
        let item = stage
            .after
            .as_ref()
            .ok_or_else(|| Error::Invalid("scope expansion staged a deletion".into()))?;
        reject_symlink_ancestors(root, path)?;
        let target = root.join(path);
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                if !expansion_target_is_witnessed(root, operation_id, path, item)? {
                    return Err(Error::Conflict(format!(
                        "workspace expansion target is not an installed journal entry: {path}"
                    )));
                }
                if !desired_after {
                    fs::remove_file(&target)?;
                    sync_directory(target.parent().expect("workspace file has parent"))?;
                }
                continue;
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Err(error) => return Err(Error::Io(error)),
        }
        if !desired_after {
            continue;
        }
        let witness = prepare_expansion_witness(root, operation_id, path, item)?;
        create_parent_directories(root, path)?;
        match fs::hard_link(&witness, &target) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return Err(Error::Conflict(format!(
                    "workspace path appeared before expansion installation: {path}"
                )));
            }
            Err(error) => return Err(Error::Io(error)),
        }
        sync_directory(target.parent().expect("workspace file has parent"))?;
    }
    sync_directory(root)
}

fn cleanup_expansion_witnesses(
    root: &Path,
    operation_id: &str,
    staged: &BTreeMap<String, Stage>,
) -> Result<()> {
    let parent = root
        .join(METADATA_DIRECTORY)
        .join(EXPANSION_WITNESSES_DIRECTORY);
    let directory = parent.join(digest(operation_id.as_bytes()));
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(Error::Invalid(
                "workspace expansion witness path is not a directory".into(),
            ));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::Io(error)),
    }
    for (path, stage) in staged {
        let item = stage
            .after
            .as_ref()
            .ok_or_else(|| Error::Invalid("scope expansion staged a deletion".into()))?;
        let witness = expansion_witness_path(root, operation_id, path, item);
        match fs::symlink_metadata(&witness) {
            Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
                // Only terminal operations reach cleanup. The retained journal owns this
                // metadata name; unlinking it never removes or opens the working path.
                // Its bytes/mode may have changed through a legitimate edit of the other
                // hard link after the scope transaction committed.
                fs::remove_file(&witness)?;
            }
            Ok(_) => {
                return Err(Error::Conflict(format!(
                    "workspace expansion witness is not a removable metadata file: {path}"
                )));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(Error::Io(error)),
        }
    }
    sync_directory(&directory)?;
    fs::remove_dir(&directory).map_err(|error| {
        if error.kind() == ErrorKind::DirectoryNotEmpty {
            Error::Conflict("workspace expansion witness directory has unexpected entries".into())
        } else {
            Error::Io(error)
        }
    })?;
    sync_directory(&parent)
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

/// Remove probe leftovers from a process killed mid-probe. Callers must serialize
/// probes sharing a directory. Capture uses its own directory under the timeline
/// lease; creation owns a new metadata directory. Only direct entries with the
/// probe prefix that are files or symbolic links are removed; links are never
/// followed and nothing is recursed into.
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

/// Whether names differing only in ASCII case name the same file here. The probe file lives in
/// the workspace metadata directory and is removed before returning.
fn probe_case_insensitive(directory: &Path) -> Result<bool> {
    sweep_stale_probes(directory)?;
    let path = probe_path(directory, "CaseProbe");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    drop(file);
    let name = path
        .file_name()
        .expect("probe path has a file name")
        .as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_uppercase() {
                byte.to_ascii_lowercase()
            } else {
                byte.to_ascii_uppercase()
            }
        })
        .collect::<Vec<u8>>();
    let flipped = path.with_file_name(std::ffi::OsString::from_vec(name));
    let insensitive = fs::symlink_metadata(&flipped).is_ok();
    fs::remove_file(&path)?;
    Ok(insensitive)
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
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(unix)]
    use std::process::{Child, Command, Stdio};
    #[cfg(unix)]
    use std::time::Instant;
    use tempfile::TempDir;

    #[cfg(unix)]
    const CHECKPOINT_BOUNDARY_ROOT: &str = "AGENTSCM_CHECKPOINT_BOUNDARY_ROOT";
    #[cfg(unix)]
    const CHECKPOINT_BOUNDARY_READY: &str = "AGENTSCM_CHECKPOINT_BOUNDARY_READY";
    #[cfg(unix)]
    const CHECKPOINT_BOUNDARY_SOURCE: &str = "AGENTSCM_CHECKPOINT_BOUNDARY_SOURCE";

    #[cfg(unix)]
    pub(super) fn checkpoint_boundary(root: &Path) -> Result<()> {
        let Some(expected_root) = std::env::var_os(CHECKPOINT_BOUNDARY_ROOT) else {
            return Ok(());
        };
        if root != Path::new(&expected_root) {
            return Err(Error::Invalid(
                "test checkpoint boundary workspace mismatch".into(),
            ));
        }
        let ready = std::env::var_os(CHECKPOINT_BOUNDARY_READY)
            .ok_or_else(|| Error::Invalid("test checkpoint boundary ready path missing".into()))?;
        let mut ready_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(ready)?;
        ready_file.write_all(b"durable-pending")?;
        ready_file.sync_all()?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(Error::Limit(
            "test checkpoint boundary pause expired".into(),
        ))
    }

    #[cfg(unix)]
    struct CheckpointChild(Option<Child>);

    #[cfg(unix)]
    impl Drop for CheckpointChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

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

    #[cfg(unix)]
    #[test]
    fn killed_checkpoint_process_recovers_its_staged_bytes() {
        let fixtures = TempDir::new().unwrap();
        let source = fixtures.path().join("store");
        let engine = Engine::open(&source).unwrap();
        let base = engine.checkpoint(None, &[put("a.txt", b"old")]).unwrap();
        let root = fixtures.path().join("workspace");
        Workspace::create(
            &engine,
            &root,
            ChangeId {
                repository: "repository-crash".to_owned(),
                id: "change-crash".to_owned(),
            },
            &base,
        )
        .unwrap();
        let changed = b"staged after killed checkpoint";
        fs::write(root.join("a.txt"), changed).unwrap();

        let ready = fixtures.path().join("checkpoint-ready");
        let stdout_path = fixtures.path().join("checkpoint-child.stdout");
        let stderr_path = fixtures.path().join("checkpoint-child.stderr");
        let mut owned = CheckpointChild(Some(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "workspace::tests::subprocess_checkpoint_helper",
                    "--nocapture",
                ])
                .env_clear()
                .env(CHECKPOINT_BOUNDARY_ROOT, &root)
                .env(CHECKPOINT_BOUNDARY_READY, &ready)
                .env(CHECKPOINT_BOUNDARY_SOURCE, &source)
                .stdin(Stdio::null())
                .stdout(File::create(&stdout_path).unwrap())
                .stderr(File::create(&stderr_path).unwrap())
                .spawn()
                .unwrap(),
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if matches!(fs::read(&ready), Ok(bytes) if bytes == b"durable-pending") {
                break;
            }
            assert!(
                owned.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                "checkpoint child exited before durable pending boundary: {}",
                fs::read_to_string(&stderr_path).unwrap()
            );
            assert!(
                Instant::now() < deadline,
                "durable pending checkpoint boundary was not reached"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let child = owned.0.as_mut().unwrap();
        child.kill().unwrap();
        let status = child.wait().unwrap();
        drop(owned.0.take());
        assert_eq!(
            status.signal(),
            Some(9),
            "checkpoint child was not SIGKILLed"
        );

        let mut workspace = Workspace::open(&root).unwrap();
        let state = workspace.state().unwrap();
        assert_eq!(state.generation, 0);
        assert_eq!(state.pending.unwrap().operation_id, "killed-checkpoint");
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
        let state = workspace.state().unwrap();
        assert_eq!(state.generation, 1);
        assert!(state.pending.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_checkpoint_helper() {
        let Some(root) = std::env::var_os(CHECKPOINT_BOUNDARY_ROOT) else {
            return;
        };
        let source = std::env::var_os(CHECKPOINT_BOUNDARY_SOURCE).unwrap();
        let engine = Engine::open(source).unwrap();
        let mut workspace = Workspace::open(root).unwrap();
        workspace
            .checkpoint(&engine, "killed-checkpoint", 0)
            .unwrap();
    }

    #[test]
    fn scoped_creation_reads_fewer_encoded_source_bytes_than_full_creation() {
        const BUDGET: u64 = 1 << 40;
        let fixtures = TempDir::new().unwrap();
        let engine = Engine::open(fixtures.path().join("store")).unwrap();
        let mut changes = vec![put("task/small.txt", b"task")];
        let unrelated: Vec<Vec<u8>> = (0..32)
            .map(|index| {
                let mut bytes = vec![index as u8; 64 * 1024];
                bytes[..8].copy_from_slice(&(index as u64).to_be_bytes());
                bytes
            })
            .collect();
        for (index, bytes) in unrelated.iter().enumerate() {
            changes.push(put(&format!("unrelated/{index:02}.bin"), bytes));
        }
        let revision = engine.checkpoint(None, &changes).unwrap();
        let change = || ChangeId {
            repository: "repository-read-measure".to_owned(),
            id: "change-read-measure".to_owned(),
        };

        let (full_engine, full_budget) = engine.with_read_budget(BUDGET);
        Workspace::create(
            &full_engine,
            fixtures.path().join("full"),
            change(),
            &revision,
        )
        .unwrap();
        full_budget.finish(Ok(())).unwrap();
        let full_bytes = BUDGET - full_budget.remaining();

        let (scoped_engine, scoped_budget) = engine.with_read_budget(BUDGET);
        Workspace::create_scoped(
            &scoped_engine,
            fixtures.path().join("scoped"),
            change(),
            &revision,
            &["task".to_owned()],
        )
        .unwrap();
        scoped_budget.finish(Ok(())).unwrap();
        let scoped_bytes = BUDGET - scoped_budget.remaining();

        eprintln!("workspace encoded source bytes: full={full_bytes}, scoped={scoped_bytes}");
        assert!(
            scoped_bytes < full_bytes,
            "scoped creation must avoid unrelated blob reads"
        );
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
    fn read_only_open_inspects_current_workspaces_and_never_migrates_older_ones() {
        let fixtures = TempDir::new().unwrap();
        let engine = Engine::open(fixtures.path().join("store")).unwrap();
        let revision = engine.checkpoint(None, &[put("a.txt", b"a")]).unwrap();
        let change = ChangeId {
            repository: "0".repeat(32),
            id: "change-1".into(),
        };
        let root = fixtures.path().join("workspace");
        Workspace::create(&engine, &root, change, &revision).unwrap();
        let database = root.join(METADATA_DIRECTORY).join(DATABASE_FILE);
        // Main database bytes, mtime, `user_version` and the metadata names; only SQLite's own
        // `-wal`/`-shm` sidecars may come and go.
        let snapshot = || {
            let version: i64 =
                Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .unwrap()
                    .pragma_query_value(None, "user_version", |row| row.get(0))
                    .unwrap();
            let mut names: Vec<_> = fs::read_dir(root.join(METADATA_DIRECTORY))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .filter(|name| !name.ends_with("-wal") && !name.ends_with("-shm"))
                .collect();
            names.sort();
            (
                fs::read(&database).unwrap(),
                fs::metadata(&database).unwrap().modified().unwrap(),
                version,
                names,
            )
        };
        let before = snapshot();
        let inspected = Workspace::open_read_only(&root).unwrap();
        let state = inspected.state().unwrap();
        assert_eq!(state.base_revision, revision);
        let status = inspected.status(&engine, 8).unwrap();
        assert!(status.complete && status.changes.is_empty());
        assert_eq!(status.generation, state.generation);
        drop(inspected);
        assert_eq!(snapshot(), before);

        let raw = Connection::open(&database).unwrap();
        raw.pragma_update(None, "user_version", 3).unwrap();
        drop(raw);
        let older = snapshot();
        assert!(Workspace::open_read_only(&root).is_err());
        assert_eq!(snapshot(), older);
        let raw = Connection::open(&database).unwrap();
        let version: i64 = raw
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
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

    struct SelectionFixture {
        _directory: TempDir,
        root: PathBuf,
        _workspace: Workspace,
    }

    fn selection_fixture() -> SelectionFixture {
        let directory = TempDir::new().unwrap();
        let engine = Engine::open(directory.path().join("source")).unwrap();
        let revision = engine.checkpoint(None, &[put("a.txt", b"old")]).unwrap();
        let root = directory.path().join("workspace");
        let change = ChangeId {
            repository: "repository-1".to_owned(),
            id: "change-1".to_owned(),
        };
        let workspace = Workspace::create(&engine, &root, change, &revision).unwrap();
        SelectionFixture {
            _directory: directory,
            root,
            _workspace: workspace,
        }
    }

    fn write_at(root: &Path, path: &str, content: &str) {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn excluded_of(report: &ScanReport) -> Vec<(String, String)> {
        let mut excluded: Vec<_> = report
            .excluded
            .iter()
            .map(|entry| (entry.path.clone(), entry.rule.source.clone()))
            .collect();
        excluded.sort();
        excluded
    }

    #[test]
    fn explicit_global_excludes_apply_only_with_a_git_common_dir() {
        let fixture = selection_fixture();
        let global = fixture.root.parent().unwrap().join("global-ignore");
        fs::write(&global, "*.globalonly\n").unwrap();
        write_at(&fixture.root, "x.globalonly", "x");
        let tracked = Tracked::default();
        let without_git = Settings {
            git_common_dir: None,
            global_excludes: Some(global.clone()),
            case_insensitive: false,
        };
        let (snapshot, report) = scan_workspace_report(&fixture.root, &tracked, &without_git)
            .unwrap()
            .unwrap();
        assert!(snapshot.files.contains_key("x.globalonly"));
        assert!(report.excluded.is_empty());

        fs::create_dir(fixture.root.join(".git")).unwrap();
        let with_git = Settings {
            git_common_dir: Some(fixture.root.join(".git")),
            ..without_git
        };
        let (snapshot, report) = scan_workspace_report(&fixture.root, &tracked, &with_git)
            .unwrap()
            .unwrap();
        assert!(!snapshot.files.contains_key("x.globalonly"));
        assert_eq!(
            excluded_of(&report),
            [("x.globalonly".to_owned(), "global-excludes".to_owned())]
        );
    }

    #[test]
    fn native_selection_hashes_non_utf8_root_and_global_excludes_bytes() {
        let directory = TempDir::new().unwrap();
        let engine = Engine::open(directory.path().join("source")).unwrap();
        let revision = engine.checkpoint(None, &[put("a.txt", b"old")]).unwrap();
        let root = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"workspace-\xff".to_vec()));
        match fs::create_dir(&root) {
            Ok(()) => fs::remove_dir(&root).unwrap(),
            Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => return,
            Err(error) => panic!("non-UTF-8 filesystem probe failed: {error}"),
        }
        let change = ChangeId {
            repository: "repository-1".into(),
            id: "change-non-utf8".into(),
        };
        let _workspace = Workspace::create(&engine, &root, change, &revision).unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        let global = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"global-\xfe".to_vec()));
        fs::write(&global, b"*.ignored\n").unwrap();
        let settings = Settings {
            git_common_dir: Some(root.join(".git")),
            global_excludes: Some(global.clone()),
            case_insensitive: false,
        };
        let tracked = Tracked::from_paths(["a.txt"]);
        let first = scan_workspace_report(&root, &tracked, &settings)
            .unwrap()
            .unwrap()
            .1
            .selection_digest;
        let second = scan_workspace_report(&root, &tracked, &settings)
            .unwrap()
            .unwrap()
            .1
            .selection_digest;
        assert_eq!(first, second);
        fs::write(global, b"*.other\n").unwrap();
        let changed = scan_workspace_report(&root, &tracked, &settings)
            .unwrap()
            .unwrap()
            .1
            .selection_digest;
        assert_ne!(first, changed);
    }

    #[test]
    fn root_git_is_skipped_and_nested_git_blocks_unless_excluded() {
        let fixture = selection_fixture();
        let root = &fixture.root;
        write_at(root, ".git/config", "[core]");
        write_at(root, ".git/info/exclude", "from-info.txt\n");
        write_at(root, "from-info.txt", "x");
        write_at(root, "vendor/.git/config", "nested");
        let tracked = Tracked::default();
        let settings = Settings {
            git_common_dir: Some(root.join(".git")),
            ..Settings::default()
        };
        assert!(matches!(
            scan_workspace_report(root, &tracked, &settings),
            Err(Error::Invalid(_))
        ));
        write_at(root, ".agentscm-workspace/exclude", "vendor/\n");
        let (snapshot, report) = scan_workspace_report(root, &tracked, &settings)
            .unwrap()
            .unwrap();
        assert!(!snapshot.files.keys().any(|path| path.starts_with(".git")));
        assert!(!snapshot.files.contains_key("from-info.txt"));
        assert_eq!(
            excluded_of(&report),
            [
                ("from-info.txt".to_owned(), "info/exclude".to_owned()),
                (
                    "vendor".to_owned(),
                    ".agentscm-workspace/exclude".to_owned()
                ),
            ]
        );
    }

    /// The Git these tests drive, resolved as in tests/integration/git_snapshot.rs. A missing Git is a test
    /// failure, never a silent skip.
    fn test_git() -> PathBuf {
        let path = PathBuf::from(
            std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".to_owned()),
        );
        assert!(
            path.exists(),
            "git not found at {}; set AGENTSCM_TEST_GIT to an absolute git path",
            path.display()
        );
        path
    }

    /// Run Git with a cleared environment and an isolated configuration.
    fn git_in(directory: &Path, args: &[&str]) -> String {
        let git = test_git();
        let home = directory.join(".isolated-home");
        fs::create_dir_all(&home).unwrap();
        let output = std::process::Command::new(&git)
            .env_clear()
            .env("PATH", git.parent().unwrap())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("xdg"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(directory)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn files_below(root: &Path, relative: &str, out: &mut Vec<String>) {
        for entry in fs::read_dir(root.join(relative)).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let child = format!("{relative}/{name}");
            if root.join(&child).is_dir() {
                files_below(root, &child, out);
            } else {
                out.push(child);
            }
        }
    }

    /// Compare the selector's excluded files with Git's own ignored files for one fixture.
    fn assert_matches_git(gitignore: &str, files: &[&str], ignore_case: bool) {
        let fixture = selection_fixture();
        let root = &fixture.root;
        git_in(root, &["init", "-q"]);
        git_in(
            root,
            &[
                "config",
                "core.ignorecase",
                if ignore_case { "true" } else { "false" },
            ],
        );
        write_at(root, ".gitignore", gitignore);
        for path in files {
            write_at(root, path, "x");
        }
        let tracked = Tracked::default();
        let settings = Settings {
            git_common_dir: Some(root.join(".git")),
            global_excludes: None,
            case_insensitive: ignore_case,
        };
        let (_, report) = scan_workspace_report(root, &tracked, &settings)
            .unwrap()
            .unwrap();
        let mut ours = Vec::new();
        for entry in &report.excluded {
            if entry.directory {
                files_below(root, &entry.path, &mut ours);
            } else {
                ours.push(entry.path.clone());
            }
        }
        ours.sort();
        let theirs = git_in(
            root,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "-z",
            ],
        );
        let mut theirs: Vec<String> = theirs
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect();
        theirs.sort();
        assert_eq!(ours, theirs, "ignorecase={ignore_case}\n{gitignore}");
    }

    #[test]
    fn selection_matches_git_for_literal_braces_and_glob_forms() {
        let patterns = "secret{a,b}\nbad{\nclose}\n/anchored.txt\ndir-only/\na/**/z\ndocs/**\n*.tmp\n!keep.tmp\n[ab].x\n**/gen/out\n\\#hash\ntrail\\ \n";
        assert_matches_git(
            patterns,
            &[
                "secret{a,b}",
                "secreta",
                "bad{",
                "close}",
                "anchored.txt",
                "sub/anchored.txt",
                "dir-only/f",
                "sub/dir-only",
                "a/x/y/z",
                "a/z",
                "a/q",
                "docs/readme",
                "docs/sub/deep",
                "x.tmp",
                "keep.tmp",
                "a.x",
                "c.x",
                "gen/out",
                "p/gen/out",
                "#hash",
                "trail ",
                "plain.txt",
            ],
            false,
        );
    }

    /// `git check-ignore --no-index --stdin -z` in an isolated repository: the probe paths Git
    /// ignores.
    /// Rule files by directory, and the probe paths to ask about.
    type GitCase = (Vec<(String, String)>, Vec<String>);

    fn git_ignored(patterns: &str, probes: &[String], ignore_case: bool) -> BTreeSet<String> {
        git_ignored_batch(
            &[(vec![(String::new(), patterns.to_owned())], probes.to_vec())],
            ignore_case,
        )
        .pop()
        .expect("one case")
    }

    /// Many cases in one repository: case `i` lives in directory `c{i}` with its own
    /// `.gitignore`, so a single `check-ignore` run answers all of them. Directory probes
    /// (trailing slash) are created for real and asked about by plain name, because Git decides
    /// directory-ness from the file system and would match a trailing slash as part of the path;
    /// files and directories use separate repositories so a name can be both.
    fn git_ignored_batch(cases: &[GitCase], ignore_case: bool) -> Vec<BTreeSet<String>> {
        use std::io::Write as _;
        let git = test_git();
        let mut ignored = vec![BTreeSet::new(); cases.len()];
        for directories in [false, true] {
            let directory = TempDir::new().unwrap();
            let root = directory.path();
            git_in(root, &["init", "-q"]);
            git_in(
                root,
                &[
                    "config",
                    "core.ignorecase",
                    if ignore_case { "true" } else { "false" },
                ],
            );
            let mut input = Vec::new();
            for (index, (layers, probes)) in cases.iter().enumerate() {
                let case_root = root.join(format!("c{index}"));
                fs::create_dir(&case_root).unwrap();
                for (dir, text) in layers {
                    fs::create_dir_all(case_root.join(dir)).unwrap();
                    fs::write(case_root.join(dir).join(".gitignore"), text).unwrap();
                }
                for probe in probes.iter().filter(|p| p.ends_with('/') == directories) {
                    let name = probe.trim_end_matches('/');
                    if directories {
                        fs::create_dir_all(case_root.join(name)).unwrap();
                    }
                    input.extend_from_slice(format!("c{index}/{name}").as_bytes());
                    input.push(0);
                }
            }
            if input.is_empty() {
                continue;
            }
            let mut child = std::process::Command::new(&git)
                .env_clear()
                .env("PATH", git.parent().unwrap())
                .env("HOME", root.join(".isolated-home"))
                .env("XDG_CONFIG_HOME", root.join(".isolated-home/xdg"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["check-ignore", "--no-index", "--stdin", "-z"])
                .current_dir(root)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let writer = std::thread::spawn(move || stdin.write_all(&input));
            let output = child.wait_with_output().unwrap();
            writer.join().unwrap().unwrap();
            // Exit 0: some ignored; 1: none ignored.
            assert!(
                matches!(output.status.code(), Some(0 | 1)),
                "git check-ignore: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            for path in output.stdout.split(|&byte| byte == 0) {
                let Ok(path) = std::str::from_utf8(path) else {
                    continue;
                };
                let Some(rest) = path.strip_prefix('c') else {
                    continue;
                };
                let (index, name) = rest.split_once('/').expect("case prefix");
                let mut name = name.to_owned();
                if directories {
                    name.push('/');
                }
                ignored[index.parse::<usize>().unwrap()].insert(name);
            }
        }
        ignored
    }

    /// The probe paths the selector excludes, walking parent directories as the scan does, or
    /// the error from loading the rules.
    fn selector_excluded(
        patterns: &str,
        probes: &[String],
        ignore_case: bool,
    ) -> Result<BTreeSet<String>> {
        selector_excluded_layers(&[(String::new(), patterns.to_owned())], probes, ignore_case)
    }

    /// Rule files by directory (`""` is the root), loaded as the scan loads them: a directory's
    /// `.gitignore` is read when the walk enters it, and nothing below an excluded directory is
    /// visited.
    fn selector_excluded_layers(
        layers: &[(String, String)],
        probes: &[String],
        ignore_case: bool,
    ) -> Result<BTreeSet<String>> {
        let directory = TempDir::new().unwrap();
        for (dir, text) in layers {
            let name = if dir.is_empty() {
                ".gitignore".to_owned()
            } else {
                format!("{dir}/.gitignore")
            };
            write_at(directory.path(), &name, text);
        }
        let tracked = Tracked::default();
        let settings = Settings {
            case_insensitive: ignore_case,
            ..Settings::default()
        };
        let mut selector = Selector::open(directory.path(), &tracked, &settings)?;
        let root_pushed = selector.enter_directory(directory.path(), "")?;
        let mut excluded = BTreeSet::new();
        for probe in probes {
            let mut inherited: Option<Rule> = None;
            let (trimmed, is_directory) = match probe.strip_suffix('/') {
                Some(trimmed) => (trimmed, true),
                None => (probe.as_str(), false),
            };
            let components: Vec<&str> = trimmed.split('/').collect();
            let mut prefix = PathBuf::new();
            let mut pushed = 0;
            let mut outcome = Ok(());
            for (position, component) in components.iter().enumerate() {
                prefix.push(component);
                let last = position + 1 == components.len();
                let classified =
                    selector.classify(&prefix, !last || is_directory, inherited.as_ref());
                if matches!(classified.selection, Selection::Excluded(_)) {
                    excluded.insert(probe.clone());
                    break;
                }
                inherited = classified.child_exclusion;
                if !last {
                    match selector
                        .enter_directory(&directory.path().join(&prefix), prefix.to_str().unwrap())
                    {
                        Ok(true) => pushed += 1,
                        Ok(false) => {}
                        Err(error) => {
                            outcome = Err(error);
                            break;
                        }
                    }
                }
            }
            for _ in 0..pushed {
                selector.leave_directory();
            }
            outcome?;
        }
        if root_pushed {
            selector.leave_directory();
        }
        Ok(excluded)
    }

    enum Expect {
        /// The parser accepts the rules and the selector must equal Git.
        Equal,
        /// The parser refuses the rules with a typed error naming file and line.
        Invalid,
    }

    /// Every adversarial pattern raised in review. `\u{a7}` in a shape expands to each printable
    /// ASCII character except `/`.
    #[test]
    fn selector_matches_git_check_ignore_for_adversarial_patterns() {
        use Expect::{Equal, Invalid};
        let class_shape = ["k\u{a7}.key"];
        let class_extras = ["k.key", "k/.key", "k/x.key", "kab.key", "k/a.key"];
        let mut table: Vec<(String, Vec<&str>, Vec<&str>, Expect)> = Vec::new();
        for class in [
            "[]]",
            "[!]a]",
            "[^]a]",
            "[\\]]",
            "[a\\-z]",
            "[[:alpha:]-z]",
            "[[:digit:][:alpha:]]",
            "[[:punct:]]",
            "[A-{]",
            "[z-a]",
            "[a-c-e]",
            "[/]",
            "[a/b]",
            "[a",
            "[",
            "[a\\]",
            "[[:alpha:]",
            "[[:bogus:]]",
            "[[:alpha:]",
            "[!a]",
            "[^a-c]",
            "[!/]",
            "[a-]",
            "[-a]",
            "[\\-a]",
            "[a-\\]]",
            "[[:]",
            "[[:a]",
            "[[]",
            "[[a]",
            "[[:space:]]",
            "[[:blank:]]",
            "[[:cntrl:]]",
            "[[:graph:]]",
            "[[:print:]]",
            "[[:xdigit:]]",
            "[[:upper:]]",
            "[[:lower:]]",
            "[[:alnum:]]",
            "[!]]",
            "[^[:alpha:]]",
            "[a-a]",
            "[\\a-c]",
            "[a-c\\-e]",
        ] {
            table.push((
                format!("k{class}.key\n"),
                class_shape.to_vec(),
                class_extras.to_vec(),
                Equal,
            ));
        }
        table.push((
            "k[\u{e9}].key\n".into(),
            vec![],
            vec!["k\u{e9}.key", "ka.key", "k\u{c3}.key"],
            Equal,
        ));
        // Classes with non-ASCII range endpoints are refused.
        for class in ["[\u{e9}-z]", "[a-\u{e9}]"] {
            table.push((format!("k{class}.key\n"), vec![], vec!["ka.key"], Invalid));
        }
        table.push((
            "a[/]b\n".into(),
            vec!["a\u{a7}b"],
            vec!["a/b", "a/x/b", "ab"],
            Equal,
        ));
        for (patterns, shapes, extras) in [
            ("ka[\n", vec!["ka\u{a7}"], vec!["ka[", "ka"]),
            ("k\\{.key\n", vec!["k\u{a7}.key"], vec![]),
            ("k\\[a].key\n", vec!["k\u{a7}a].key"], vec!["k[a].key"]),
            (
                "k{a,b}.key\n",
                vec!["k\u{a7}.key"],
                vec!["k{a,b}.key", "ka.key", "kb.key", "k{a.key"],
            ),
            ("k{.key\n", vec!["k\u{a7}.key"], vec![]),
            ("k\\\n", vec!["k\u{a7}"], vec!["k", "k\\"]),
            ("!k\\\n*.key\n", vec!["k\u{a7}.key"], vec![]),
            ("\\!k.key\n", vec!["\u{a7}k.key"], vec!["!k.key"]),
            ("\\#k.key\n", vec!["\u{a7}k.key"], vec!["#k.key"]),
            ("k\\*.key\n", vec!["k\u{a7}.key"], vec!["k*.key", "kab.key"]),
            ("k\\?.key\n", vec!["k\u{a7}.key"], vec!["k?.key"]),
            ("**\n", vec![], vec!["a", "a/b", "a/b/c.key"]),
            (
                "**/k.key\n",
                vec![],
                vec!["k.key", "a/k.key", "a/b/k.key", "xk.key"],
            ),
            ("x/a[!q]b\n", vec![], vec!["x/a/b", "x/aab", "x/aqb"]),
            ("x/a[^q]b\n", vec![], vec!["x/a/b", "x/aab", "x/aqb"]),
            ("x/a[!/]b\n", vec![], vec!["x/a/b", "x/aab"]),
            ("x/a?b\n", vec![], vec!["x/a/b", "x/aab"]),
            ("x/a*b\n", vec![], vec!["x/a/b", "x/aab", "x/ab"]),
            ("k/**\n", vec![], vec!["k", "k/a", "k/a/b", "kk/a"]),
            (
                "a/**/b.key\n",
                vec![],
                vec!["a/b.key", "a/x/b.key", "a/x/y/b.key", "ab.key", "x/a/b.key"],
            ),
            ("k*.key\n", vec!["k\u{a7}x.key"], vec!["k.key", "k/x.key"]),
            ("k?.key\n", vec!["k\u{a7}.key"], vec!["k/.key", "kab.key"]),
            ("/k.key\n", vec![], vec!["k.key", "a/k.key"]),
            ("a/k.key\n", vec![], vec!["a/k.key", "b/a/k.key", "k.key"]),
            ("k/\n", vec![], vec!["k", "k/a", "x/k/a"]),
            // Whitespace.
            (".env\n!.env\t\n", vec![], vec![".env", ".env\t"]),
            ("a\\\t\n", vec![], vec!["a\t", "a", "a "]),
            ("a\\ \n", vec![], vec!["a ", "a", "a  "]),
            ("a \n", vec![], vec!["a ", "a"]),
            ("a  \t\n", vec![], vec!["a  \t", "a", "a  "]),
            ("a\\\\ \n", vec![], vec!["a\\", "a\\ ", "a"]),
            ("a\u{a0}b\n", vec![], vec!["a\u{a0}b", "ab"]),
            ("a b\n", vec![], vec!["a b", "ab"]),
            // Negation pairs.
            (
                "*.key\n!k[[:digit:]-z].key\n",
                vec!["k\u{a7}.key"],
                vec!["k.key"],
            ),
            (
                "*.key\n!k[.key\n",
                vec!["k\u{a7}.key"],
                vec!["k[.key", "k.key"],
            ),
            ("*.key\n!k[a-c].key\n!kb.key\n", vec!["k\u{a7}.key"], vec![]),
        ] {
            table.push((patterns.to_owned(), shapes, extras, Equal));
        }
        for patterns in [
            "a\u{a0}\n",
            "a\u{3000}\n",
            "a\\\u{a0}\n",
            "foo/\n!foo\\/\n",
            "/\n",
            "//\n",
            "*/\n",
            " \n",
            "foo//\n",
            "//foo\n",
            "a//b\n",
            "\\\n",
            "!\n",
            "!!foo\n",
            "k[!a].key\n",
            "*.key\n!k[a/b].key\n",
            "k[!A].key\n",
            "*.key\n!k[A].key\n",
            "k[a-A].key\n",
            "K[b].key\n",
            "\u{c4}.key\n",
        ] {
            table.push((
                patterns.to_owned(),
                vec![],
                vec![
                    "a/b.key",
                    "a",
                    "foo",
                    "foo/x",
                    "x/foo",
                    "k",
                    "k.key",
                    "ka.key",
                    "kb.key",
                    "kA.key",
                    "kB.key",
                    "ab",
                    "a/",
                    "x/a/b.key",
                    "\u{e4}.key",
                    "\u{c4}.key",
                    "ab/b.key",
                    "aXY/b.key",
                    "x/a",
                    "x/ab",
                    "x/a/c",
                ],
                Equal,
            ));
        }
        // A run of two or more stars that is not a whole path segment reads differently across
        // Git versions and is refused, as is `**` before an escaped slash.
        for patterns in [
            "a**/b.key\n",
            "a/***/b.key\n",
            "***\n",
            "k**\n",
            "**k\n",
            "k**/\n",
            "x/a**\n",
            "**/k**\n",
            ".*/***b\n",
            "a**/\\b\n",
            "?**/b\n",
            "a/**b\n",
            "a**\n",
            "**/**a\n",
        ] {
            table.push((patterns.to_owned(), vec![], vec!["a/b.key"], Invalid));
        }
        for patterns in ["a/**\\/b.key\n", "/**\\/[a-b]a\n", "**\\/b\n"] {
            table.push((patterns.to_owned(), vec![], vec!["a/b.key"], Invalid));
        }
        // A NUL cannot be part of a rule and is refused with its line.
        table.push(("a\0b\n".to_owned(), vec![], vec!["a"], Invalid));

        let mut total_git_exclusions = 0;
        let printable: Vec<char> = (' '..='~').filter(|c| *c != '/').collect();
        for ignore_case in [false, true] {
            let mut compared: Vec<(String, Vec<String>, BTreeSet<String>)> = Vec::new();
            for (patterns, shapes, extras, expect) in &table {
                let mut probes: BTreeSet<String> =
                    extras.iter().map(|probe| (*probe).to_owned()).collect();
                for shape in shapes {
                    for c in &printable {
                        probes.insert(shape.replace('\u{a7}', &c.to_string()));
                    }
                }
                // Every probe also appears nested, and as a directory (trailing slash).
                let base: Vec<String> = probes.iter().cloned().collect();
                for probe in &base {
                    for prefix in ["dir/", "a-/", "sub/deep/"] {
                        probes.insert(format!("{prefix}{probe}"));
                    }
                    if !probe.ends_with('/') {
                        probes.insert(format!("{probe}/"));
                    }
                }
                let probes: Vec<String> = probes.into_iter().collect();
                let context = format!("{patterns:?} ignorecase={ignore_case}");
                let ours = selector_excluded(patterns, &probes, ignore_case);
                match expect {
                    Invalid => assert!(
                        matches!(&ours, Err(Error::Invalid(message)) if message.contains(".gitignore") && message.contains("line")),
                        "{context}: {ours:?}"
                    ),
                    Equal => {
                        let ours = ours.unwrap_or_else(|error| panic!("{context}: {error}"));
                        compared.push((patterns.clone(), probes, ours));
                    }
                }
            }
            let cases: Vec<GitCase> = compared
                .iter()
                .map(|(patterns, probes, _)| {
                    (vec![(String::new(), patterns.clone())], probes.clone())
                })
                .collect();
            for ((patterns, _, ours), theirs) in
                compared.iter().zip(git_ignored_batch(&cases, ignore_case))
            {
                total_git_exclusions += theirs.len();
                // Never fail open: whatever Git excludes, the selector excludes, and no more.
                let missed: Vec<_> = theirs.difference(ours).collect();
                let extra: Vec<_> = ours.difference(&theirs).collect();
                assert!(
                    missed.is_empty() && extra.is_empty(),
                    "{patterns:?} ignorecase={ignore_case}\nGit excludes but selector includes: {missed:?}\nselector excludes but Git includes: {extra:?}"
                );
            }
        }
        // Guard against a vacuous comparison: the table makes Git exclude many probes.
        assert!(total_git_exclusions > 500, "{total_git_exclusions}");
    }

    /// A fixed-seed xorshift generator: the corpus is the same on every run.
    struct Prng(u64);

    impl Prng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    /// Seeded random rule files (root, `sub/` and `sub/deep/` layers) against `git check-ignore`:
    /// every rule set the parser accepts must select exactly what Git does (files, directories,
    /// nested paths, both case modes), and every one it rejects must be a typed error.
    #[test]
    fn seeded_fuzz_patterns_match_git_check_ignore() {
        // Two alphabets: general punctuation, classes and carriage returns, and star/slash
        // heavy forms.
        #[rustfmt::skip]
        const GENERAL: [&str; 26] = [
            "a", "b", "A", "-", "]", "[", "!", "^", "\\", "*", "?", "/", ":", "{", "}", ",", ".",
            " ", "\t", "\r", "**", "[:alpha:]", "[:upper:]", "[:digit:]", "[:punct:]", "[:bogus:]",
        ];
        #[rustfmt::skip]
        const STARS: [&str; 27] = [
            "a", "b", "a", "b", "*", "*", "**", "/", "/", "/", "?", "!", "\\", "/", "*", "a", "b",
            "A", "[a]", "[!a]", "**/", "/**", "*/", "-", "[a-b]", ".", "\r",
        ];
        #[rustfmt::skip]
        let probes: Vec<String> = [
            "a", "b", "A", "B", "ab", "a-", "-", "]", "[", "!", "^", "\\", ".a", "a.b", "a b",
            "{", "}", ",", "x", "a/a", "a/b", "A/b", "b/a", "a/b/a", "a-/a", "ab/a", "[/a",
            "a/ b", "a/!", "a/-", "-/a", "]/a", "x/y/z", "a/a/a", "b/b", "a/B", "A/a", "dir/ab",
            "sub/a", "sub/b", "sub/A", "sub/ab", "sub/a/b", "sub/deep/a", "sub/deep/b",
            "sub/deep/ab", "sub/deep/A/b", "sub/x/a",
            "a/", "b/", "A/", "ab/", "a-/", "-/", "]/", "[/", "!/", "^/", ".a/", "x/", "a/a/",
            "a/b/", "A/b/", "b/a/", "a/b/a/", "a-/a/", "x/y/", "x/y/z/", "a b/", "a/-/", "{/",
            "}/", ",/", "sub/", "sub/deep/", "dir/", "dir/ab/", "sub/a/", "sub/deep/a/",
            "sub/deep/A/",
        ]
        .iter()
        .map(|probe| (*probe).to_owned())
        .collect();
        assert!(probes.len() >= 60);
        let mut cases: Vec<Vec<(String, String)>> = Vec::new();
        for (seed, tokens) in [
            (0x9e37_79b9_7f4a_7c15, &GENERAL[..]),
            (0x5151_5151_aaaa_bbbb, &STARS[..]),
        ] {
            let mut prng = Prng(seed);
            for _ in 0..800 {
                let file = |prng: &mut Prng| -> String {
                    let line = |prng: &mut Prng| -> String {
                        (0..1 + prng.below(7))
                            .map(|_| tokens[prng.below(tokens.len())])
                            .collect()
                    };
                    let mut text = line(prng);
                    if prng.below(5) == 0 {
                        text.push('\n');
                        text.push_str(&line(prng));
                    }
                    // Sometimes no final newline, so a trailing carriage return ends the file.
                    if prng.below(8) != 0 {
                        text.push('\n');
                    }
                    text
                };
                let mut layers = vec![(String::new(), file(&mut prng))];
                if prng.below(2) == 0 {
                    layers.push(("sub".to_owned(), file(&mut prng)));
                }
                if prng.below(3) == 0 {
                    layers.push(("sub/deep".to_owned(), file(&mut prng)));
                }
                cases.push(layers);
            }
        }
        let mut accepted = 0;
        let mut rejected = 0;
        let mut compared = 0;
        let mut git_exclusions = 0;
        for ignore_case in [false, true] {
            let mut batch = Vec::new();
            let mut ours = Vec::new();
            for layers in &cases {
                match selector_excluded_layers(layers, &probes, ignore_case) {
                    Ok(excluded) => {
                        accepted += 1;
                        batch.push((layers.clone(), probes.clone()));
                        ours.push(excluded);
                    }
                    Err(Error::Invalid(message)) => {
                        assert!(message.contains(".gitignore"), "{layers:?}: {message}");
                        rejected += 1;
                    }
                    Err(error) => panic!("{layers:?}: {error}"),
                }
            }
            let theirs = git_ignored_batch(&batch, ignore_case);
            for ((case, ours), theirs) in batch.iter().zip(&ours).zip(&theirs) {
                compared += 1;
                git_exclusions += theirs.len();
                let missed: Vec<_> = theirs.difference(ours).collect();
                let extra: Vec<_> = ours.difference(theirs).collect();
                assert!(
                    missed.is_empty() && extra.is_empty(),
                    "{:?} ignorecase={ignore_case}\nGit excludes but selector includes: {missed:?}\nselector excludes but Git includes: {extra:?}",
                    case.0
                );
            }
        }
        eprintln!(
            "fuzz: {} rule sets x 2 modes; accepted {accepted}, rejected {rejected}, compared {compared}, git exclusions {git_exclusions}",
            cases.len()
        );
        assert!(compared >= 2000 && git_exclusions > 5000);
    }

    /// Git's measured handling of reversed and chained ranges, which the matcher implements.
    #[test]
    fn git_treats_reversed_and_chained_ranges_as_the_matcher_does() {
        let probes: Vec<String> = ["a", "c", "d", "e", "f", "z", "-", "b"]
            .iter()
            .map(|c| format!("k{c}.key"))
            .collect();
        // `[z-a]` is an empty range, but its first character is still a member: only `z`.
        let reversed = git_ignored("k[z-a].key\n", &probes, false);
        assert_eq!(reversed, BTreeSet::from(["kz.key".to_owned()]));
        // `[a-c-e]` is the range a-c, then a literal `-`, then a literal `e`.
        let chained = git_ignored("k[a-c-e].key\n", &probes, false);
        let expected: BTreeSet<String> = ["a", "b", "c", "-", "e"]
            .iter()
            .map(|c| format!("k{c}.key"))
            .collect();
        assert_eq!(chained, expected);
        // A POSIX class cannot start a range, so the `-` after it is a literal member.
        let after_class = git_ignored("k[[:digit:]-z].key\n", &probes, false);
        assert!(after_class.contains("k-.key") && after_class.contains("kz.key"));
        assert!(!after_class.contains("kd.key"));
    }

    #[test]
    fn selection_matches_git_for_trailing_whitespace_rules() {
        // A whitelist ending in a tab names a different file; escaped spaces are kept.
        let patterns = ".env\n!.env\t\na\\ \nb\\\\ \nc  \nd\\ \t\n";
        assert_matches_git(patterns, &[".env", "a ", "b\\", "c", "d \t"], false);
    }

    #[test]
    fn selection_matches_git_in_both_case_modes() {
        let patterns = ".env\n!.ENV\n*.TMP\n[A-C]x\nDocs/\n";
        let files = [
            ".env", ".ENV", "a.tmp", "b.TMP", "ax", "Bx", "dx", "Docs/f", "docs/f", "other",
        ];
        // One file per fixture: names that differ only in case would collide on a case-folding
        // file system, and the scan reports that collision.
        for ignore_case in [false, true] {
            for file in files {
                assert_matches_git(patterns, &[file], ignore_case);
            }
        }
    }

    #[test]
    fn a_linked_worktree_gitfile_resolves_the_main_repository_exclude() {
        let fixture = selection_fixture();
        let parent = fixture.root.parent().unwrap().to_owned();
        let main = parent.join("main");
        fs::create_dir(&main).unwrap();
        git_in(&main, &["init", "-q"]);
        write_at(&main, "tracked.txt", "x");
        git_in(&main, &["add", "tracked.txt"]);
        git_in(&main, &["commit", "-q", "-m", "init"]);
        let linked = parent.join("linked");
        git_in(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
        write_at(&main, ".git/info/exclude", "from-main-info.txt\n");
        fs::copy(linked.join(".git"), fixture.root.join(".git")).unwrap();
        for name in ["from-main-info.txt", "plain.txt"] {
            write_at(&fixture.root, name, "x");
            write_at(&linked, name, "x");
        }
        // The common directory as Git reports it for the linked worktree.
        let common = git_in(
            &linked,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        let settings = Settings {
            git_common_dir: Some(PathBuf::from(common.trim_end_matches('\n'))),
            ..Settings::default()
        };
        let tracked = Tracked::default();
        let (snapshot, report) = scan_workspace_report(&fixture.root, &tracked, &settings)
            .unwrap()
            .unwrap();
        let theirs = git_in(
            &linked,
            &["ls-files", "--others", "--ignored", "--exclude-standard"],
        );
        let ours: Vec<&str> = report.excluded.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(ours, theirs.lines().collect::<Vec<_>>());
        assert_eq!(ours, ["from-main-info.txt"]);
        assert!(snapshot.files.contains_key("plain.txt"));
    }

    #[test]
    fn apply_is_blocked_by_an_excluded_local_file_at_a_path_it_would_add() {
        let fixtures = TempDir::new().unwrap();
        let engine = Engine::open(fixtures.path().join("store")).unwrap();
        let base = engine
            .checkpoint(None, &[put("changed.txt", b"base")])
            .unwrap();
        let right = engine
            .checkpoint(Some(&base), &[put("added.txt", b"theirs")])
            .unwrap();
        let merged = engine
            .checkpoint_with_parents(&[base.clone(), right], &[put("added.txt", b"theirs")])
            .unwrap();
        let root = fixtures.path().join("workspace");
        let mut workspace = Workspace::create(
            &engine,
            &root,
            ChangeId {
                repository: "repository-1".to_owned(),
                id: "change-1".to_owned(),
            },
            &base,
        )
        .unwrap();
        // The user's own excluded file holds exactly the bytes apply would write.
        write_at(&root, ".agentscm-workspace/exclude", "added.txt\n");
        write_at(&root, "added.txt", "theirs");
        let completion = crate::merge::MergeCompletion {
            repository: "repository-1".to_owned(),
            session: "a".repeat(64),
            generation: 7,
            left: base,
            revision: merged,
        };
        assert_eq!(
            workspace.apply(&engine, "apply-1", 0, &completion).unwrap(),
            OperationOutcome::Blocked {
                paths: vec!["added.txt".to_owned()]
            }
        );
        let state = workspace.state().unwrap();
        assert!(state.pending.is_none());
        assert_eq!(state.generation, 0);
        assert_eq!(fs::read(root.join("added.txt")).unwrap(), b"theirs");
    }

    #[test]
    fn excluded_entries_do_not_count_toward_the_selected_entry_bound() {
        let fixture = selection_fixture();
        write_at(&fixture.root, ".agentscm-workspace/exclude", "*.skip\n");
        for index in 0..300 {
            write_at(&fixture.root, &format!("top{index}.skip"), "x");
        }
        let tracked = Tracked::default();
        let (snapshot, report) =
            scan_workspace_report(&fixture.root, &tracked, &Settings::default())
                .unwrap()
                .unwrap();
        assert_eq!(report.excluded_total, 300);
        assert_eq!(snapshot.files.len(), 1, "only the base file a.txt");
        assert_eq!(snapshot.directories.len(), 0);
    }

    #[test]
    fn ambient_settings_probe_case_for_native_workspaces_without_running_git() {
        let fixture = selection_fixture();
        // No `.git`: PATH is irrelevant because Git is never started.
        let settings = git_config::ambient_settings(&fixture.root, || {
            probe_case_insensitive(&fixture.root.join(METADATA_DIRECTORY))
        })
        .unwrap();
        assert!(settings.global_excludes.is_none());
        let probes = fs::read_dir(fixture.root.join(METADATA_DIRECTORY))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(PROBE_PREFIX)
            })
            .count();
        assert_eq!(probes, 0);
    }
}
