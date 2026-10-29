use std::fs::{self, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::{
    METADATA_DIRECTORY, Scope, Snapshot, StoredState, Tracked, Workspace, absolute_path,
    decode_optional_scope, encode_optional_scope, pending_operation, read_scope, read_state,
    require_complete_snapshot, scan_workspace_full_scoped_with_probe, source_snapshot_scoped,
    sql_i64, sql_u64, staged_between,
};
use crate::context::ChangeId;
use crate::lease::Lease;
use crate::model::{EntryChange, EntryKind};
use crate::store::{Engine, validate_id};
use crate::{Error, Result};

pub(crate) const TIMELINE_LEASE_FILE: &str = ".agentscm-timeline.lock";
pub(crate) const TIMELINE_PROBES_DIRECTORY: &str = "timeline-probes";
const MAX_TIMELINE_ENTRIES: u64 = 4_096;
const MAX_ADMITTED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_TIMELINE_PAGE: usize = 100;
const MAX_TARGET_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotTrigger {
    Manual,
    TurnEnd,
    CheckStart,
    McpExplicit,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapturePause {
    EntryLimit,
    ByteLimit,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSkip {
    CaptureBusy,
    WorkspaceIncomplete,
    OperationPending,
    WorkspaceChanged,
    ChangeMismatch,
    Deadline,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureResult {
    Captured { entry: SnapshotEntry },
    Unchanged { revision: String },
    Paused { reason: CapturePause },
    Skipped { reason: CaptureSkip },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckLinkRequest {
    pub source_revision: String,
    pub tested_revision: String,
    pub change: ChangeId,
    pub change_version: u64,
    pub context_digest: String,
    pub policy_epoch: u64,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_version: Option<u64>,
    pub requirement_digest: String,
    pub run_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckLinkResult {
    Recorded { entry: CheckLinkEntry },
    AlreadyRecorded { entry: CheckLinkEntry },
    Paused { reason: CapturePause },
    Skipped { reason: CaptureSkip },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckEvidenceCoverage {
    ReferenceOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEntry {
    pub id: u64,
    pub trigger: SnapshotTrigger,
    pub recorded_at: u64,
    pub revision: String,
    pub base_revision: String,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    pub changed_paths: u64,
    pub admitted_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckLinkEntry {
    pub id: u64,
    pub recorded_at: u64,
    pub source_revision: String,
    pub tested_revision: String,
    pub base_revision: String,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    pub change: ChangeId,
    pub change_version: u64,
    pub context_digest: String,
    pub policy_epoch: u64,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_version: Option<u64>,
    pub requirement_digest: String,
    pub run_id: String,
    pub evidence_coverage: CheckEvidenceCoverage,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimelineEntry {
    Snapshot(SnapshotEntry),
    CheckLink(CheckLinkEntry),
}

impl TimelineEntry {
    pub fn id(&self) -> u64 {
        match self {
            Self::Snapshot(entry) => entry.id,
            Self::CheckLink(entry) => entry.id,
        }
    }

    /// The captured revision, or the source revision associated with a check run.
    pub fn revision(&self) -> &str {
        match self {
            Self::Snapshot(entry) => &entry.revision,
            Self::CheckLink(entry) => &entry.source_revision,
        }
    }

    /// Exact effective scope at record time. `None` is a whole-tree workspace.
    pub fn scope(&self) -> Option<&[String]> {
        match self {
            Self::Snapshot(entry) => entry.scope.as_deref(),
            Self::CheckLink(entry) => entry.scope.as_deref(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TimelineCoverage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest_recorded_id: Option<u64>,
    pub includes_pre_feature_history: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TimelinePage {
    pub entries: Vec<TimelineEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before: Option<u64>,
    pub coverage: TimelineCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CaptureFence {
    state: StoredState,
    scope: Option<Scope>,
}

struct CaptureProbeDirectory {
    path: PathBuf,
}

impl CaptureProbeDirectory {
    /// The caller holds the timeline lease. This fixed child stays on the workspace filesystem
    /// while isolating capture probes from legacy scans that sweep the metadata directory.
    fn create(root: &Path) -> Result<Self> {
        let path = root
            .join(METADATA_DIRECTORY)
            .join(TIMELINE_PROBES_DIRECTORY);
        crate::local_recovery::create_private_directory(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CaptureProbeDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

impl Workspace {
    pub fn capture(&self, engine: &Engine, trigger: SnapshotTrigger) -> Result<CaptureResult> {
        self.capture_inner(engine, trigger, None)
    }

    /// Capture with a caller-owned wall-clock allowance. A deadline is checked after the
    /// bounded scan and again before the row insert; expiry never publishes or acknowledges
    /// a snapshot after the corresponding check.
    pub fn capture_with_deadline(
        &self,
        engine: &Engine,
        trigger: SnapshotTrigger,
        deadline: Duration,
    ) -> Result<CaptureResult> {
        self.capture_inner(engine, trigger, Some(deadline))
    }

    fn capture_inner(
        &self,
        engine: &Engine,
        trigger: SnapshotTrigger,
        deadline: Option<Duration>,
    ) -> Result<CaptureResult> {
        let started = Instant::now();
        if self.read_only {
            return Err(Error::Conflict(
                "a read-only workspace cannot record snapshots".into(),
            ));
        }
        let Some(_lease) = self.try_timeline_lease()? else {
            return Ok(CaptureResult::Skipped {
                reason: CaptureSkip::CaptureBusy,
            });
        };

        let Some(fence) = self.capture_fence()? else {
            return Ok(CaptureResult::Skipped {
                reason: CaptureSkip::OperationPending,
            });
        };
        if !fence.state.ready {
            return Ok(CaptureResult::Skipped {
                reason: CaptureSkip::WorkspaceIncomplete,
            });
        }
        let base = require_complete_snapshot(source_snapshot_scoped(
            engine,
            &fence.state.base_revision,
            fence.scope.as_ref(),
        )?)?;
        let tracked = Tracked::from_paths(base.files.keys().map(String::as_str));
        let probe_directory = CaptureProbeDirectory::create(&self.root)?;
        let working = require_complete_snapshot(
            scan_workspace_full_scoped_with_probe(
                &self.root,
                &tracked,
                fence.scope.as_ref(),
                probe_directory.path(),
                true,
            )?
            .map(|(snapshot, _)| snapshot),
        )?;
        drop(probe_directory);
        let staged = staged_between(&base.files, &working.files);
        if staged.is_empty() {
            return Ok(CaptureResult::Unchanged {
                revision: fence.state.base_revision,
            });
        }
        let admitted_bytes = admitted_bytes(&staged)?;
        let changed_paths = u64::try_from(staged.len())
            .map_err(|_| Error::Limit("snapshot path count does not fit u64".into()))?;

        let connection = self.connection()?;
        if let Some((revision, base_revision, scope)) = last_snapshot(&connection)?
            && base_revision == fence.state.base_revision
            && scope == fence.scope
            && snapshot_matches(engine, &revision, fence.scope.as_ref(), &working)?
        {
            return Ok(CaptureResult::Unchanged { revision });
        }
        if let Some(reason) = quota_pause(&connection, admitted_bytes)? {
            return Ok(CaptureResult::Paused { reason });
        }
        if deadline_expired(started, deadline) {
            return Ok(CaptureResult::Skipped {
                reason: CaptureSkip::Deadline,
            });
        }
        if let Some(reason) = self.fence_change(&fence)? {
            return Ok(CaptureResult::Skipped { reason });
        }

        let changes: Vec<EntryChange> = staged
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
        let revision = engine.checkpoint_entries(Some(&fence.state.base_revision), &changes)?;

        self.insert_snapshot_entry(
            trigger,
            started,
            deadline,
            &fence,
            &revision,
            changed_paths,
            admitted_bytes,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_snapshot_entry(
        &self,
        trigger: SnapshotTrigger,
        started: Instant,
        deadline: Option<Duration>,
        fence: &CaptureFence,
        revision: &str,
        changed_paths: u64,
        admitted_bytes: u64,
    ) -> Result<CaptureResult> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(reason) = fence_change(&tx, fence)? {
            return Ok(CaptureResult::Skipped { reason });
        }
        if let Some((previous, base_revision, scope)) = last_snapshot(&tx)?
            && previous == revision
            && base_revision == fence.state.base_revision
            && scope == fence.scope
        {
            return Ok(CaptureResult::Unchanged { revision: previous });
        }
        if let Some(reason) = quota_pause(&tx, admitted_bytes)? {
            return Ok(CaptureResult::Paused { reason });
        }
        if deadline_expired(started, deadline) {
            return Ok(CaptureResult::Skipped {
                reason: CaptureSkip::Deadline,
            });
        }
        tx.execute(
            "INSERT INTO timeline_entries(
                kind,trigger,recorded_at,source_revision,base_revision,generation,
                scope_json,changed_paths,admitted_bytes
             ) VALUES ('snapshot',?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                trigger_name(trigger),
                sql_i64("snapshot time", now()?)?,
                revision,
                fence.state.base_revision,
                sql_i64("workspace generation", fence.state.generation)?,
                encode_optional_scope(fence.scope.as_ref())?,
                sql_i64("snapshot changed paths", changed_paths)?,
                sql_i64("snapshot admitted bytes", admitted_bytes)?,
            ],
        )?;
        let id = sql_u64("timeline entry ID", tx.last_insert_rowid())?;
        let entry = read_entry(&tx, id)?;
        tx.commit()?;
        match entry {
            TimelineEntry::Snapshot(entry) => Ok(CaptureResult::Captured { entry }),
            TimelineEntry::CheckLink(_) => Err(Error::Invalid(
                "new snapshot row decoded as a check link".into(),
            )),
        }
    }

    pub fn record_check_start(
        &self,
        engine: &Engine,
        request: &CheckLinkRequest,
    ) -> Result<CheckLinkResult> {
        if self.read_only {
            return Err(Error::Conflict(
                "a read-only workspace cannot record check links".into(),
            ));
        }
        validate_check_request(engine, request)?;
        let connection = self.connection()?;
        if let Some(entry) = matching_check_link(&connection, request)? {
            return Ok(CheckLinkResult::AlreadyRecorded { entry });
        }
        drop(connection);
        let Some(_lease) = self.try_timeline_lease()? else {
            return Ok(CheckLinkResult::Skipped {
                reason: CaptureSkip::CaptureBusy,
            });
        };
        let connection = self.connection()?;
        if let Some(reason) = quota_pause(&connection, 0)? {
            return Ok(CheckLinkResult::Paused { reason });
        }
        let fence = match self.check_link_fence(&connection, request)? {
            Ok(fence) => fence,
            Err(reason) => return Ok(CheckLinkResult::Skipped { reason }),
        };

        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(entry) = matching_check_link(&tx, request)? {
            return Ok(CheckLinkResult::AlreadyRecorded { entry });
        }
        if let Some(reason) = quota_pause(&tx, 0)? {
            return Ok(CheckLinkResult::Paused { reason });
        }
        match self.check_link_fence(&tx, request)? {
            Ok(current) if current == fence => {}
            Ok(_) => {
                return Ok(CheckLinkResult::Skipped {
                    reason: CaptureSkip::WorkspaceChanged,
                });
            }
            Err(reason) => return Ok(CheckLinkResult::Skipped { reason }),
        }
        tx.execute(
            "INSERT INTO timeline_entries(
                kind,trigger,recorded_at,source_revision,base_revision,generation,scope_json,
                tested_revision,change_repository,change_id,change_version,context_digest,
                policy_epoch,target,target_revision,target_version,requirement_digest,run_id
             ) VALUES ('check_link','check_start',?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                sql_i64("check link time", now()?)?,
                request.source_revision,
                fence.state.base_revision,
                sql_i64("workspace generation", fence.state.generation)?,
                encode_optional_scope(fence.scope.as_ref())?,
                request.tested_revision,
                request.change.repository,
                request.change.id,
                sql_i64("change version", request.change_version)?,
                request.context_digest,
                sql_i64("policy epoch", request.policy_epoch)?,
                request.target,
                request.target_revision,
                request
                    .target_version
                    .map(|value| sql_i64("target version", value))
                    .transpose()?,
                request.requirement_digest,
                request.run_id,
            ],
        )?;
        let id = sql_u64("timeline entry ID", tx.last_insert_rowid())?;
        let entry = read_entry(&tx, id)?;
        tx.commit()?;
        match entry {
            TimelineEntry::CheckLink(entry) => Ok(CheckLinkResult::Recorded { entry }),
            TimelineEntry::Snapshot(_) => Err(Error::Invalid(
                "new check link row decoded as a snapshot".into(),
            )),
        }
    }

    pub fn timeline(&self, limit: usize, before: Option<u64>) -> Result<TimelinePage> {
        if !(1..=MAX_TIMELINE_PAGE).contains(&limit) {
            return Err(Error::Limit(format!(
                "timeline limit must be between 1 and {MAX_TIMELINE_PAGE}"
            )));
        }
        if before == Some(0) {
            return Err(Error::Invalid("timeline cursor must be positive".into()));
        }
        let connection = self.connection()?;
        if workspace_schema_version(&connection)? == 4 {
            return Ok(TimelinePage {
                entries: Vec::new(),
                next_before: None,
                coverage: TimelineCoverage {
                    earliest_recorded_id: None,
                    includes_pre_feature_history: false,
                },
            });
        }
        let earliest = connection
            .query_row("SELECT MIN(sequence) FROM timeline_entries", [], |row| {
                row.get::<_, Option<i64>>(0)
            })?
            .map(|value| sql_u64("timeline entry ID", value))
            .transpose()?;
        let bound = before
            .map(|value| sql_i64("timeline cursor", value))
            .transpose()?
            .unwrap_or(i64::MAX);
        let requested = i64::try_from(limit + 1)
            .map_err(|_| Error::Limit("timeline page size does not fit SQLite".into()))?;
        let mut statement = connection.prepare(
            "SELECT sequence FROM timeline_entries
             WHERE sequence < ?1 ORDER BY sequence DESC LIMIT ?2",
        )?;
        let ids = statement
            .query_map(params![bound, requested], |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let has_more = ids.len() > limit;
        let mut entries = Vec::with_capacity(limit.min(ids.len()));
        for id in ids.into_iter().take(limit) {
            entries.push(read_entry(&connection, sql_u64("timeline entry ID", id)?)?);
        }
        let next_before = has_more.then(|| entries.last().expect("full page has an entry").id());
        Ok(TimelinePage {
            entries,
            next_before,
            coverage: TimelineCoverage {
                earliest_recorded_id: earliest,
                includes_pre_feature_history: false,
            },
        })
    }

    pub fn timeline_entry(&self, id: u64) -> Result<TimelineEntry> {
        if id == 0 {
            return Err(Error::Invalid("timeline entry ID must be positive".into()));
        }
        let connection = self.connection()?;
        if workspace_schema_version(&connection)? == 4 {
            return Err(Error::NotFound(format!("workspace timeline entry {id}")));
        }
        read_entry(&connection, id)
    }

    pub fn fork_from_entry(
        &self,
        engine: &Engine,
        id: u64,
        destination: impl AsRef<Path>,
        change: ChangeId,
    ) -> Result<Workspace> {
        change.validate()?;
        let state = self.state()?;
        if change.repository != state.change.repository {
            return Err(Error::Conflict(
                "workspace fork Change belongs to another repository".into(),
            ));
        }
        if change == state.change {
            return Err(Error::Conflict(
                "workspace fork requires a different Change".into(),
            ));
        }
        let entry = self.timeline_entry(id)?;
        engine.load_revision(entry.revision())?;
        let destination = absolute_path(destination.as_ref())?;
        let destination = require_new_nonnested_destination(&self.root, &destination)?;
        match entry.scope() {
            Some(scope) => {
                Workspace::create_scoped(engine, &destination, change, entry.revision(), scope)
            }
            None => Workspace::create(engine, &destination, change, entry.revision()),
        }
    }

    fn try_timeline_lease(&self) -> Result<Option<Lease>> {
        let path = self.root.join(METADATA_DIRECTORY).join(TIMELINE_LEASE_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(Error::Invalid(
                "workspace timeline lease is not a non-symlink regular file".into(),
            ));
        }
        match Lease::try_exclusive(file) {
            Ok(lease) => Ok(Some(lease)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(Error::Io(error)),
        }
    }

    fn capture_fence(&self) -> Result<Option<CaptureFence>> {
        let connection = self.connection()?;
        if pending_operation(&connection)?.is_some() {
            return Ok(None);
        }
        Ok(Some(CaptureFence {
            state: read_state(&connection)?,
            scope: read_scope(&connection)?,
        }))
    }

    fn fence_change(&self, fence: &CaptureFence) -> Result<Option<CaptureSkip>> {
        fence_change(&self.connection()?, fence)
    }

    fn check_link_fence(
        &self,
        connection: &Connection,
        request: &CheckLinkRequest,
    ) -> Result<std::result::Result<CaptureFence, CaptureSkip>> {
        if pending_operation(connection)?.is_some() {
            return Ok(Err(CaptureSkip::OperationPending));
        }
        let state = read_state(connection)?;
        if !state.ready {
            return Ok(Err(CaptureSkip::WorkspaceIncomplete));
        }
        if state.change != request.change {
            return Ok(Err(CaptureSkip::ChangeMismatch));
        }
        if state.base_revision != request.source_revision {
            return Ok(Err(CaptureSkip::WorkspaceChanged));
        }
        Ok(Ok(CaptureFence {
            state,
            scope: read_scope(connection)?,
        }))
    }
}

fn validate_check_request(engine: &Engine, request: &CheckLinkRequest) -> Result<()> {
    request.change.validate()?;
    for (label, value) in [
        ("source revision", request.source_revision.as_str()),
        ("tested revision", request.tested_revision.as_str()),
        ("context digest", request.context_digest.as_str()),
        ("requirement digest", request.requirement_digest.as_str()),
    ] {
        validate_id(value).map_err(|_| Error::Invalid(format!("invalid {label}")))?;
    }
    crate::skill_workflow_service::validate_run_id(&request.run_id)?;
    if request.change_version == 0 {
        return Err(Error::Invalid("change version must be positive".into()));
    }
    sql_i64("change version", request.change_version)?;
    if request.target.is_empty()
        || request.target.len() > MAX_TARGET_BYTES
        || !request.target.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return Err(Error::Limit(format!(
            "check target must be 1..{MAX_TARGET_BYTES} canonical ASCII bytes"
        )));
    }
    match (&request.target_revision, request.target_version) {
        (Some(revision), Some(version)) if version > 0 => {
            validate_id(revision)?;
            sql_i64("target version", version)?;
        }
        (Some(_), Some(_)) => {
            return Err(Error::Invalid("target version must be positive".into()));
        }
        (None, None) => {}
        _ => {
            return Err(Error::Invalid(
                "target revision and version must be both present or both absent".into(),
            ));
        }
    }
    engine.load_revision(&request.source_revision)?;
    engine.load_revision(&request.tested_revision)?;
    if let Some(target) = &request.target_revision {
        engine.load_revision(target)?;
    }
    Ok(())
}

fn fence_change(connection: &Connection, fence: &CaptureFence) -> Result<Option<CaptureSkip>> {
    if pending_operation(connection)?.is_some() {
        return Ok(Some(CaptureSkip::OperationPending));
    }
    if read_state(connection)? != fence.state || read_scope(connection)? != fence.scope {
        return Ok(Some(CaptureSkip::WorkspaceChanged));
    }
    Ok(None)
}

fn admitted_bytes(staged: &std::collections::BTreeMap<String, super::Stage>) -> Result<u64> {
    staged.values().try_fold(0u64, |total, stage| {
        let bytes = stage.after.as_ref().map_or(0, |item| item.content.len());
        total
            .checked_add(
                u64::try_from(bytes).map_err(|_| {
                    Error::Limit("snapshot changed source bytes do not fit u64".into())
                })?,
            )
            .ok_or_else(|| Error::Limit("snapshot changed source bytes overflow".into()))
    })
}

fn quota_pause(connection: &Connection, additional_bytes: u64) -> Result<Option<CapturePause>> {
    let (entries, admitted): (i64, i64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(admitted_bytes), 0) FROM timeline_entries",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let entries = sql_u64("timeline entry count", entries)?;
    let admitted = sql_u64("timeline admitted bytes", admitted)?;
    if entries >= MAX_TIMELINE_ENTRIES {
        return Ok(Some(CapturePause::EntryLimit));
    }
    if admitted
        .checked_add(additional_bytes)
        .is_none_or(|total| total > MAX_ADMITTED_BYTES)
    {
        return Ok(Some(CapturePause::ByteLimit));
    }
    Ok(None)
}

fn snapshot_matches(
    engine: &Engine,
    revision: &str,
    scope: Option<&Scope>,
    working: &Snapshot,
) -> Result<bool> {
    Ok(
        source_snapshot_scoped(engine, revision, scope)?.is_some_and(|snapshot| {
            snapshot.files == working.files && snapshot.directories == working.directories
        }),
    )
}

fn last_snapshot(connection: &Connection) -> Result<Option<(String, String, Option<Scope>)>> {
    if workspace_schema_version(connection)? == 4 {
        return Ok(None);
    }
    let row: Option<(String, String, Vec<u8>)> = connection
        .query_row(
            "SELECT source_revision,base_revision,scope_json FROM timeline_entries
             WHERE kind = 'snapshot' ORDER BY sequence DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(revision, base_revision, scope)| {
        validate_id(&base_revision)?;
        Ok((revision, base_revision, decode_optional_scope(&scope)?))
    })
    .transpose()
}

fn workspace_schema_version(connection: &Connection) -> Result<i64> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(Error::from)
}

fn matching_check_link(
    connection: &Connection,
    request: &CheckLinkRequest,
) -> Result<Option<CheckLinkEntry>> {
    let id: Option<i64> = connection
        .query_row(
            "SELECT sequence FROM timeline_entries
             WHERE kind='check_link' AND run_id=?1 AND context_digest=?2
                AND requirement_digest=?3",
            params![
                request.run_id,
                request.context_digest,
                request.requirement_digest
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else {
        return Ok(None);
    };
    let entry = match read_entry(connection, sql_u64("timeline entry ID", id)?)? {
        TimelineEntry::CheckLink(entry) => entry,
        TimelineEntry::Snapshot(_) => {
            return Err(Error::Invalid(
                "check link identity resolved to a snapshot".into(),
            ));
        }
    };
    if check_entry_matches(&entry, request) {
        Ok(Some(entry))
    } else {
        Err(Error::Conflict(
            "workflow run identity was reused with different check inputs".into(),
        ))
    }
}

fn check_entry_matches(entry: &CheckLinkEntry, request: &CheckLinkRequest) -> bool {
    entry.source_revision == request.source_revision
        && entry.tested_revision == request.tested_revision
        && entry.change == request.change
        && entry.change_version == request.change_version
        && entry.context_digest == request.context_digest
        && entry.policy_epoch == request.policy_epoch
        && entry.target == request.target
        && entry.target_revision == request.target_revision
        && entry.target_version == request.target_version
        && entry.requirement_digest == request.requirement_digest
        && entry.run_id == request.run_id
}

#[allow(clippy::type_complexity)]
fn read_entry(connection: &Connection, id: u64) -> Result<TimelineEntry> {
    let row = connection
        .query_row(
            "SELECT kind,trigger,recorded_at,source_revision,base_revision,generation,scope_json,
                changed_paths,admitted_bytes,tested_revision,change_repository,change_id,
                change_version,context_digest,policy_epoch,target,target_revision,target_version,
                requirement_digest,run_id
             FROM timeline_entries WHERE sequence=?1",
            [sql_i64("timeline entry ID", id)?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<i64>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, Option<i64>>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, Option<i64>>(17)?,
                    row.get::<_, Option<String>>(18)?,
                    row.get::<_, Option<String>>(19)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("workspace timeline entry {id}")))?;
    let (
        kind,
        trigger,
        recorded_at,
        source_revision,
        base_revision,
        generation,
        scope_json,
        changed_paths,
        admitted_bytes,
        tested_revision,
        change_repository,
        change_id,
        change_version,
        context_digest,
        policy_epoch,
        target,
        target_revision,
        target_version,
        requirement_digest,
        run_id,
    ) = row;
    validate_id(&source_revision)?;
    validate_id(&base_revision)?;
    let scope = decode_optional_scope(&scope_json)?.map(|scope| scope.roots().to_vec());
    let recorded_at = sql_u64("timeline timestamp", recorded_at)?;
    let generation = sql_u64("workspace generation", generation)?;
    match kind.as_str() {
        "snapshot" => {
            let changed_paths = changed_paths.ok_or_else(|| {
                Error::Invalid("snapshot timeline entry has no changed path count".into())
            })?;
            let admitted_bytes = admitted_bytes.ok_or_else(|| {
                Error::Invalid("snapshot timeline entry has no admitted byte count".into())
            })?;
            Ok(TimelineEntry::Snapshot(SnapshotEntry {
                id,
                trigger: parse_trigger(&trigger)?,
                recorded_at,
                revision: source_revision,
                base_revision,
                generation,
                scope,
                changed_paths: sql_u64("snapshot changed paths", changed_paths)?,
                admitted_bytes: sql_u64("snapshot admitted bytes", admitted_bytes)?,
            }))
        }
        "check_link" => {
            if trigger != "check_start" {
                return Err(Error::Invalid(
                    "check link timeline entry has an invalid trigger".into(),
                ));
            }
            let tested_revision = tested_revision
                .ok_or_else(|| Error::Invalid("check link has no tested revision".into()))?;
            let change = ChangeId {
                repository: change_repository
                    .ok_or_else(|| Error::Invalid("check link has no repository".into()))?,
                id: change_id
                    .ok_or_else(|| Error::Invalid("check link has no Change ID".into()))?,
            };
            change.validate()?;
            validate_id(&tested_revision)?;
            let context_digest = context_digest
                .ok_or_else(|| Error::Invalid("check link has no context digest".into()))?;
            let requirement_digest = requirement_digest
                .ok_or_else(|| Error::Invalid("check link has no requirement digest".into()))?;
            let run_id = run_id.ok_or_else(|| Error::Invalid("check link has no run ID".into()))?;
            validate_id(&context_digest)?;
            validate_id(&requirement_digest)?;
            crate::skill_workflow_service::validate_run_id(&run_id)?;
            if let Some(revision) = &target_revision {
                validate_id(revision)?;
            }
            let change_version = sql_u64(
                "change version",
                change_version
                    .ok_or_else(|| Error::Invalid("check link has no change version".into()))?,
            )?;
            if change_version == 0 {
                return Err(Error::Invalid(
                    "check link change version must be positive".into(),
                ));
            }
            let target = target.ok_or_else(|| Error::Invalid("check link has no target".into()))?;
            if target.is_empty()
                || target.len() > MAX_TARGET_BYTES
                || !target.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
                })
            {
                return Err(Error::Invalid("check link has an invalid target".into()));
            }
            let target_version = target_version
                .map(|value| sql_u64("target version", value))
                .transpose()?;
            if target_version == Some(0) {
                return Err(Error::Invalid(
                    "check link target version must be positive".into(),
                ));
            }
            if target_revision.is_some() != target_version.is_some() {
                return Err(Error::Invalid(
                    "check link target revision and version are incomplete".into(),
                ));
            }
            Ok(TimelineEntry::CheckLink(CheckLinkEntry {
                id,
                recorded_at,
                source_revision,
                tested_revision,
                base_revision,
                generation,
                scope,
                change,
                change_version,
                context_digest,
                policy_epoch: sql_u64(
                    "policy epoch",
                    policy_epoch
                        .ok_or_else(|| Error::Invalid("check link has no policy epoch".into()))?,
                )?,
                target,
                target_revision,
                target_version,
                requirement_digest,
                run_id,
                evidence_coverage: CheckEvidenceCoverage::ReferenceOnly,
            }))
        }
        _ => Err(Error::Invalid(format!(
            "invalid workspace timeline entry kind {kind}"
        ))),
    }
}

fn trigger_name(trigger: SnapshotTrigger) -> &'static str {
    match trigger {
        SnapshotTrigger::Manual => "manual",
        SnapshotTrigger::TurnEnd => "turn_end",
        SnapshotTrigger::CheckStart => "check_start",
        SnapshotTrigger::McpExplicit => "mcp_explicit",
    }
}

fn parse_trigger(value: &str) -> Result<SnapshotTrigger> {
    match value {
        "manual" => Ok(SnapshotTrigger::Manual),
        "turn_end" => Ok(SnapshotTrigger::TurnEnd),
        "check_start" => Ok(SnapshotTrigger::CheckStart),
        "mcp_explicit" => Ok(SnapshotTrigger::McpExplicit),
        _ => Err(Error::Invalid(format!(
            "invalid workspace snapshot trigger {value}"
        ))),
    }
}

fn deadline_expired(started: Instant, deadline: Option<Duration>) -> bool {
    deadline.is_some_and(|deadline| started.elapsed() >= deadline)
}

fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| Error::Invalid("system clock is before the Unix epoch".into()))
}

fn require_new_nonnested_destination(
    source: &Path,
    destination: &Path,
) -> Result<std::path::PathBuf> {
    if destination.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(Error::Invalid(
            "workspace fork destination must be an absolute normalized path".into(),
        ));
    }
    let name = destination.file_name().ok_or_else(|| {
        Error::Invalid("workspace fork destination needs a directory name".into())
    })?;
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Invalid("workspace fork destination has no parent".into()))?;
    let canonical_parent = fs::canonicalize(parent)?;
    let destination = canonical_parent.join(name);
    let source = fs::canonicalize(source)?;
    if destination.starts_with(&source) || source.starts_with(&destination) {
        return Err(Error::Conflict(
            "workspace fork destination must not contain or be contained by the source".into(),
        ));
    }
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            return Err(Error::Conflict(format!(
                "workspace destination already exists: {}",
                destination.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Io(error)),
    }
    let mut ancestor = destination.parent();
    while let Some(path) = ancestor {
        if path.join(METADATA_DIRECTORY).try_exists()? {
            return Err(Error::Conflict(
                "workspace fork destination is nested in another workspace".into(),
            ));
        }
        ancestor = path.parent();
    }
    Ok(destination)
}

pub(crate) fn validate_recovery_schema(connection: &Connection) -> Result<()> {
    connection.prepare(
        "SELECT sequence,kind,trigger,recorded_at,source_revision,base_revision,generation,
            scope_json,changed_paths,admitted_bytes,tested_revision,change_repository,change_id,
            change_version,context_digest,policy_epoch,target,target_revision,target_version,
            requirement_digest,run_id
         FROM timeline_entries LIMIT 0",
    )?;
    let (entries, admitted): (i64, i64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(admitted_bytes), 0) FROM timeline_entries",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if sql_u64("timeline entry count", entries)? > MAX_TIMELINE_ENTRIES
        || sql_u64("timeline admitted bytes", admitted)? > MAX_ADMITTED_BYTES
    {
        return Err(Error::Invalid(
            "workspace timeline exceeds its retained limits".into(),
        ));
    }
    let mut statement = connection.prepare("SELECT sequence FROM timeline_entries")?;
    let ids = statement
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for id in ids {
        read_entry(connection, sql_u64("timeline entry ID", id)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileChange;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn put(path: &str, content: &[u8]) -> FileChange {
        FileChange {
            path: path.into(),
            content: Some(content.to_vec()),
        }
    }

    #[test]
    fn final_snapshot_insert_rejects_a_changed_generation_and_base() {
        let fixtures = TempDir::new().unwrap();
        let engine = Engine::open(fixtures.path().join("store")).unwrap();
        let base = engine
            .checkpoint(
                None,
                &[put("selected.txt", b"base"), put("outside.txt", b"old")],
            )
            .unwrap();
        let root = fixtures.path().join("workspace");
        let workspace = Workspace::create(
            &engine,
            &root,
            ChangeId {
                repository: "repository".into(),
                id: "change-one".into(),
            },
            &base,
        )
        .unwrap();
        let fence = workspace.capture_fence().unwrap().unwrap();
        let captured_revision = engine
            .checkpoint(Some(&base), &[put("selected.txt", b"edit")])
            .unwrap();
        let next_base = engine
            .checkpoint(Some(&base), &[put("outside.txt", b"new")])
            .unwrap();

        let database = root
            .join(METADATA_DIRECTORY)
            .join(super::super::DATABASE_FILE);
        let connection = Connection::open(database).unwrap();
        connection
            .execute(
                "UPDATE workspace_state SET base_revision=?1,generation=1 WHERE singleton=1",
                [&next_base],
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            workspace
                .insert_snapshot_entry(
                    SnapshotTrigger::Manual,
                    Instant::now(),
                    None,
                    &fence,
                    &captured_revision,
                    1,
                    4,
                )
                .unwrap(),
            CaptureResult::Skipped {
                reason: CaptureSkip::WorkspaceChanged
            }
        ));
        assert!(workspace.timeline(10, None).unwrap().entries.is_empty());
    }

    #[test]
    fn check_link_replay_succeeds_while_timeline_lease_is_busy() {
        let fixtures = TempDir::new().unwrap();
        let engine = Engine::open(fixtures.path().join("store")).unwrap();
        let base = engine
            .checkpoint(None, &[put("selected.txt", b"base")])
            .unwrap();
        let tested = engine
            .checkpoint(Some(&base), &[put("selected.txt", b"tested")])
            .unwrap();
        let root = fixtures.path().join("workspace");
        let workspace = Workspace::create(
            &engine,
            &root,
            ChangeId {
                repository: "repository".into(),
                id: "change-one".into(),
            },
            &base,
        )
        .unwrap();
        let request = CheckLinkRequest {
            source_revision: base.clone(),
            tested_revision: tested,
            change: ChangeId {
                repository: "repository".into(),
                id: "change-one".into(),
            },
            change_version: 1,
            context_digest: "b".repeat(64),
            policy_epoch: 1,
            target: "main".into(),
            target_revision: None,
            target_version: None,
            requirement_digest: "c".repeat(64),
            run_id: format!("loop-{}", "d".repeat(59)),
        };
        assert!(matches!(
            workspace.record_check_start(&engine, &request).unwrap(),
            CheckLinkResult::Recorded { .. }
        ));

        let _lease = workspace.try_timeline_lease().unwrap().unwrap();
        assert!(matches!(
            workspace.record_check_start(&engine, &request).unwrap(),
            CheckLinkResult::AlreadyRecorded { .. }
        ));

        let mut new_request = request;
        new_request.run_id = format!("loop-{}", "e".repeat(59));
        assert!(matches!(
            workspace.record_check_start(&engine, &new_request).unwrap(),
            CheckLinkResult::Skipped {
                reason: CaptureSkip::CaptureBusy
            }
        ));
    }

    #[test]
    fn capture_probe_directory_is_isolated_from_parent_sweeps_and_rejects_symlinks() {
        let fixtures = TempDir::new().unwrap();
        let root = fixtures.path().join("workspace");
        let metadata = root.join(METADATA_DIRECTORY);
        fs::create_dir(&root).unwrap();
        fs::create_dir(&metadata).unwrap();

        let probes = CaptureProbeDirectory::create(&root).unwrap();
        let held_probe = probes.path().join(".agentscm-probe-held");
        fs::write(&held_probe, b"held").unwrap();
        super::super::sweep_stale_probes(&metadata).unwrap();
        assert_eq!(fs::read(&held_probe).unwrap(), b"held");
        fs::remove_file(held_probe).unwrap();
        let probe_path = probes.path().to_path_buf();
        drop(probes);
        assert!(!probe_path.exists());

        let target = fixtures.path().join("foreign");
        fs::create_dir(&target).unwrap();
        symlink(&target, &probe_path).unwrap();
        assert!(CaptureProbeDirectory::create(&root).is_err());
        assert!(target.is_dir());
    }
}
