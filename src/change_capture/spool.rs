//! Bounded, private and crash-safe local capture spool.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::change_capture::{CaptureError, Result, sha256};
use crate::change_record::{
    ActionFinishV1, ActionKindV1, ActionStartV1, ArtifactMediaV1, ArtifactRefV1, CaptureBatchV1,
    CaptureEnvelopeV1, CaptureModeV1, CapturedEventKindV1, ContentAvailabilityV1, CoverageV1,
    RetentionClassV1, SourceBindingV1, ToolKindV1,
};

const MAX_PENDING_PAGE: usize = 100;
const MAX_DELIVERY_PAGE: usize = 10;
const MAX_SEQUENCE_EVENTS: usize = 16_384;
const MAX_SEQUENCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PHYSICAL_RECORDS: usize = 65_536;
const ACTION_FINISH_RESERVE_BYTES: u64 = 16 * 1024;
pub const MIN_ACTION_CAPTURE_RECORDS: usize = 48;
pub const MIN_ACTION_CAPTURE_BYTES: u64 = 64 * 1024;
const BASE_METADATA_RESERVE_BYTES: u64 = 2 * 1024 * 1024;
const BASE_METADATA_RESERVE_RECORDS: usize = 256;
const MAX_DEFERRED_BYTES: u64 = 64 * 1024;
const MAX_COMPACTION_PAGE: usize = 100;
const MAX_EXPORT_ANCHOR_ATTEMPTS: usize = 8;
const SPOOL_DIRECTORIES: [&str; 11] = [
    "artifacts",
    "outbox",
    "acknowledged",
    "sessions",
    "associations",
    "deferred",
    "observations",
    "managed-invocations",
    "recipe-attempts",
    "recipe-fences",
    "quarantine",
];
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
static INIT_BEFORE_MUTATION_BARRIER: std::sync::Mutex<
    Option<(PathBuf, std::sync::Arc<std::sync::Barrier>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static EXPORT_BEFORE_INSTALL_BARRIER: std::sync::Mutex<
    Option<(PathBuf, std::sync::Arc<std::sync::Barrier>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static EXPORT_AFTER_INSTALL_BARRIER: std::sync::Mutex<
    Option<(PathBuf, std::sync::Arc<std::sync::Barrier>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static EXPORT_AFTER_ANCHOR_CREATE_BARRIER: std::sync::Mutex<
    Option<(PathBuf, std::sync::Arc<std::sync::Barrier>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static EXPORT_AFTER_CHILD_OPEN_BARRIER: std::sync::Mutex<
    Option<(PathBuf, std::sync::Arc<std::sync::Barrier>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static EXPORT_BEFORE_DESTINATION_REOPEN_BARRIER: std::sync::Mutex<
    Option<(PathBuf, std::sync::Arc<std::sync::Barrier>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static FAIL_EXPORT_SYNC_FOR: std::sync::Mutex<Option<(PathBuf, &'static str)>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static FAIL_EXPORT_READDIR_FOR: std::sync::Mutex<Option<(FileIdentityV1, usize)>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static EXPORT_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
thread_local! {
    static FAIL_DIRECTORY_SYNC_FOR: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
    static FAIL_OCCURRENCE_UNLINK_AFTER_RECEIPT: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    static FAIL_EXPORT_ACL_CLEAR: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpoolLimits {
    pub max_records: usize,
    pub max_total_bytes: u64,
    pub max_artifact_bytes: u64,
}

impl Default for SpoolLimits {
    fn default() -> Self {
        Self {
            max_records: 4_096,
            max_total_bytes: 64 * 1024 * 1024,
            max_artifact_bytes: 16 * 1024 * 1024,
        }
    }
}

impl SpoolLimits {
    /// Validates the structural bounds accepted by the private spool implementation.
    pub fn validate(self) -> Result<()> {
        if self.max_records == 0
            || self.max_records > 65_536
            || self.max_total_bytes < 4_096
            || self.max_total_bytes > 1024 * 1024 * 1024
            || self.max_artifact_bytes == 0
            || self.max_artifact_bytes > self.max_total_bytes
        {
            return Err(CaptureError::Invalid(
                "capture spool limits are invalid".into(),
            ));
        }
        Ok(())
    }

    /// Rejects configured limits that cannot retain one action and its reserved terminal receipt.
    /// Smaller limits remain useful only for internal quota fault tests.
    pub fn validate_action_capture(self) -> Result<()> {
        self.validate()?;
        if self.max_records < MIN_ACTION_CAPTURE_RECORDS
            || self.max_total_bytes < MIN_ACTION_CAPTURE_BYTES
        {
            return Err(CaptureError::Invalid(format!(
                "configured capture spool requires at least {MIN_ACTION_CAPTURE_RECORDS} records and {MIN_ACTION_CAPTURE_BYTES} bytes"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    Stored,
    Duplicate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamespaceBindOutcome {
    Stored,
    Duplicate,
    Conflict,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactionReport {
    pub envelopes: usize,
    pub bytes_reclaimed: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuarantineEntryV1 {
    pub name: String,
    pub size_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuarantinePruneReport {
    pub records: usize,
    pub bytes_reclaimed: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpoolClassificationV1 {
    V4IdentityMatch,
    V4IdentityMismatch,
    V3,
    V2,
    PristinePartialV4,
    EvidencePartial,
    UnknownNonempty,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpoolProbeStateV1 {
    Busy,
    ClearDirectoryLocal,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpoolDigestV1 {
    Available(String),
    Unreadable,
    OversizedUnhashed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpoolEntryInspectionV1 {
    pub relative_path: Option<String>,
    pub relative_path_hex: String,
    pub size_bytes: Option<u64>,
    pub digest: SpoolDigestV1,
    pub regular_file: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpoolAttemptInspectionV1 {
    pub fence_id: String,
    pub action_id: String,
    pub work_id: String,
    pub generation: i64,
    pub recipe_id: String,
    pub root_device: u64,
    pub root_inode: u64,
    pub host_id: Option<String>,
    pub boot_id: Option<String>,
    pub phase: String,
    pub pid: Option<u32>,
    pub pgid: Option<u32>,
    pub start_present: Option<bool>,
    pub finish_present: Option<bool>,
    pub resolution: String,
}

const MAX_INSPECTION_RETURNED: usize = 100;
const MAX_INSPECTION_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INSPECTION_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_INSPECTION_DEPTH: usize = 32;
const MAX_READ_ONLY_ENVELOPES: usize = 8_192;
const MAX_READ_ONLY_EVENTS: usize = 100_000;
const MAX_READ_ONLY_ENVELOPE_BYTES: u64 = 1024 * 1024;
const MAX_READ_ONLY_COMPACTION_BYTES: u64 = 32 * 1024 * 1024;
const MAX_READ_ONLY_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const READ_ONLY_SCAN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundedCountV1 {
    pub present: usize,
    pub returned: usize,
    pub omitted: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SpoolInspectionV1 {
    pub version: u8,
    pub mode: String,
    pub classification: SpoolClassificationV1,
    pub observed_root: FileIdentityV1,
    pub committed_identity: Option<SpoolIdentityV1>,
    pub namespace: Option<SpoolNamespaceV1>,
    pub physical_records: usize,
    pub physical_bytes: u64,
    pub entries: Vec<SpoolEntryInspectionV1>,
    pub entry_count: BoundedCountV1,
    pub pending: Vec<CaptureEnvelopeV1>,
    pub pending_count: BoundedCountV1,
    pub deferred: Vec<DeferredHookV1>,
    pub deferred_count: BoundedCountV1,
    pub quarantine: Vec<QuarantineEntryV1>,
    pub quarantine_count: BoundedCountV1,
    pub attempts: Vec<SpoolAttemptInspectionV1>,
    pub attempt_count: BoundedCountV1,
    pub acknowledged_count: BoundedCountV1,
    pub live_claim_count: BoundedCountV1,
    pub temp_count: BoundedCountV1,
    pub probe: SpoolProbeStateV1,
    pub cannot_observe_original_holders: bool,
    pub resumable: bool,
    pub authority: String,
    pub completeness: String,
    pub issues: Vec<String>,
    pub issues_omitted: usize,
    pub recovery: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SpoolExportManifestV1 {
    pub version: u8,
    pub source_path: String,
    pub observed_root: FileIdentityV1,
    pub classification: SpoolClassificationV1,
    pub entries: Vec<SpoolEntryInspectionV1>,
    pub entry_count: BoundedCountV1,
    pub attempts: Vec<SpoolAttemptInspectionV1>,
    pub attempt_count: BoundedCountV1,
    pub probe_start: SpoolProbeStateV1,
    pub probe_end: SpoolProbeStateV1,
    pub source_in_use: bool,
    pub resumable: bool,
    pub authority: String,
    pub completeness: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CompactedEnvelopeV1 {
    digest: String,
    /// Action receipts, native occurrence events, and provider-history gaps stay available for
    /// replay-safe state transitions. Ordinary event batches need only their exact digest after
    /// Hub acknowledgement.
    envelope: Option<CaptureEnvelopeV1>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileIdentityV1 {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpoolIdentityV1 {
    pub version: u8,
    pub root: FileIdentityV1,
    pub spool_lock: FileIdentityV1,
    pub managed_lock: FileIdentityV1,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CompactionStateV1 {
    version: u8,
    entries: BTreeMap<String, CompactedEnvelopeV1>,
    /// Operation-id hashes mapped to their exact private observation fingerprints. Completed
    /// native action pairs can reclaim the one-file-per-observation form without weakening replay
    /// conflict detection.
    #[serde(default)]
    observations: BTreeMap<String, String>,
    /// Bounded original routing facts for native provider occurrences whose per-occurrence claim
    /// file was consumed after a durable terminal receipt.
    #[serde(default)]
    native_occurrences: BTreeMap<String, NativeOccurrenceReceiptV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpoolNamespaceV1 {
    pub version: u8,
    pub hub: String,
    pub repository: String,
    pub worktree_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeferredReasonV1 {
    TargetLookupUnavailable,
    NoCurrentAuthorizedWork,
}

/// A sanitized local record of a hook that could not be associated with an exact authorized
/// target when it arrived. Deferred records are never rewritten into Hub envelopes.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeferredHookV1 {
    pub occurrence_id: String,
    /// Durable order reserved in the observed native session even when no Hub target was reachable.
    #[serde(default)]
    pub reserved_sequence: Option<u64>,
    pub session_id: String,
    pub hook_event_name: String,
    pub observed_at: i64,
    pub reason: DeferredReasonV1,
    pub input_digest: String,
    pub event_kind: Option<CapturedEventKindV1>,
    pub tool_kind: Option<ToolKindV1>,
    pub source: Option<SourceBindingV1>,
    pub recipe_digest: Option<String>,
    pub action_kind: Option<ActionKindV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct DeferredRecordV1 {
    receipt_order: u64,
    hook: DeferredHookV1,
    #[serde(default)]
    target: Option<SessionAssociationV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionAssociationV1 {
    pub observed_session_id: String,
    pub canonical_session_id: String,
    pub work_id: String,
    pub generation: i64,
    pub parent_session_id: Option<String>,
    pub capture_mode: CaptureModeV1,
    /// The mode this session was first seen with. Absent in older records, which count as
    /// originally in `capture_mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_capture_mode: Option<CaptureModeV1>,
}

#[derive(Clone, Debug)]
pub(crate) struct DeferredGapV1 {
    pub occurrence_id: String,
    pub reserved_sequence: u64,
    pub observed_at: i64,
    pub target: SessionAssociationV1,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeliveryCursorV1 {
    version: u8,
    envelope_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ObservationScopeV1 {
    version: u8,
    id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedInvocationV1 {
    pub version: u8,
    pub marker_id: String,
    #[serde(default)]
    pub operation_prefix: Option<String>,
    pub observed_session_id: String,
    pub canonical_session_id: String,
    pub work_id: String,
    pub generation: i64,
    pub tool_use_id: String,
    pub command_digest: String,
    pub repository: String,
    pub worktree_id: String,
    #[serde(default)]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub root_path: Option<String>,
    pub root_device: u64,
    pub root_inode: u64,
    #[serde(default)]
    pub source: Option<SourceBindingV1>,
    #[serde(default)]
    pub setup_digest: Option<String>,
    #[serde(default)]
    pub recipe_digest: Option<String>,
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub capture_mode: Option<CaptureModeV1>,
    pub observed_at: i64,
    #[serde(default)]
    pub pre_fingerprint: Option<String>,
    #[serde(default)]
    pub post_fingerprint: Option<String>,
    #[serde(default)]
    pub route: Option<NativeOccurrenceRouteV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "route", deny_unknown_fields)]
pub(crate) enum NativeOccurrenceRouteV1 {
    Managed,
    OrdinaryEvent,
    OrdinaryAction {
        kind: ActionKindV1,
        recipe_provenance: crate::change_record::RecipeProvenanceV1,
        coverage: CoverageV1,
        result_source: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeOccurrenceReceiptV1 {
    pub version: u8,
    pub marker_id: String,
    pub observed_session_id: String,
    pub canonical_session_id: String,
    pub work_id: String,
    pub generation: i64,
    pub tool_use_id: String,
    pub route: NativeOccurrenceRouteV1,
    pub pre_fingerprint: Option<String>,
    pub post_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RecipeFenceBindingV1 {
    version: u8,
    fence_id: String,
    repository: String,
    worktree_id: String,
    root_path: String,
    lock_device: u64,
    lock_inode: u64,
    #[serde(default)]
    work_id: Option<String>,
    #[serde(default)]
    root_device: Option<u64>,
    #[serde(default)]
    root_inode: Option<u64>,
    #[serde(default)]
    recipe_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "state", deny_unknown_fields)]
pub(crate) enum RecipeAttemptPhaseV1 {
    Reserved,
    Spawned { pid: u32, pgid: u32 },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecipeAttemptV1 {
    pub version: u8,
    pub fence_id: String,
    pub marker_id: String,
    pub work_id: String,
    pub generation: i64,
    pub observed_session_id: String,
    pub canonical_session_id: String,
    pub repository: String,
    pub worktree_id: String,
    pub root_path: String,
    pub root_device: u64,
    pub root_inode: u64,
    pub base_ref: String,
    pub recipe_id: String,
    pub recipe_digest: String,
    pub setup_digest: String,
    pub coverage: CoverageV1,
    pub host_id: Option<String>,
    pub boot_id: Option<String>,
    pub start_operation_id: String,
    pub finish_operation_id: String,
    pub action_id: String,
    pub phase: RecipeAttemptPhaseV1,
}

#[derive(Clone, Debug)]
pub(crate) struct RecipeAttemptReservationV1 {
    pub fence_id: String,
    pub work_id: String,
    pub generation: i64,
    pub observed_session_id: String,
    pub canonical_session_id: String,
    pub repository: String,
    pub worktree_id: String,
    pub root_path: String,
    pub root_device: u64,
    pub root_inode: u64,
    pub base_ref: String,
    pub recipe_id: String,
    pub recipe_digest: String,
    pub setup_digest: String,
    pub coverage: CoverageV1,
    pub command_digest: String,
    pub host_id: Option<String>,
    pub boot_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LocalSpool {
    root: PathBuf,
    limits: SpoolLimits,
    identity: SpoolIdentityV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadOnlyEnvelopeState {
    Pending,
    Acknowledged,
    Compacted,
}

#[derive(Clone, Debug)]
pub(crate) struct ReadOnlyEnvelope {
    pub envelope: CaptureEnvelopeV1,
    pub state: ReadOnlyEnvelopeState,
}

#[derive(Clone, Debug)]
pub(crate) struct ReadOnlySpoolProjection {
    pub envelopes: Vec<ReadOnlyEnvelope>,
    pub compacted_event_batches_unattributed: usize,
}

/// Descriptor-pinned reader for an existing private spool.
///
/// This reader never initializes, recovers, locks or rewrites spool state. The namespace and
/// fixed v4 identity are checked before retained facts are interpreted. Child directories remain
/// pinned by descriptor so replacing a pathname after startup cannot redirect evidence reads.
pub(crate) struct ReadOnlySpool {
    root_path: PathBuf,
    root: File,
    outbox: File,
    acknowledged: File,
    artifacts: File,
    outbox_identity: FileIdentityV1,
    acknowledged_identity: FileIdentityV1,
    artifacts_identity: FileIdentityV1,
    identity: SpoolIdentityV1,
    namespace: SpoolNamespaceV1,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SequenceState {
    next: u64,
    events: BTreeMap<String, u64>,
    #[serde(default)]
    uncertain_occurrences: u64,
    #[serde(default)]
    start_observed: bool,
    #[serde(default)]
    provider_history: Option<ProviderHistoryCursorV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderHistoryCursorV1 {
    pub path_digest: String,
    pub file_device: u64,
    pub file_inode: u64,
    pub work_id: String,
    pub generation: i64,
    pub canonical_session_id: String,
    pub offset: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ObservedOrderStateV1 {
    version: u8,
    next: u64,
    orders: BTreeMap<String, u64>,
}

struct SpoolLock(File);
struct DirectoryClaim(File);

pub(crate) struct RecipeFenceGuard(File);
pub(crate) struct ManagedCaptureGuard(File);

impl DirectoryClaim {
    fn lock(directory: File) -> Result<Self> {
        // SAFETY: `directory` is an owned directory descriptor and remains live in the guard.
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(directory))
    }
}

impl Drop for DirectoryClaim {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains owned by `self.0` for this call.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl Drop for SpoolLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains owned by `self.0` for this call.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl Drop for RecipeFenceGuard {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains owned by `self.0` for this call.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl Drop for ManagedCaptureGuard {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains owned by `self.0` for this call.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl ReadOnlySpool {
    pub(crate) fn open(root: impl AsRef<Path>, repository: &str) -> Result<Self> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(CaptureError::Invalid(
                "capture spool directory must be absolute".into(),
            ));
        }
        if !bounded_text(repository, 1_024) {
            return Err(CaptureError::Invalid(
                "capture repository namespace is invalid".into(),
            ));
        }

        let root_directory = open_private_directory(root)?;
        validate_private_metadata(&root_directory.metadata()?, "capture spool root")?;
        let root_identity = file_identity(&root_directory)?;
        if path_identity(root)? != root_identity {
            return Err(CaptureError::Conflict(
                "spool_v4_identity_mismatch: capture spool directory changed during read-only open"
                    .into(),
            ));
        }

        let format: SpoolIdentityV1 = serde_json::from_slice(&read_private_bounded_at(
            &root_directory,
            OsStr::new("capture-spool-v4.json"),
            MAX_DEFERRED_BYTES,
        )?)?;
        validate_format_v4(&format)?;
        if format.root != root_identity {
            return Err(CaptureError::Conflict(
                "spool_v4_identity_mismatch: capture spool root identity changed".into(),
            ));
        }
        for (name, expected) in [
            (OsStr::new("spool.lock"), format.spool_lock),
            (OsStr::new("managed-capture.lock"), format.managed_lock),
        ] {
            let lock = openat_private_regular_read_only(&root_directory, name)?;
            if file_identity(&lock)? != expected {
                return Err(CaptureError::Conflict(format!(
                    "spool_v4_identity_mismatch: {} identity changed",
                    name.to_string_lossy()
                )));
            }
        }

        let namespace: SpoolNamespaceV1 = serde_json::from_slice(&read_private_bounded_at(
            &root_directory,
            OsStr::new("namespace-v1.json"),
            MAX_DEFERRED_BYTES,
        )?)?;
        validate_namespace(&namespace)?;
        if namespace.repository != repository {
            return Err(CaptureError::Conflict(
                "capture spool namespace does not match the selected repository".into(),
            ));
        }

        let open_directory = |name: &'static str| -> Result<File> {
            let directory = openat_file(
                &root_directory,
                OsStr::new(name),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )?;
            validate_private_metadata(
                &directory.metadata()?,
                &format!("capture directory {name}"),
            )?;
            Ok(directory)
        };
        let outbox = open_directory("outbox")?;
        let acknowledged = open_directory("acknowledged")?;
        let artifacts = open_directory("artifacts")?;
        let outbox_identity = file_identity(&outbox)?;
        let acknowledged_identity = file_identity(&acknowledged)?;
        let artifacts_identity = file_identity(&artifacts)?;

        // A valid v4 spool always has this state. Reading it here rejects partial initialization
        // without running the writer's recovery path.
        let compaction: CompactionStateV1 = serde_json::from_slice(&read_private_bounded_at(
            &root_directory,
            OsStr::new("compaction-v1.json"),
            MAX_READ_ONLY_COMPACTION_BYTES,
        )?)?;
        if compaction.entries.len() > MAX_READ_ONLY_ENVELOPES
            || !valid_compaction_state(&compaction)
        {
            return Err(CaptureError::Invalid(
                "capture compaction index is invalid".into(),
            ));
        }

        Ok(Self {
            root_path: root.to_owned(),
            root: root_directory,
            outbox,
            acknowledged,
            artifacts,
            outbox_identity,
            acknowledged_identity,
            artifacts_identity,
            identity: format,
            namespace,
        })
    }

    fn verify_pinned_identity(&self) -> Result<()> {
        if file_identity(&self.root)? != self.identity.root
            || path_identity(&self.root_path)? != self.identity.root
        {
            return Err(CaptureError::Conflict(
                "spool_v4_identity_mismatch: pinned capture spool identity changed".into(),
            ));
        }
        for (name, pinned, expected) in [
            ("outbox", &self.outbox, self.outbox_identity),
            (
                "acknowledged",
                &self.acknowledged,
                self.acknowledged_identity,
            ),
            ("artifacts", &self.artifacts, self.artifacts_identity),
        ] {
            if file_identity(pinned)? != expected {
                return Err(CaptureError::Conflict(
                    "spool_v4_identity_mismatch: pinned capture directory changed".into(),
                ));
            }
            let current = openat_file(
                &self.root,
                OsStr::new(name),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )?;
            validate_private_metadata(&current.metadata()?, &format!("capture directory {name}"))?;
            if file_identity(&current)? != expected {
                return Err(CaptureError::Conflict(format!(
                    "spool_v4_identity_mismatch: capture directory {name} was replaced"
                )));
            }
        }
        let namespace: SpoolNamespaceV1 = serde_json::from_slice(&read_private_bounded_at(
            &self.root,
            OsStr::new("namespace-v1.json"),
            MAX_DEFERRED_BYTES,
        )?)?;
        validate_namespace(&namespace)?;
        if namespace != self.namespace {
            return Err(CaptureError::Conflict(
                "capture spool namespace changed after read-only open".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn projection(&self) -> Result<ReadOnlySpoolProjection> {
        self.verify_pinned_identity()?;
        let deadline = Instant::now()
            .checked_add(READ_ONLY_SCAN_TIMEOUT)
            .ok_or_else(|| CaptureError::Limit("evidence scan deadline overflowed".into()))?;
        let compaction_bytes = read_private_bounded_at(
            &self.root,
            OsStr::new("compaction-v1.json"),
            MAX_READ_ONLY_COMPACTION_BYTES,
        )?;
        let mut total_bytes = compaction_bytes.len() as u64;
        let compaction: CompactionStateV1 = serde_json::from_slice(&compaction_bytes)?;
        if compaction.entries.len() > MAX_READ_ONLY_ENVELOPES
            || !valid_compaction_state(&compaction)
        {
            return Err(CaptureError::Limit(
                "capture compaction index exceeds the evidence projection bound".into(),
            ));
        }
        let outbox_names =
            read_only_directory_entry_names(&self.outbox, MAX_READ_ONLY_ENVELOPES, deadline)?;
        let acknowledged_names =
            read_only_directory_entry_names(&self.acknowledged, MAX_READ_ONLY_ENVELOPES, deadline)?;
        let acknowledged_keys: BTreeSet<Vec<u8>> = acknowledged_names
            .iter()
            .map(|name| name.as_bytes().to_vec())
            .collect();
        let mut retained = BTreeMap::<String, ReadOnlyEnvelope>::new();
        for name in &outbox_names {
            check_read_only_deadline(deadline)?;
            let name = name.to_str().ok_or_else(|| {
                CaptureError::Invalid("capture outbox filename is not UTF-8".into())
            })?;
            if owned_temp_target(name)
                .is_some_and(|target| target.strip_suffix(".json").is_some_and(valid_digest))
            {
                continue;
            }
            let key = name.strip_suffix(".json").ok_or_else(|| {
                CaptureError::Invalid("capture outbox filename is invalid".into())
            })?;
            if !valid_digest(key) {
                return Err(CaptureError::Invalid(
                    "capture outbox identity is invalid".into(),
                ));
            }
            let bytes = read_private_bounded_at(
                &self.outbox,
                OsStr::new(name),
                MAX_READ_ONLY_ENVELOPE_BYTES,
            )?;
            total_bytes = total_bytes
                .checked_add(bytes.len() as u64)
                .filter(|total| *total <= MAX_READ_ONLY_TOTAL_BYTES)
                .ok_or_else(|| {
                    CaptureError::Limit(
                        "capture evidence projection exceeds its aggregate byte bound".into(),
                    )
                })?;
            let envelope: CaptureEnvelopeV1 = serde_json::from_slice(&bytes)?;
            envelope
                .validate()
                .map_err(|error| CaptureError::Invalid(error.to_string()))?;
            if envelope_key(&envelope) != key {
                return Err(CaptureError::Conflict(
                    "retained envelope filename does not match its identity".into(),
                ));
            }
            let state = if acknowledged_keys.contains(key.as_bytes()) {
                let marker = read_private_bounded_at(&self.acknowledged, OsStr::new(key), 64)?;
                total_bytes = total_bytes
                    .checked_add(marker.len() as u64)
                    .filter(|total| *total <= MAX_READ_ONLY_TOTAL_BYTES)
                    .ok_or_else(|| {
                        CaptureError::Limit(
                            "capture evidence projection exceeds its aggregate byte bound".into(),
                        )
                    })?;
                if marker != sha256(&bytes).as_bytes() {
                    return Err(CaptureError::Conflict(
                        "acknowledgement differs from the retained envelope".into(),
                    ));
                }
                ReadOnlyEnvelopeState::Acknowledged
            } else {
                ReadOnlyEnvelopeState::Pending
            };
            if retained
                .insert(key.to_owned(), ReadOnlyEnvelope { envelope, state })
                .is_some()
            {
                return Err(CaptureError::Conflict(
                    "capture spool contains duplicate envelope identities".into(),
                ));
            }
        }

        let mut compacted_event_batches_unattributed = 0usize;
        for (key, compacted) in compaction.entries {
            check_read_only_deadline(deadline)?;
            let Some(envelope) = compacted.envelope else {
                compacted_event_batches_unattributed =
                    compacted_event_batches_unattributed.saturating_add(1);
                continue;
            };
            if let Some(existing) = retained.get(&key) {
                if sha256(&serde_json::to_vec(&existing.envelope)?) != compacted.digest {
                    return Err(CaptureError::Conflict(
                        "compacted envelope differs from retained bytes".into(),
                    ));
                }
                continue;
            }
            retained.insert(
                key,
                ReadOnlyEnvelope {
                    envelope,
                    state: ReadOnlyEnvelopeState::Compacted,
                },
            );
            if retained.len() > MAX_READ_ONLY_ENVELOPES {
                return Err(CaptureError::Limit(
                    "capture evidence projection contains too many retained envelopes".into(),
                ));
            }
        }

        let final_outbox_names =
            read_only_directory_entry_names(&self.outbox, MAX_READ_ONLY_ENVELOPES, deadline)?;
        let final_acknowledged_names =
            read_only_directory_entry_names(&self.acknowledged, MAX_READ_ONLY_ENVELOPES, deadline)?;
        let final_compaction_bytes = read_private_bounded_at(
            &self.root,
            OsStr::new("compaction-v1.json"),
            MAX_READ_ONLY_COMPACTION_BYTES,
        )?;
        total_bytes = total_bytes
            .checked_add(final_compaction_bytes.len() as u64)
            .filter(|total| *total <= MAX_READ_ONLY_TOTAL_BYTES)
            .ok_or_else(|| {
                CaptureError::Limit(
                    "capture evidence projection exceeds its aggregate byte bound".into(),
                )
            })?;
        debug_assert!(total_bytes <= MAX_READ_ONLY_TOTAL_BYTES);
        let projected_events = retained.values().try_fold(0usize, |total, retained| {
            let events = match &retained.envelope {
                CaptureEnvelopeV1::EventBatch(batch) => batch.events.len(),
                CaptureEnvelopeV1::ActionStart(_)
                | CaptureEnvelopeV1::ActionFinish(_)
                | CaptureEnvelopeV1::Usage(_) => 0,
            };
            total
                .checked_add(events)
                .filter(|count| *count <= MAX_READ_ONLY_EVENTS)
                .ok_or_else(|| {
                    CaptureError::Limit(
                        "capture evidence projection contains too many retained events".into(),
                    )
                })
        })?;
        debug_assert!(projected_events <= MAX_READ_ONLY_EVENTS);
        check_read_only_deadline(deadline)?;
        self.verify_pinned_identity()?;
        if final_outbox_names != outbox_names
            || final_acknowledged_names != acknowledged_names
            || final_compaction_bytes != compaction_bytes
        {
            return Err(CaptureError::Conflict(
                "capture spool changed during evidence projection; retry the read".into(),
            ));
        }

        let mut envelopes: Vec<_> = retained.into_values().collect();
        envelopes.sort_by(|left, right| {
            delivery_key(&left.envelope).cmp(&delivery_key(&right.envelope))
        });
        Ok(ReadOnlySpoolProjection {
            envelopes,
            compacted_event_batches_unattributed,
        })
    }

    pub(crate) fn read_artifact(
        &self,
        reference: &ArtifactRefV1,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        self.verify_pinned_identity()?;
        if reference.availability != ContentAvailabilityV1::Local {
            return Err(CaptureError::NotFound(
                "artifact is not recorded as locally available".into(),
            ));
        }
        if reference.size_bytes > max_bytes {
            return Err(CaptureError::Limit(
                "artifact reference exceeds the evidence read bound".into(),
            ));
        }
        let bytes = read_private_bounded_at(
            &self.artifacts,
            OsStr::new(&reference.digest),
            reference.size_bytes,
        )?;
        if bytes.len() as u64 != reference.size_bytes || sha256(&bytes) != reference.digest {
            return Err(CaptureError::Conflict(
                "local artifact failed size or digest verification".into(),
            ));
        }
        self.verify_pinned_identity()?;
        Ok(bytes)
    }
}

impl LocalSpool {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_limits(root, SpoolLimits::default())
    }

    pub fn open_with_limits(root: impl AsRef<Path>, limits: SpoolLimits) -> Result<Self> {
        Self::open_with_limits_inner(root.as_ref(), limits, None)
    }

    /// Opens a persisted binding only when its complete v4 inode identity still matches. The
    /// expected comparison happens under the directory claim before recovery or any other write.
    pub fn open_with_limits_expected(
        root: impl AsRef<Path>,
        limits: SpoolLimits,
        expected: SpoolIdentityV1,
    ) -> Result<Self> {
        Self::open_with_limits_inner(root.as_ref(), limits, Some(expected))
    }

    fn open_with_limits_inner(
        root: &Path,
        limits: SpoolLimits,
        expected: Option<SpoolIdentityV1>,
    ) -> Result<Self> {
        if !root.is_absolute() {
            return Err(CaptureError::Invalid(
                "capture spool directory must be absolute".into(),
            ));
        }
        limits.validate()?;
        match fs::symlink_metadata(root) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return Err(CaptureError::Invalid(
                        "capture spool path is not a private directory".into(),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if expected.is_some() {
                    return Err(CaptureError::Conflict(
                        "spool_v4_identity_mismatch: persisted capture spool path is missing"
                            .into(),
                    ));
                }
                if limits.max_records < 6 {
                    return Err(CaptureError::Limit(
                        "capture spool limits cannot admit its fixed v4 metadata".into(),
                    ));
                }
                private_directory(root)?;
            }
            Err(error) => return Err(error.into()),
        }

        let root_directory = open_private_directory(root)?;
        let directory_claim = DirectoryClaim::lock(root_directory)?;
        validate_private_metadata(&directory_claim.0.metadata()?, "capture spool root")?;
        let observed_root = file_identity(&directory_claim.0)?;
        if path_identity(root)? != observed_root {
            return Err(CaptureError::Conflict(
                "spool_v4_identity_mismatch: capture spool directory changed during open".into(),
            ));
        }
        let root_is_empty = directory_entry_names(&directory_claim.0)?.is_empty();

        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| {
            CaptureError::Invalid("capture observation-scope randomness unavailable".into())
        })?;
        let observation_scope = serde_json::to_vec(&ObservationScopeV1 {
            version: 1,
            id: random.iter().map(|byte| format!("{byte:02x}")).collect(),
        })?;
        let compaction = serde_json::to_vec(&CompactionStateV1 {
            version: 1,
            entries: BTreeMap::new(),
            observations: BTreeMap::new(),
            native_occurrences: BTreeMap::new(),
        })?;
        let order = serde_json::to_vec(&ObservedOrderStateV1 {
            version: 1,
            next: 1,
            orders: BTreeMap::new(),
        })?;
        const FORMAT_V4_ADMISSION_BYTES: u64 = 1_024;
        let initial_records = 6usize;
        let initial_bytes = compaction
            .len()
            .saturating_add(order.len())
            .saturating_add(observation_scope.len())
            .saturating_add(FORMAT_V4_ADMISSION_BYTES as usize) as u64;

        let format_path = root.join("capture-spool-v4.json");
        let format = match read_bounded_at(
            &directory_claim.0,
            OsStr::new("capture-spool-v4.json"),
            MAX_DEFERRED_BYTES,
        ) {
            Ok(bytes) => {
                let format: SpoolIdentityV1 = serde_json::from_slice(&bytes)?;
                validate_format_v4(&format)?;
                validate_bound_layout(root, &directory_claim.0, &format)?;
                if expected.is_some_and(|expected| expected != format) {
                    return Err(CaptureError::Conflict(
                        "spool_v4_identity_mismatch: persisted capture spool identity changed"
                            .into(),
                    ));
                }
                format
            }
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                if expected.is_some() {
                    return Err(CaptureError::Conflict(
                        "spool_v4_identity_mismatch: persisted capture spool identity is missing"
                            .into(),
                    ));
                }
                if !root_is_empty {
                    return Err(CaptureError::Conflict(
                        "spool_legacy_format: capture spool has no valid v4 identity commit; inspect and export it before selecting a new empty path"
                            .into(),
                    ));
                }
                if initial_records > limits.max_records || initial_bytes > limits.max_total_bytes {
                    return Err(CaptureError::Limit(
                        "capture spool limits cannot admit its fixed private metadata".into(),
                    ));
                }
                #[cfg(test)]
                if let Some((_, barrier)) = INIT_BEFORE_MUTATION_BARRIER
                    .lock()
                    .unwrap()
                    .clone()
                    .filter(|(target, _)| target == root)
                {
                    barrier.wait();
                    barrier.wait();
                }
                if !directory_entry_names(&directory_claim.0)?.is_empty() {
                    return Err(CaptureError::Conflict(
                        "spool_legacy_format: capture spool changed before v4 initialization"
                            .into(),
                    ));
                }
                for name in SPOOL_DIRECTORIES {
                    private_directory(&root.join(name))?;
                }
                let spool_lock = create_private_lock_file(&root.join("spool.lock"))?;
                let managed_lock = create_private_lock_file(&root.join("managed-capture.lock"))?;
                atomic_write(&root.join("compaction-v1.json"), &compaction)?;
                atomic_write(&root.join("order-v1.json"), &order)?;
                atomic_write(&root.join("observation-scope-v1.json"), &observation_scope)?;
                File::open(root)?.sync_all()?;
                let format = SpoolIdentityV1 {
                    version: 4,
                    root: observed_root,
                    spool_lock: file_identity(&spool_lock)?,
                    managed_lock: file_identity(&managed_lock)?,
                };
                let format_bytes = serde_json::to_vec(&format)?;
                if format_bytes.len() as u64 > FORMAT_V4_ADMISSION_BYTES {
                    return Err(CaptureError::Limit(
                        "capture spool v4 identity commit exceeds its admitted bound".into(),
                    ));
                }
                validate_pristine_v4_layout(&directory_claim.0)?;
                validate_private_authority_layout(&directory_claim.0, true)?;
                // The v4 identity marker is the final durable initialization commit. A crash
                // before this point leaves an inspect-only partial root and is never migrated.
                atomic_write(&format_path, &format_bytes)?;
                File::open(root)?.sync_all()?;
                format
            }
            Err(error) => return Err(error),
        };
        drop(directory_claim);

        let spool = Self {
            root: root.to_owned(),
            limits,
            identity: format,
        };
        {
            let _lock = spool.lock()?;
            recover_spool_temps(&spool.root, spool.limits)?;
            recover_compacted_remainders(&spool.root, spool.limits)?;
        }
        Ok(spool)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn identity(&self) -> SpoolIdentityV1 {
        self.identity
    }

    /// Acquires live caller exclusion for one stable configured-recipe fence. The permanent lock
    /// inode and immutable root binding are never used as evidence that an arbitrary child ended.
    pub(crate) fn try_recipe_fence(
        &self,
        reservation: &RecipeAttemptReservationV1,
    ) -> Result<Option<RecipeFenceGuard>> {
        validate_recipe_attempt_reservation(reservation)?;
        let scope_id = recipe_lock_scope_id(
            &reservation.root_path,
            &reservation.repository,
            &reservation.worktree_id,
        );
        let directory = self.root.join("recipe-fences");
        let binding_path = directory.join(format!("{scope_id}.json"));
        let lock_path = directory.join(format!("{scope_id}.lock"));
        let file = {
            let _global = self.lock()?;
            for name in regular_file_names(&directory, self.limits.max_records)? {
                if !name.ends_with(".json") || name == format!("{scope_id}.json") {
                    continue;
                }
                let bytes = read_bounded(&directory.join(name), MAX_DEFERRED_BYTES)?;
                let retained: RecipeFenceBindingV1 = serde_json::from_slice(&bytes)?;
                validate_recipe_fence_binding(&retained)?;
                if retained.version == 1
                    && retained.root_path == reservation.root_path
                    && retained.repository == reservation.repository
                    && retained.worktree_id == reservation.worktree_id
                {
                    return Err(CaptureError::Conflict(
                        "legacy configured-recipe fence is inspect/export only and cannot run from this checkout"
                            .into(),
                    ));
                }
            }
            if !lock_path.exists() {
                self.reserve(0, 1)?;
            }
            let file = open_private_lock_file(&lock_path)?;
            let metadata = file.metadata()?;
            if !metadata.file_type().is_file() {
                return Err(CaptureError::Conflict(
                    "configured recipe lock is not a regular file".into(),
                ));
            }
            let binding = RecipeFenceBindingV1 {
                version: 2,
                fence_id: scope_id,
                repository: reservation.repository.clone(),
                worktree_id: reservation.worktree_id.clone(),
                root_path: reservation.root_path.clone(),
                lock_device: metadata.dev(),
                lock_inode: metadata.ino(),
                work_id: None,
                root_device: None,
                root_inode: None,
                recipe_id: None,
            };
            validate_recipe_fence_binding(&binding)?;
            match read_bounded(&binding_path, MAX_DEFERRED_BYTES) {
                Ok(bytes) => {
                    let retained: RecipeFenceBindingV1 = serde_json::from_slice(&bytes)?;
                    validate_recipe_fence_binding(&retained)?;
                    if retained != binding {
                        return Err(CaptureError::Conflict(
                            "configured recipe root identity differs from its retained fence"
                                .into(),
                        ));
                    }
                }
                Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    let bytes = serde_json::to_vec(&binding)?;
                    self.reserve(bytes.len() as u64, 1)?;
                    atomic_write(&binding_path, &bytes)?;
                }
                Err(error) => return Err(error),
            }
            file
        };
        // SAFETY: `file` owns a stable private descriptor; nonblocking flock is advisory caller
        // exclusion only and is never interpreted as child liveness or termination evidence.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
        {
            let _global = self.lock()?;
            let current = open_existing_private_file(&lock_path)?;
            if file_identity(&current)? != file_identity(&file)? {
                return Err(CaptureError::Conflict(
                    "configured recipe lock was replaced during acquisition".into(),
                ));
            }
        }
        Ok(Some(RecipeFenceGuard(file)))
    }

    /// Returns the durable random identity of this local observation scope. Orders are comparable
    /// only when this value and the canonical session/generation all match.
    pub fn observation_scope_id(&self) -> Result<String> {
        let _lock = self.lock()?;
        let scope = load_observation_scope(&self.root)?;
        Ok(scope.id)
    }

    pub(crate) fn retain_managed_invocation(
        &self,
        marker: &ManagedInvocationV1,
    ) -> Result<AppendOutcome> {
        validate_managed_invocation(marker)?;
        let path = self
            .root
            .join("managed-invocations")
            .join(format!("{}.json", marker.marker_id));
        let bytes = serde_json::to_vec(marker)?;
        let _lock = self.lock()?;
        match read_bounded(&path, MAX_DEFERRED_BYTES) {
            Ok(existing) => {
                let existing: ManagedInvocationV1 = serde_json::from_slice(&existing)?;
                validate_managed_invocation(&existing)?;
                if existing == *marker {
                    Ok(AppendOutcome::Duplicate)
                } else {
                    Err(CaptureError::Conflict(
                        "managed invocation identity was reused with different facts".into(),
                    ))
                }
            }
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.reserve(bytes.len() as u64, 1)?;
                atomic_write(&path, &bytes)?;
                Ok(AppendOutcome::Stored)
            }
            Err(error) => Err(error),
        }
    }

    /// Verifies the initialization-only v4 identity commit. This never creates or migrates it.
    pub(crate) fn ensure_v4_mutation_identity(&self) -> Result<()> {
        let _lock = self.lock()?;
        Ok(())
    }

    /// Serializes all state transitions for native managed occurrences. This is live caller
    /// exclusion only and is never evidence that an arbitrary child process terminated.
    pub(crate) fn lock_managed_capture(&self) -> Result<ManagedCaptureGuard> {
        let file =
            self.acquire_bound_lock("managed-capture.lock", self.identity.managed_lock, false)?;
        Ok(ManagedCaptureGuard(file))
    }

    pub(crate) fn managed_invocation_for_occurrence(
        &self,
        observed_session_id: &str,
        tool_use_id: &str,
    ) -> Result<Option<ManagedInvocationV1>> {
        if !bounded_identifier(observed_session_id, 255) || !bounded_identifier(tool_use_id, 255) {
            return Err(CaptureError::Invalid(
                "managed invocation occurrence identity is invalid".into(),
            ));
        }
        let _lock = self.lock()?;
        let mut matched = None;
        for name in regular_file_names(
            &self.root.join("managed-invocations"),
            self.limits.max_records,
        )? {
            let bytes = read_bounded(
                &self.root.join("managed-invocations").join(name),
                MAX_DEFERRED_BYTES,
            )?;
            let marker: ManagedInvocationV1 = serde_json::from_slice(&bytes)?;
            validate_managed_invocation(&marker)?;
            if marker.observed_session_id == observed_session_id
                && marker.tool_use_id == tool_use_id
            {
                if matched.is_some() {
                    return Err(CaptureError::Conflict(
                        "managed invocation occurrence has multiple retained bindings".into(),
                    ));
                }
                matched = Some(marker);
            }
        }
        Ok(matched)
    }

    pub(crate) fn managed_invocation_by_id(
        &self,
        marker_id: &str,
    ) -> Result<Option<ManagedInvocationV1>> {
        if !valid_digest(marker_id) {
            return Err(CaptureError::Invalid(
                "managed invocation marker identity is invalid".into(),
            ));
        }
        let path = self
            .root
            .join("managed-invocations")
            .join(format!("{marker_id}.json"));
        let _lock = self.lock()?;
        match read_bounded(&path, MAX_DEFERRED_BYTES) {
            Ok(bytes) => {
                let marker: ManagedInvocationV1 = serde_json::from_slice(&bytes)?;
                validate_managed_invocation(&marker)?;
                if marker.marker_id != marker_id {
                    return Err(CaptureError::Conflict(
                        "managed invocation filename differs from its identity".into(),
                    ));
                }
                Ok(Some(marker))
            }
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn occurrence_receipt(
        &self,
        observed_session_id: &str,
        tool_use_id: &str,
    ) -> Result<Option<NativeOccurrenceReceiptV1>> {
        if !bounded_identifier(observed_session_id, 255) || !bounded_identifier(tool_use_id, 255) {
            return Err(CaptureError::Invalid(
                "native occurrence identity is invalid".into(),
            ));
        }
        let marker_id = sha256(format!("{observed_session_id}\0{tool_use_id}").as_bytes());
        let _lock = self.lock()?;
        let state = load_compaction_state(&self.root, self.limits.max_total_bytes)?;
        let receipt = state.native_occurrences.get(&marker_id).cloned();
        if receipt.as_ref().is_some_and(|receipt| {
            !validate_occurrence_receipt(receipt)
                || receipt.observed_session_id != observed_session_id
                || receipt.tool_use_id != tool_use_id
        }) {
            return Err(CaptureError::Conflict(
                "compacted native occurrence identity is invalid".into(),
            ));
        }
        if let Some(receipt) = receipt.as_ref() {
            cleanup_compacted_occurrence_marker(&self.root, receipt)?;
        }
        Ok(receipt)
    }

    pub(crate) fn recipe_attempt_references_marker(&self, marker_id: &str) -> Result<bool> {
        if !valid_digest(marker_id) {
            return Err(CaptureError::Invalid(
                "native occurrence marker identity is invalid".into(),
            ));
        }
        let _lock = self.lock()?;
        for name in regular_file_names(&self.root.join("recipe-attempts"), self.limits.max_records)?
        {
            let bytes = read_bounded(
                &self.root.join("recipe-attempts").join(name),
                MAX_DEFERRED_BYTES,
            )?;
            let attempt: RecipeAttemptV1 = serde_json::from_slice(&bytes)?;
            validate_recipe_attempt(&attempt)?;
            if attempt.marker_id == marker_id {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Revalidates the admitted managed Pre and writes the configured ActionStart while holding
    /// the managed occurrence guard. The guard is released before the child is spawned.
    pub(crate) fn admit_configured_start(
        &self,
        attempt: &RecipeAttemptV1,
        start: ActionStartV1,
    ) -> Result<ActionStartV1> {
        validate_recipe_attempt(attempt)?;
        let _managed = self.lock_managed_capture()?;
        let marker_path = self
            .root
            .join("managed-invocations")
            .join(format!("{}.json", attempt.marker_id));
        let marker: ManagedInvocationV1 = {
            let _lock = self.lock()?;
            serde_json::from_slice(&read_bounded(&marker_path, MAX_DEFERRED_BYTES)?)?
        };
        validate_managed_invocation(&marker)?;
        if marker.marker_id != attempt.marker_id
            || marker.version != 4
            || marker.route != Some(NativeOccurrenceRouteV1::Managed)
            || marker.pre_fingerprint.is_none()
            || marker.source.as_ref() != Some(&start.source)
        {
            return Err(CaptureError::Conflict(
                "configured recipe start differs from its durable managed Pre claim".into(),
            ));
        }
        let pre_operation_id = format!("managed:event:{}:pre", marker.marker_id);
        if !matches!(
            self.replayable_event_batch(&pre_operation_id)?,
            Some(CaptureBatchV1 { .. })
        ) {
            return Err(CaptureError::Conflict(
                "configured recipe invocation has no durable admitted Pre facts".into(),
            ));
        }
        if self
            .action_start(&format!("native:start:{}", marker.marker_id))?
            .is_some()
            || self
                .action_finish(&format!("native:finish:{}", marker.marker_id))?
                .is_some()
        {
            return Err(CaptureError::Conflict(
                "managed occurrence already entered generic fallback".into(),
            ));
        }
        let retained_attempt = self.recipe_attempt(&attempt.fence_id)?.ok_or_else(|| {
            CaptureError::Conflict("configured recipe attempt disappeared".into())
        })?;
        if retained_attempt != *attempt {
            return Err(CaptureError::Conflict(
                "configured recipe attempt changed before start admission".into(),
            ));
        }
        let (_, start) = self.append_action_start_ordered(start)?;
        Ok(start)
    }

    pub(crate) fn bind_managed_phase(
        &self,
        marker_id: &str,
        phase: &str,
        fingerprint: &str,
    ) -> Result<ManagedInvocationV1> {
        if !valid_digest(marker_id)
            || !matches!(phase, "pre" | "post")
            || !valid_digest(fingerprint)
        {
            return Err(CaptureError::Invalid(
                "managed phase identity is invalid".into(),
            ));
        }
        let path = self
            .root
            .join("managed-invocations")
            .join(format!("{marker_id}.json"));
        let _lock = self.lock()?;
        let bytes = read_bounded(&path, MAX_DEFERRED_BYTES)?;
        let mut marker: ManagedInvocationV1 = serde_json::from_slice(&bytes)?;
        validate_managed_invocation(&marker)?;
        if marker.version != 4 || marker.marker_id != marker_id {
            return Err(CaptureError::Conflict(
                "legacy managed invocation is inspect/export only".into(),
            ));
        }
        let retained = if phase == "pre" {
            &mut marker.pre_fingerprint
        } else {
            &mut marker.post_fingerprint
        };
        match retained {
            Some(existing) if existing != fingerprint => {
                return Err(CaptureError::Conflict(
                    "managed provider occurrence phase was replayed with different native facts"
                        .into(),
                ));
            }
            Some(_) => return Ok(marker),
            None => *retained = Some(fingerprint.to_owned()),
        }
        let replacement = serde_json::to_vec(&marker)?;
        self.reserve_replace(&path, replacement.len() as u64)?;
        atomic_replace(&path, &replacement)?;
        Ok(marker)
    }

    pub(crate) fn enrich_managed_invocation_source(
        &self,
        marker_id: &str,
        source: SourceBindingV1,
    ) -> Result<ManagedInvocationV1> {
        if !valid_digest(marker_id) {
            return Err(CaptureError::Invalid(
                "managed invocation marker identity is invalid".into(),
            ));
        }
        let path = self
            .root
            .join("managed-invocations")
            .join(format!("{marker_id}.json"));
        let _lock = self.lock()?;
        let bytes = read_bounded(&path, MAX_DEFERRED_BYTES)?;
        let mut marker: ManagedInvocationV1 = serde_json::from_slice(&bytes)?;
        validate_managed_invocation(&marker)?;
        if marker.version != 4 || marker.marker_id != marker_id {
            return Err(CaptureError::Conflict(
                "legacy managed invocation is inspect/export only".into(),
            ));
        }
        match marker.source.as_ref() {
            Some(existing) if existing != &source => {
                return Err(CaptureError::Conflict(
                    "managed invocation source differs from its retained original".into(),
                ));
            }
            Some(_) => return Ok(marker),
            None => marker.source = Some(source),
        }
        let replacement = serde_json::to_vec(&marker)?;
        self.reserve_replace(&path, replacement.len() as u64)?;
        atomic_replace(&path, &replacement)?;
        Ok(marker)
    }

    /// Returns the full retained event facts needed to replay a native occurrence or a
    /// provider-history gap across the following durable state transition and compaction.
    pub(crate) fn replayable_event_batch(
        &self,
        operation_id: &str,
    ) -> Result<Option<CaptureBatchV1>> {
        if !replayable_event_operation(operation_id) {
            return Err(CaptureError::Invalid(
                "replayable event operation identity is invalid".into(),
            ));
        }
        let key = envelope_key_parts("event", operation_id);
        let _lock = self.lock()?;
        let path = self.root.join("outbox").join(format!("{key}.json"));
        match read_bounded(&path, self.limits.max_total_bytes) {
            Ok(bytes) => {
                let envelope: CaptureEnvelopeV1 = serde_json::from_slice(&bytes)?;
                envelope
                    .validate()
                    .map_err(|error| CaptureError::Invalid(error.to_string()))?;
                match envelope {
                    CaptureEnvelopeV1::EventBatch(batch) if batch.operation_id == operation_id => {
                        Ok(Some(batch))
                    }
                    _ => Err(CaptureError::Conflict(
                        "replayable event identity has the wrong retained envelope".into(),
                    )),
                }
            }
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let state = load_compaction_state(&self.root, self.limits.max_total_bytes)?;
                let Some(compacted) = state.entries.get(&key) else {
                    return Ok(None);
                };
                match compacted.envelope.clone() {
                    Some(CaptureEnvelopeV1::EventBatch(batch))
                        if batch.operation_id == operation_id =>
                    {
                        Ok(Some(batch))
                    }
                    Some(_) => Err(CaptureError::Conflict(
                        "replayable event identity has the wrong compacted envelope".into(),
                    )),
                    None => Err(CaptureError::Conflict(
                        "event replay facts are unavailable; inspect/export the spool and keep it disabled"
                            .into(),
                    )),
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn remove_managed_invocation(&self, marker_id: &str) -> Result<()> {
        if !valid_digest(marker_id) {
            return Err(CaptureError::Invalid(
                "managed invocation marker identity is invalid".into(),
            ));
        }
        let directory = self.root.join("managed-invocations");
        let path = directory.join(format!("{marker_id}.json"));
        let _lock = self.lock()?;
        let bytes = match read_bounded(&path, MAX_DEFERRED_BYTES) {
            Ok(bytes) => bytes,
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let marker: ManagedInvocationV1 = serde_json::from_slice(&bytes)?;
        validate_managed_invocation(&marker)?;
        if marker.version != 4 {
            return Err(CaptureError::Conflict(
                "legacy native occurrence claim requires inspect/export and an explicit new spool identity"
                    .into(),
            ));
        }
        let receipt = occurrence_receipt_from_marker(&marker)?;
        if !validate_occurrence_receipt(&receipt) {
            return Err(CaptureError::Conflict(
                "native occurrence compact receipt is invalid".into(),
            ));
        }
        let compaction_path = self.root.join("compaction-v1.json");
        let mut state = load_compaction_state(&self.root, self.limits.max_total_bytes)?;
        match state.native_occurrences.get(marker_id) {
            Some(existing) if existing != &receipt => {
                return Err(CaptureError::Conflict(
                    "native occurrence compact identity differs from retained claim".into(),
                ));
            }
            Some(_) => {}
            None => {
                if state.native_occurrences.len() >= MAX_SEQUENCE_EVENTS {
                    return Err(CaptureError::Limit(
                        "native occurrence compact receipt limit reached; export is required"
                            .into(),
                    ));
                }
                state
                    .native_occurrences
                    .insert(marker_id.to_owned(), receipt);
                let replacement = serde_json::to_vec(&state)?;
                self.reserve_replace(&compaction_path, replacement.len() as u64)?;
                atomic_replace(&compaction_path, &replacement)?;
            }
        }
        #[cfg(test)]
        if FAIL_OCCURRENCE_UNLINK_AFTER_RECEIPT.with(std::cell::Cell::get) {
            return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
        }
        fs::remove_file(path)?;
        File::open(directory)?.sync_all()?;
        File::open(&self.root)?.sync_all().map_err(Into::into)
    }

    pub(crate) fn reserve_recipe_attempt(
        &self,
        reservation: &RecipeAttemptReservationV1,
    ) -> Result<(RecipeAttemptV1, bool)> {
        self.ensure_v4_mutation_identity()?;
        validate_recipe_attempt_reservation(reservation)?;
        let _managed = self.lock_managed_capture()?;
        let attempt_path = self
            .root
            .join("recipe-attempts")
            .join(format!("{}.json", reservation.fence_id));
        let _lock = self.lock()?;
        match read_bounded(&attempt_path, MAX_DEFERRED_BYTES) {
            Ok(bytes) => {
                let attempt: RecipeAttemptV1 = serde_json::from_slice(&bytes)?;
                validate_recipe_attempt(&attempt)?;
                validate_recipe_attempt_binding(&attempt, reservation)?;
                return Ok((attempt, false));
            }
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut pending = Vec::new();
        let mut completed = Vec::new();
        let mut unresolved = Vec::new();
        for name in regular_file_names(
            &self.root.join("managed-invocations"),
            self.limits.max_records,
        )? {
            let bytes = read_bounded(
                &self.root.join("managed-invocations").join(&name),
                MAX_DEFERRED_BYTES,
            )?;
            let marker: ManagedInvocationV1 = serde_json::from_slice(&bytes)?;
            validate_managed_invocation(&marker)?;
            if marker.version != 4 || marker.route != Some(NativeOccurrenceRouteV1::Managed) {
                return Err(CaptureError::Conflict(
                    "legacy target-bound managed invocation is inspect/export only".into(),
                ));
            }
            if marker.observed_session_id == reservation.observed_session_id
                && marker.canonical_session_id == reservation.canonical_session_id
                && marker.work_id == reservation.work_id
                && marker.generation == reservation.generation
                && marker.command_digest == reservation.command_digest
                && marker.repository == reservation.repository
                && marker.worktree_id == reservation.worktree_id
                && marker.root_path.as_deref() == Some(reservation.root_path.as_str())
                && marker.root_device == reservation.root_device
                && marker.root_inode == reservation.root_inode
                && marker.setup_digest.as_deref() == Some(reservation.setup_digest.as_str())
                && marker.source.is_some()
                && marker.operation_prefix.is_some()
                && marker.capture_mode.is_some()
            {
                let pre_operation_id = format!("managed:event:{}:pre", marker.marker_id);
                if marker.pre_fingerprint.is_none()
                    || !matches!(
                        retained_envelope(&self.root, self.limits, "event", &pre_operation_id,)?,
                        Some(CaptureEnvelopeV1::EventBatch(_))
                    )
                {
                    return Err(CaptureError::Conflict(
                        "configured recipe invocation has no durable admitted Pre facts".into(),
                    ));
                }
                let start_operation_id = format!("recipe:start:{}", marker.marker_id);
                let finish_operation_id = format!("recipe:finish:{}", marker.marker_id);
                let start =
                    retained_envelope(&self.root, self.limits, "start", &start_operation_id)?;
                let finish =
                    retained_envelope(&self.root, self.limits, "finish", &finish_operation_id)?;
                match (start, finish) {
                    (None, None) => pending.push(marker),
                    (
                        Some(CaptureEnvelopeV1::ActionStart(_)),
                        Some(CaptureEnvelopeV1::ActionFinish(_)),
                    ) => {
                        completed.push(marker);
                    }
                    (Some(CaptureEnvelopeV1::ActionStart(_)), None) => unresolved.push(marker),
                    (None, Some(CaptureEnvelopeV1::ActionFinish(_))) => {
                        return Err(CaptureError::Conflict(
                            "configured recipe finish exists without its managed start".into(),
                        ));
                    }
                    _ => {
                        return Err(CaptureError::Conflict(
                            "configured recipe operation identity has the wrong envelope kind"
                                .into(),
                        ));
                    }
                }
            }
        }
        let mut matches = if !unresolved.is_empty() {
            unresolved
        } else if pending.is_empty() {
            completed
        } else {
            pending
        };
        if matches.len() != 1 {
            return Err(if matches.is_empty() {
                CaptureError::NotFound(
                    "configured recipe has no exact pending managed invocation".into(),
                )
            } else {
                CaptureError::Conflict(
                    "configured recipe has multiple pending managed invocations".into(),
                )
            });
        }
        let marker = matches.pop().expect("one managed invocation match");
        let attempt = RecipeAttemptV1 {
            version: 1,
            fence_id: reservation.fence_id.clone(),
            marker_id: marker.marker_id.clone(),
            work_id: reservation.work_id.clone(),
            generation: reservation.generation,
            observed_session_id: reservation.observed_session_id.clone(),
            canonical_session_id: reservation.canonical_session_id.clone(),
            repository: reservation.repository.clone(),
            worktree_id: reservation.worktree_id.clone(),
            root_path: reservation.root_path.clone(),
            root_device: reservation.root_device,
            root_inode: reservation.root_inode,
            base_ref: reservation.base_ref.clone(),
            recipe_id: reservation.recipe_id.clone(),
            recipe_digest: reservation.recipe_digest.clone(),
            setup_digest: reservation.setup_digest.clone(),
            coverage: reservation.coverage,
            host_id: reservation.host_id.clone(),
            boot_id: reservation.boot_id.clone(),
            start_operation_id: format!("recipe:start:{}", marker.marker_id),
            finish_operation_id: format!("recipe:finish:{}", marker.marker_id),
            action_id: format!("act:{}", marker.marker_id),
            phase: RecipeAttemptPhaseV1::Reserved,
        };
        validate_recipe_attempt(&attempt)?;
        let bytes = serde_json::to_vec(&attempt)?;
        self.reserve(bytes.len() as u64, 1)?;
        atomic_write(&attempt_path, &bytes)?;
        Ok((attempt, true))
    }

    pub(crate) fn mark_recipe_attempt_spawned(
        &self,
        fence_id: &str,
        marker_id: &str,
        pid: u32,
        pgid: u32,
    ) -> Result<()> {
        if !valid_digest(fence_id) || !valid_digest(marker_id) || pid == 0 || pgid == 0 {
            return Err(CaptureError::Invalid(
                "spawned recipe attempt identity is invalid".into(),
            ));
        }
        let path = self
            .root
            .join("recipe-attempts")
            .join(format!("{fence_id}.json"));
        let _lock = self.lock()?;
        let mut attempt: RecipeAttemptV1 =
            serde_json::from_slice(&read_bounded(&path, MAX_DEFERRED_BYTES)?)?;
        validate_recipe_attempt(&attempt)?;
        if attempt.marker_id != marker_id {
            return Err(CaptureError::Conflict(
                "spawned recipe attempt differs from the retained reservation".into(),
            ));
        }
        match attempt.phase {
            RecipeAttemptPhaseV1::Reserved => {
                attempt.phase = RecipeAttemptPhaseV1::Spawned { pid, pgid };
            }
            RecipeAttemptPhaseV1::Spawned {
                pid: retained_pid,
                pgid: retained_pgid,
            } if retained_pid == pid && retained_pgid == pgid => return Ok(()),
            RecipeAttemptPhaseV1::Spawned { .. } => {
                return Err(CaptureError::Conflict(
                    "spawned recipe process identity changed".into(),
                ));
            }
        }
        let bytes = serde_json::to_vec(&attempt)?;
        self.reserve_replace(&path, bytes.len() as u64)?;
        atomic_replace(&path, &bytes)
    }

    pub(crate) fn recipe_attempt(&self, fence_id: &str) -> Result<Option<RecipeAttemptV1>> {
        if !valid_digest(fence_id) {
            return Err(CaptureError::Invalid(
                "configured recipe fence identity is invalid".into(),
            ));
        }
        let path = self
            .root
            .join("recipe-attempts")
            .join(format!("{fence_id}.json"));
        let _lock = self.lock()?;
        match read_bounded(&path, MAX_DEFERRED_BYTES) {
            Ok(bytes) => {
                let attempt: RecipeAttemptV1 = serde_json::from_slice(&bytes)?;
                validate_recipe_attempt(&attempt)?;
                Ok(Some(attempt))
            }
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn retained_observed_order(&self, operation_id: &str) -> Result<Option<u64>> {
        if !bounded_identifier(operation_id, 255) {
            return Err(CaptureError::Invalid(
                "observed operation id is invalid".into(),
            ));
        }
        let _lock = self.lock()?;
        let state = load_observed_order_state(
            &self.root.join("order-v1.json"),
            self.limits.max_total_bytes,
        )?;
        Ok(state.orders.get(operation_id).copied())
    }

    pub(crate) fn clear_recipe_attempt(&self, attempt: &RecipeAttemptV1) -> Result<()> {
        validate_recipe_attempt(attempt)?;
        let directory = self.root.join("recipe-attempts");
        let path = directory.join(format!("{}.json", attempt.fence_id));
        let _lock = self.lock()?;
        let retained: RecipeAttemptV1 =
            serde_json::from_slice(&read_bounded(&path, MAX_DEFERRED_BYTES)?)?;
        validate_recipe_attempt(&retained)?;
        let mut expected = attempt.clone();
        expected.phase = retained.phase.clone();
        if retained != expected {
            return Err(CaptureError::Conflict(
                "recipe attempt changed before it could be cleared".into(),
            ));
        }
        fs::remove_file(path)?;
        File::open(directory)?.sync_all()?;
        Ok(())
    }

    /// Remembers the last Hub-authoritative target for an observed native session. Deferred
    /// records copy this value when they are created, so a later generation cannot reattribute an
    /// earlier outage.
    pub fn remember_session_association(
        &self,
        observed_session_id: &str,
        canonical_session_id: &str,
        work_id: &str,
        generation: i64,
        parent_session_id: Option<&str>,
        capture_mode: CaptureModeV1,
    ) -> Result<()> {
        let association = SessionAssociationV1 {
            observed_session_id: observed_session_id.to_owned(),
            canonical_session_id: canonical_session_id.to_owned(),
            work_id: work_id.to_owned(),
            generation,
            parent_session_id: parent_session_id.map(str::to_owned),
            capture_mode,
            original_capture_mode: None,
        };
        validate_association(&association)?;
        let path = association_path(&self.root, observed_session_id);
        let _lock = self.lock()?;
        let first = load_association(&self.root, observed_session_id)?
            .map_or(capture_mode, |earlier| earlier.first_capture_mode());
        let association = SessionAssociationV1 {
            capture_mode: clamp_shared(first, capture_mode),
            original_capture_mode: Some(first),
            ..association
        };
        let bytes = serde_json::to_vec(&association)?;
        if load_association(&self.root, observed_session_id)?.as_ref() == Some(&association) {
            return Ok(());
        }
        self.reserve_replace(&path, bytes.len() as u64)?;
        atomic_replace(&path, &bytes)
    }

    /// Returns the parent session and capture mode that identify this canonical session to the Hub,
    /// pinning the supplied live values the first time the (Work, generation, session) is seen.
    /// The Hub compares both as part of session identity, so a later mode change or a parent id
    /// that appears mid-session must not alter what later batches carry.
    pub fn pin_session_identity(
        &self,
        observed_session_id: &str,
        canonical_session_id: &str,
        work_id: &str,
        generation: i64,
        parent_session_id: Option<&str>,
        capture_mode: CaptureModeV1,
    ) -> Result<(Option<String>, CaptureModeV1)> {
        let mut live = SessionAssociationV1 {
            observed_session_id: observed_session_id.to_owned(),
            canonical_session_id: canonical_session_id.to_owned(),
            work_id: work_id.to_owned(),
            generation,
            parent_session_id: parent_session_id.map(str::to_owned),
            capture_mode,
            original_capture_mode: Some(capture_mode),
        };
        validate_association(&live)?;
        let _lock = self.lock()?;
        let earlier = load_association(&self.root, observed_session_id)?;
        if let Some(pinned) = &earlier
            && pinned.canonical_session_id == canonical_session_id
            && pinned.work_id == work_id
            && pinned.generation == generation
        {
            return Ok((
                pinned.parent_session_id.clone(),
                clamp_shared(pinned.first_capture_mode(), pinned.capture_mode),
            ));
        }
        // A session that began unshared never starts sharing, even under a new Work or generation.
        if let Some(earlier) = &earlier {
            let first = earlier.first_capture_mode();
            live.original_capture_mode = Some(first);
            live.capture_mode = clamp_shared(first, capture_mode);
        }
        let bytes = serde_json::to_vec(&live)?;
        let path = association_path(&self.root, observed_session_id);
        self.reserve_replace(&path, bytes.len() as u64)?;
        atomic_replace(&path, &bytes)?;
        Ok((live.parent_session_id, live.capture_mode))
    }

    /// Returns deferred occurrences that were bound to the exact target current when capture was
    /// interrupted. The sanitized deferred facts stay local; callers emit only explicit gaps.
    pub(crate) fn deferred_gaps(&self, observed_session_id: &str) -> Result<Vec<DeferredGapV1>> {
        validate_session_key(observed_session_id, "deferred")?;
        let _lock = self.lock()?;
        let mut records = deferred_records(&self.root, self.limits.max_records)?;
        records.sort_by_key(|record| record.receipt_order);
        Ok(records
            .into_iter()
            .filter(|record| record.hook.session_id == observed_session_id)
            .filter_map(|record| {
                record.target.zip(record.hook.reserved_sequence).map(
                    |(target, reserved_sequence)| DeferredGapV1 {
                        occurrence_id: record.hook.occurrence_id,
                        reserved_sequence,
                        observed_at: record.hook.observed_at,
                        target,
                    },
                )
            })
            .collect())
    }

    /// Binds this private spool to one Hub/repository/worktree namespace. A mismatch is reported
    /// without replacing the durable marker.
    pub fn bind_namespace(&self, namespace: SpoolNamespaceV1) -> Result<NamespaceBindOutcome> {
        validate_namespace(&namespace)?;
        let bytes = serde_json::to_vec(&namespace)?;
        let path = self.root.join("namespace-v1.json");
        let _lock = self.lock()?;
        match read_bounded(&path, MAX_DEFERRED_BYTES) {
            Ok(existing) => {
                let retained: SpoolNamespaceV1 =
                    serde_json::from_slice(&existing).map_err(|_| {
                        CaptureError::Invalid("capture spool namespace is invalid".into())
                    })?;
                validate_namespace(&retained)?;
                if retained == namespace {
                    Ok(NamespaceBindOutcome::Duplicate)
                } else {
                    Ok(NamespaceBindOutcome::Conflict)
                }
            }
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.reserve(bytes.len() as u64, 1)?;
                atomic_write(&path, &bytes)?;
                Ok(NamespaceBindOutcome::Stored)
            }
            Err(error) => Err(error),
        }
    }

    /// Retains a sanitized, explicitly unassociated hook. Exact replay is idempotent; reuse of
    /// an occurrence id for different facts is rejected.
    pub fn retain_deferred(&self, deferred: &DeferredHookV1) -> Result<AppendOutcome> {
        validate_deferred(deferred)?;
        if deferred.reserved_sequence.is_none() {
            return Err(CaptureError::Invalid(
                "new deferred capture records require a reserved sequence".into(),
            ));
        }
        let path = self.root.join("deferred").join(format!(
            "{}.json",
            sha256(deferred.occurrence_id.as_bytes())
        ));
        let _lock = self.lock()?;
        if path.exists() {
            let existing = read_bounded(&path, MAX_DEFERRED_BYTES)?;
            let existing = decode_deferred_record(&existing)?;
            // The occurrence id already fixes any provider timestamp; a differing time is only a
            // later receipt of the same hook, so it is the same record.
            let same = DeferredHookV1 {
                observed_at: existing.hook.observed_at,
                ..deferred.clone()
            };
            return if existing.hook == same {
                Ok(AppendOutcome::Duplicate)
            } else {
                Err(CaptureError::Conflict(
                    "deferred occurrence id was reused with different facts".into(),
                ))
            };
        }
        let receipt_order = deferred_records(&self.root, self.limits.max_records)?
            .into_iter()
            .map(|record| record.receipt_order)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| CaptureError::Limit("deferred receipt order overflow".into()))?;
        let target = load_association(&self.root, &deferred.session_id)?;
        let bytes = serde_json::to_vec(&DeferredRecordV1 {
            receipt_order,
            hook: deferred.clone(),
            target,
        })?;
        if bytes.len() as u64 > MAX_DEFERRED_BYTES {
            return Err(CaptureError::Limit(
                "deferred capture record exceeds its byte bound".into(),
            ));
        }
        let outstanding = || outstanding_actions(&self.root, self.limits.max_records);
        self.reserve_with_action_capacity(
            bytes.len() as u64,
            1,
            outstanding,
            BASE_METADATA_RESERVE_BYTES.min(self.limits.max_total_bytes / 4),
            BASE_METADATA_RESERVE_RECORDS.min(self.limits.max_records / 4),
        )?;
        atomic_write(&path, &bytes)?;
        Ok(AppendOutcome::Stored)
    }

    /// Lists sanitized unassociated hooks for local diagnosis only. There is deliberately no
    /// acknowledgement or reassociation API in schema v1.
    pub fn deferred(&self, limit: usize) -> Result<Vec<DeferredHookV1>> {
        if limit == 0 || limit > MAX_PENDING_PAGE {
            return Err(CaptureError::Invalid(format!(
                "deferred limit must be 1..{MAX_PENDING_PAGE}"
            )));
        }
        let _lock = self.lock()?;
        let mut records = deferred_records(&self.root, self.limits.max_records)?;
        records.sort_by_key(|record| record.receipt_order);
        records.truncate(limit);
        Ok(records.into_iter().map(|record| record.hook).collect())
    }

    /// Lists bounded private crash debris for inspection. Entries remain retained until an
    /// explicit named prune; ordinary capture never deletes quarantine evidence automatically.
    pub fn quarantine(&self, limit: usize) -> Result<Vec<QuarantineEntryV1>> {
        if limit == 0 || limit > MAX_PENDING_PAGE {
            return Err(CaptureError::Invalid(format!(
                "quarantine limit must be 1..{MAX_PENDING_PAGE}"
            )));
        }
        let _lock = self.lock()?;
        recover_spool_temps(&self.root, self.limits)?;
        quarantine_entries(&self.root, limit)
    }

    /// Explicitly deletes exact inspected quarantine entries. Only opaque names previously
    /// returned by [`Self::quarantine`] are accepted; active writer temps are outside this path.
    pub fn prune_quarantine(&self, names: &[String]) -> Result<QuarantinePruneReport> {
        if names.is_empty() || names.len() > MAX_PENDING_PAGE {
            return Err(CaptureError::Invalid(format!(
                "quarantine prune must name 1..{MAX_PENDING_PAGE} entries"
            )));
        }
        let mut unique = BTreeSet::new();
        for name in names {
            if !valid_quarantine_name(name) || !unique.insert(name.as_str()) {
                return Err(CaptureError::Invalid(
                    "quarantine prune contains an invalid or duplicate name".into(),
                ));
            }
        }
        let _lock = self.lock()?;
        recover_spool_temps(&self.root, self.limits)?;
        let quarantine = self.root.join("quarantine");
        let mut bytes_reclaimed = 0_u64;
        for name in names {
            let path = quarantine.join(name);
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    CaptureError::NotFound(format!("quarantine entry {name} is no longer retained"))
                } else {
                    error.into()
                }
            })?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(CaptureError::Invalid(
                    "quarantine contains a non-regular retained entry".into(),
                ));
            }
            bytes_reclaimed = bytes_reclaimed.saturating_add(metadata.len());
            fs::remove_file(path)?;
        }
        File::open(quarantine)?.sync_all()?;
        Ok(QuarantinePruneReport {
            records: names.len(),
            bytes_reclaimed,
        })
    }

    /// Binds a local-only native observation fingerprint to one operation. The fingerprint is
    /// never included in a Hub envelope, but prevents a provider retry from silently changing raw
    /// native result fields that are outside the shared privacy allowlist.
    pub fn bind_observation(&self, operation_id: &str, fingerprint: &str) -> Result<AppendOutcome> {
        if !bounded_identifier(operation_id, 255) || !valid_digest(fingerprint) {
            return Err(CaptureError::Invalid(
                "native observation identity is invalid".into(),
            ));
        }
        let path = self
            .root
            .join("observations")
            .join(sha256(operation_id.as_bytes()));
        let _lock = self.lock()?;
        if path.exists() {
            return if read_bounded(&path, 64)? == fingerprint.as_bytes() {
                Ok(AppendOutcome::Duplicate)
            } else {
                Err(CaptureError::Conflict(
                    "native operation was replayed with different observed result fields".into(),
                ))
            };
        }
        let observation_key = sha256(operation_id.as_bytes());
        if let Some(existing) = load_compaction_state(&self.root, self.limits.max_total_bytes)?
            .observations
            .get(&observation_key)
        {
            return if existing == fingerprint {
                Ok(AppendOutcome::Duplicate)
            } else {
                Err(CaptureError::Conflict(
                    "native operation was replayed with different observed result fields".into(),
                ))
            };
        }
        self.reserve(fingerprint.len() as u64, 1)?;
        atomic_write(&path, fingerprint.as_bytes())?;
        Ok(AppendOutcome::Stored)
    }

    /// Requires an observation fingerprint that was durably bound before the corresponding
    /// managed event became replayable. Missing facts are uncertainty, not permission to bless a
    /// replay after marker removal.
    pub(crate) fn verify_observation(&self, operation_id: &str, fingerprint: &str) -> Result<()> {
        if !bounded_identifier(operation_id, 255) || !valid_digest(fingerprint) {
            return Err(CaptureError::Invalid(
                "native observation identity is invalid".into(),
            ));
        }
        let key = sha256(operation_id.as_bytes());
        let _lock = self.lock()?;
        match read_bounded(&self.root.join("observations").join(&key), 64) {
            Ok(existing) if existing == fingerprint.as_bytes() => Ok(()),
            Ok(_) => Err(CaptureError::Conflict(
                "native operation was replayed with different observed result fields".into(),
            )),
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                match load_compaction_state(&self.root, self.limits.max_total_bytes)?
                    .observations
                    .get(&key)
                {
                    Some(existing) if existing == fingerprint => Ok(()),
                    Some(_) => Err(CaptureError::Conflict(
                        "native operation was replayed with different observed result fields"
                            .into(),
                    )),
                    None => Err(CaptureError::Conflict(
                        "managed replay fingerprint is unavailable; inspect/export the spool and keep it disabled"
                            .into(),
                    )),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Retains bytes before returning their reference. The caller decides the retention class;
    /// envelope validation rejects `SharedApproved` outside shared-conversation event bodies.
    pub fn retain_artifact(
        &self,
        bytes: &[u8],
        media: ArtifactMediaV1,
        retention: RetentionClassV1,
    ) -> Result<ArtifactRefV1> {
        let size_bytes = u64::try_from(bytes.len())
            .map_err(|_| CaptureError::Limit("artifact size is out of range".into()))?;
        if size_bytes > self.limits.max_artifact_bytes {
            return Err(CaptureError::Limit(format!(
                "local artifact exceeds {} bytes",
                self.limits.max_artifact_bytes
            )));
        }
        let digest = sha256(bytes);
        let path = self.root.join("artifacts").join(&digest);
        let _lock = self.lock()?;
        if path.exists() {
            let existing = read_bounded(&path, self.limits.max_artifact_bytes)?;
            if existing != bytes {
                return Err(CaptureError::Conflict(
                    "local artifact digest collides with different bytes".into(),
                ));
            }
        } else {
            self.reserve_artifact(bytes.len() as u64, 1)?;
            atomic_write(&path, bytes)?;
        }
        Ok(ArtifactRefV1 {
            digest,
            size_bytes,
            media,
            retention,
            availability: ContentAvailabilityV1::Local,
        })
    }

    /// Returns a non-retained reference for a failed local retention attempt. This keeps content
    /// availability distinct from the observed command result.
    pub fn missing_artifact_reference(
        &self,
        bytes: &[u8],
        media: ArtifactMediaV1,
        retention: RetentionClassV1,
    ) -> ArtifactRefV1 {
        ArtifactRefV1 {
            digest: sha256(bytes),
            size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            media,
            retention,
            availability: ContentAvailabilityV1::Missing,
        }
    }

    pub fn read_artifact(&self, reference: &ArtifactRefV1) -> Result<Vec<u8>> {
        if reference.availability != ContentAvailabilityV1::Local {
            return Err(CaptureError::NotFound(
                "artifact is not recorded as locally available".into(),
            ));
        }
        if reference.size_bytes > self.limits.max_artifact_bytes {
            return Err(CaptureError::Limit(
                "artifact reference exceeds this spool's read bound".into(),
            ));
        }
        let bytes = read_bounded(
            &self.root.join("artifacts").join(&reference.digest),
            reference.size_bytes,
        )?;
        if bytes.len() as u64 != reference.size_bytes || sha256(&bytes) != reference.digest {
            return Err(CaptureError::Conflict(
                "local artifact failed size or digest verification".into(),
            ));
        }
        Ok(bytes)
    }

    pub fn append(&self, envelope: &CaptureEnvelopeV1) -> Result<AppendOutcome> {
        self.append_inner(envelope, false, false)
    }

    /// Atomically admits a new action start together with its durable local observation order.
    /// If start persistence fails, a newly allocated order is rolled back under the same spool
    /// lock using the capacity admitted before either write.
    pub(crate) fn append_action_start_ordered(
        &self,
        mut start: ActionStartV1,
    ) -> Result<(AppendOutcome, ActionStartV1)> {
        if start.observed_order != 0 || start.observation_scope_id.is_some() {
            return Err(CaptureError::Invalid(
                "new action start must not supply a local observation order".into(),
            ));
        }
        let order_path = self.root.join("order-v1.json");
        let key = envelope_key_parts("start", &start.operation_id);
        let path = self.root.join("outbox").join(format!("{key}.json"));
        let _lock = self.lock()?;
        if let Some(CaptureEnvelopeV1::ActionStart(retained)) =
            retained_envelope(&self.root, self.limits, "start", &start.operation_id)?
        {
            start.observed_order = retained.observed_order;
            start.observation_scope_id = retained.observation_scope_id.clone();
            start.started_at = retained.started_at;
            let candidate = CaptureEnvelopeV1::ActionStart(start);
            return if candidate == CaptureEnvelopeV1::ActionStart(retained.clone()) {
                Ok((AppendOutcome::Duplicate, retained))
            } else {
                Err(CaptureError::Conflict(
                    "capture action start was reused with different facts".into(),
                ))
            };
        }
        let mut state = load_observed_order_state(&order_path, self.limits.max_total_bytes)?;
        let original_state = serde_json::to_vec(&state)?;
        let mut allocated = false;
        let order = if let Some(order) = state.orders.get(&start.operation_id) {
            *order
        } else {
            if state.orders.len() >= 65_536 || state.next == 0 {
                return Err(CaptureError::Limit(
                    "observed action order reached its retention bound".into(),
                ));
            }
            let order = state.next;
            state.next = state
                .next
                .checked_add(1)
                .ok_or_else(|| CaptureError::Limit("observed action order overflow".into()))?;
            state.orders.insert(start.operation_id.clone(), order);
            allocated = true;
            order
        };
        start.observed_order = order;
        start.observation_scope_id = Some(load_observation_scope(&self.root)?.id);
        let envelope = CaptureEnvelopeV1::ActionStart(start.clone());
        envelope
            .validate()
            .map_err(|error| CaptureError::Invalid(error.to_string()))?;
        let envelope_bytes = serde_json::to_vec(&envelope)?;
        if let Some(compacted) = load_compaction_state(&self.root, self.limits.max_total_bytes)?
            .entries
            .get(&key)
        {
            return if compacted.digest == sha256(&envelope_bytes) {
                Ok((AppendOutcome::Duplicate, start))
            } else {
                Err(CaptureError::Conflict(
                    "capture action start differs from compacted history".into(),
                ))
            };
        }
        let order_bytes = serde_json::to_vec(&state)?;
        if order_bytes.len() as u64 > self.limits.max_total_bytes / 4 {
            return Err(CaptureError::Limit(
                "observed action order exceeds its byte bound".into(),
            ));
        }
        let outstanding = || {
            let mut outstanding = outstanding_action_keys(&self.root, self.limits.max_records)?;
            outstanding.insert(action_key_start(&start));
            Ok(outstanding.len())
        };
        self.reserve_with_action_capacity(
            (envelope_bytes.len() as u64).saturating_add(if allocated {
                order_bytes.len() as u64
            } else {
                0
            }),
            1 + usize::from(allocated),
            outstanding,
            BASE_METADATA_RESERVE_BYTES.min(self.limits.max_total_bytes / 4),
            BASE_METADATA_RESERVE_RECORDS.min(self.limits.max_records / 4),
        )?;
        if allocated {
            atomic_replace(&order_path, &order_bytes)?;
        }
        if let Err(error) = atomic_write(&path, &envelope_bytes) {
            match read_bounded(&path, self.limits.max_total_bytes) {
                Ok(visible) if visible == envelope_bytes => {
                    // Rename made the exact start visible, but directory durability is uncertain.
                    // Its order must remain reserved and replayable even though this call fails.
                    return Err(error);
                }
                Ok(_) => {
                    return Err(CaptureError::Conflict(
                        "action start write became visible with different bytes".into(),
                    ));
                }
                Err(CaptureError::Io(read_error))
                    if read_error.kind() == std::io::ErrorKind::NotFound =>
                {
                    if allocated {
                        atomic_replace(&order_path, &original_state).map_err(|rollback| {
                            CaptureError::Conflict(format!(
                                "action start failed ({error}); observation-order rollback failed ({rollback})"
                            ))
                        })?;
                    }
                }
                Err(_) => {
                    // Visibility cannot be proved absent. Preserve the mapping and high-water
                    // rather than risk reusing an order that may already be in a visible start.
                    return Err(error);
                }
            }
            return Err(error);
        }
        Ok((AppendOutcome::Stored, start))
    }

    /// Retains a bounded health/gap envelope from the metadata headroom that ordinary content
    /// cannot consume. Callers must not use this path for ordinary events or artifacts.
    pub(crate) fn append_priority(&self, envelope: &CaptureEnvelopeV1) -> Result<AppendOutcome> {
        check_priority_envelope(envelope)?;
        self.append_inner(envelope, true, false)
    }

    /// Appends an event batch whose observation time is only the hook's receipt time. A retained
    /// or compacted batch under the same operation id is a re-delivery of the same hook, so it is
    /// a duplicate even though its receipt time differs. The check and the append share one lock.
    pub(crate) fn append_at_receipt(&self, envelope: &CaptureEnvelopeV1) -> Result<AppendOutcome> {
        self.append_inner(envelope, false, true)
    }

    /// Priority variant of [`Self::append_at_receipt`] for gap events.
    pub(crate) fn append_priority_at_receipt(
        &self,
        envelope: &CaptureEnvelopeV1,
    ) -> Result<AppendOutcome> {
        check_priority_envelope(envelope)?;
        self.append_inner(envelope, true, true)
    }

    fn append_inner(
        &self,
        envelope: &CaptureEnvelopeV1,
        priority: bool,
        receipt_timed: bool,
    ) -> Result<AppendOutcome> {
        envelope
            .validate()
            .map_err(|error| CaptureError::Invalid(error.to_string()))?;
        let bytes = serde_json::to_vec(envelope)?;
        let key = envelope_key(envelope);
        let path = self.root.join("outbox").join(format!("{key}.json"));
        let _lock = self.lock()?;
        if path.exists() {
            let existing = read_bounded(&path, self.limits.max_total_bytes)?;
            let same_but_time = receipt_timed
                && without_observation_time(&existing)? == without_observation_time(&bytes)?;
            return if existing == bytes || same_but_time {
                Ok(AppendOutcome::Duplicate)
            } else {
                Err(CaptureError::Conflict(
                    "capture operation id was reused with different bytes".into(),
                ))
            };
        }
        if let Some(compacted) = load_compaction_state(&self.root, self.limits.max_total_bytes)?
            .entries
            .get(&key)
        {
            // Digest-only compaction keeps no bytes to compare, so a receipt-timed re-delivery
            // falls back to its operation id; only acknowledged envelopes are compacted.
            let matches = match (&compacted.envelope, receipt_timed) {
                (Some(retained), true) => {
                    without_observation_time(&serde_json::to_vec(retained)?)?
                        == without_observation_time(&bytes)?
                }
                (None, true) => true,
                (_, false) => compacted.digest == sha256(&bytes),
            };
            return if matches {
                Ok(AppendOutcome::Duplicate)
            } else {
                Err(CaptureError::Conflict(
                    "capture operation id was reused with bytes that differ from compacted history"
                        .into(),
                ))
            };
        }
        self.reserve_envelope(envelope, bytes.len() as u64, priority)?;
        atomic_write(&path, &bytes)?;
        Ok(AppendOutcome::Stored)
    }

    pub fn pending(&self, limit: usize) -> Result<Vec<CaptureEnvelopeV1>> {
        if limit == 0 || limit > MAX_PENDING_PAGE {
            return Err(CaptureError::Invalid(format!(
                "pending limit must be 1..{MAX_PENDING_PAGE}"
            )));
        }
        let _lock = self.lock()?;
        recover_spool_temps(&self.root, self.limits)?;
        recover_compacted_remainders(&self.root, self.limits)?;
        let mut names = regular_file_names(&self.root.join("outbox"), self.limits.max_records)?;
        names.sort();
        let mut values = Vec::new();
        for name in names {
            let key = name
                .strip_suffix(".json")
                .ok_or_else(|| CaptureError::Invalid("invalid outbox filename".into()))?;
            let bytes = read_bounded(
                &self.root.join("outbox").join(&name),
                self.limits.max_total_bytes,
            )?;
            if exact_acknowledgement(&self.root, key, &bytes)? {
                continue;
            }
            let envelope: CaptureEnvelopeV1 = serde_json::from_slice(&bytes)?;
            envelope
                .validate()
                .map_err(|error| CaptureError::Invalid(error.to_string()))?;
            values.push(envelope);
        }
        values.sort_by(|left, right| delivery_key(left).cmp(&delivery_key(right)));
        values.truncate(limit);
        Ok(values)
    }

    /// Returns one bounded delivery page after the last durably attempted envelope. A page wraps
    /// once through the retained causal order and skips acknowledged records. Advancing the cursor
    /// is separate so callers can retain an exact blocked envelope without falsely acknowledging it.
    pub fn delivery_page(&self, limit: usize) -> Result<Vec<CaptureEnvelopeV1>> {
        if limit == 0 || limit > MAX_DELIVERY_PAGE {
            return Err(CaptureError::Invalid(format!(
                "delivery limit must be 1..{MAX_DELIVERY_PAGE}"
            )));
        }
        let _lock = self.lock()?;
        recover_spool_temps(&self.root, self.limits)?;
        let mut retained = retained_envelopes(&self.root, self.limits)?;
        retained.sort_by(|left, right| delivery_key(&left.1).cmp(&delivery_key(&right.1)));
        let start = match load_delivery_cursor(&self.root)? {
            Some(cursor) => retained
                .iter()
                .position(|(key, _)| *key == cursor)
                .ok_or_else(|| {
                    CaptureError::Conflict(
                        "delivery cursor does not name a retained envelope".into(),
                    )
                })?
                .saturating_add(1),
            None => 0,
        };
        let mut page = Vec::new();
        for offset in 0..retained.len() {
            let index = (start + offset) % retained.len().max(1);
            let (key, envelope) = &retained[index];
            let bytes = serde_json::to_vec(envelope)?;
            if exact_acknowledgement(&self.root, key, &bytes)? {
                continue;
            }
            page.push(envelope.clone());
            if page.len() == limit {
                break;
            }
        }
        Ok(page)
    }

    /// Durably records that an exact retained envelope was attempted. This is delivery traversal
    /// state only: it does not acknowledge, rewrite or remove the envelope.
    pub fn advance_delivery_cursor(&self, envelope: &CaptureEnvelopeV1) -> Result<()> {
        envelope
            .validate()
            .map_err(|error| CaptureError::Invalid(error.to_string()))?;
        let bytes = serde_json::to_vec(envelope)?;
        let key = envelope_key(envelope);
        let _lock = self.lock()?;
        let retained = read_bounded(
            &self.root.join("outbox").join(format!("{key}.json")),
            self.limits.max_total_bytes,
        )?;
        if retained != bytes {
            return Err(CaptureError::Conflict(
                "delivery cursor envelope differs from retained bytes".into(),
            ));
        }
        let cursor = serde_json::to_vec(&DeliveryCursorV1 {
            version: 1,
            envelope_key: key,
        })?;
        atomic_replace(&self.root.join("delivery-cursor-v1.json"), &cursor)
    }

    pub fn action_start(&self, operation_id: &str) -> Result<Option<ActionStartV1>> {
        match self.envelope("start", operation_id)? {
            Some(CaptureEnvelopeV1::ActionStart(start)) => Ok(Some(start)),
            Some(_) => Err(CaptureError::Conflict(
                "retained action-start key contains another record variant".into(),
            )),
            None => Ok(None),
        }
    }

    pub fn action_finish(&self, operation_id: &str) -> Result<Option<ActionFinishV1>> {
        match self.envelope("finish", operation_id)? {
            Some(CaptureEnvelopeV1::ActionFinish(finish)) => Ok(Some(finish)),
            Some(_) => Err(CaptureError::Conflict(
                "retained action-finish key contains another record variant".into(),
            )),
            None => Ok(None),
        }
    }

    /// Marks an exact local envelope acknowledged without deleting its retained bytes.
    pub fn acknowledge(&self, envelope: &CaptureEnvelopeV1) -> Result<AppendOutcome> {
        let bytes = serde_json::to_vec(envelope)?;
        let key = envelope_key(envelope);
        let _lock = self.lock()?;
        let retained = match read_bounded(
            &self.root.join("outbox").join(format!("{key}.json")),
            self.limits.max_total_bytes,
        ) {
            Ok(retained) => retained,
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let state = load_compaction_state(&self.root, self.limits.max_total_bytes)?;
                let Some(compacted) = state.entries.get(&key) else {
                    return Err(CaptureError::NotFound(
                        "capture envelope is not retained locally".into(),
                    ));
                };
                return if compacted.digest == sha256(&bytes) {
                    Ok(AppendOutcome::Duplicate)
                } else {
                    Err(CaptureError::Conflict(
                        "acknowledgement differs from compacted envelope history".into(),
                    ))
                };
            }
            Err(error) => return Err(error),
        };
        if retained != bytes {
            return Err(CaptureError::Conflict(
                "acknowledgement differs from the retained envelope".into(),
            ));
        }
        if exact_acknowledgement(&self.root, &key, &bytes)? {
            return Ok(AppendOutcome::Duplicate);
        }
        self.reserve(64, 1)?;
        let marker = self.root.join("acknowledged").join(key);
        atomic_write(&marker, sha256(&bytes).as_bytes())?;
        Ok(AppendOutcome::Stored)
    }

    /// Explicitly compacts acknowledged envelopes. Ordinary event batches retain an exact
    /// idempotency digest; replayable events and completed action pairs retain their full receipts
    /// so following state transitions and action continuity remain checkable. Pending envelopes,
    /// incomplete action starts, artifacts and deferred diagnostics are never removed.
    pub fn compact_acknowledged(&self, limit: usize) -> Result<CompactionReport> {
        if limit == 0 || limit > MAX_COMPACTION_PAGE {
            return Err(CaptureError::Invalid(format!(
                "compaction limit must be 1..{MAX_COMPACTION_PAGE}"
            )));
        }
        let _lock = self.lock()?;
        recover_spool_temps(&self.root, self.limits)?;
        recover_compacted_remainders(&self.root, self.limits)?;
        let acknowledged: BTreeSet<String> =
            regular_file_names(&self.root.join("acknowledged"), self.limits.max_records)?
                .into_iter()
                .collect();
        let retained = retained_envelopes(&self.root, self.limits)?;
        let mut by_key = BTreeMap::new();
        let mut starts = BTreeMap::new();
        let mut finishes = BTreeMap::new();
        for (key, envelope) in retained {
            if !acknowledged.contains(&key) {
                continue;
            }
            let bytes = serde_json::to_vec(&envelope)?;
            if !exact_acknowledgement(&self.root, &key, &bytes)? {
                continue;
            }
            match &envelope {
                CaptureEnvelopeV1::ActionStart(start) => {
                    starts.insert(action_key_start(start), key.clone());
                }
                CaptureEnvelopeV1::ActionFinish(finish) => {
                    finishes.insert(action_key_finish(finish), key.clone());
                }
                CaptureEnvelopeV1::EventBatch(_) | CaptureEnvelopeV1::Usage(_) => {}
            }
            by_key.insert(key, envelope);
        }
        let state_path = self.root.join("compaction-v1.json");
        let old_state_bytes = fs::metadata(&state_path).map_or(0, |metadata| metadata.len());
        let mut state = load_compaction_state(&self.root, self.limits.max_total_bytes)?;
        let mut selected = BTreeSet::new();
        // A crash can occur after the compact index is durable but before both pair files are
        // removed. Exact indexed remnants are independently safe to finish deleting.
        for (key, envelope) in &by_key {
            let Some(compacted) = state.entries.get(key) else {
                continue;
            };
            let bytes = serde_json::to_vec(envelope)?;
            let expected_envelope = retain_compacted_envelope(envelope).then(|| envelope.clone());
            if compacted.digest != sha256(&bytes) || compacted.envelope != expected_envelope {
                return Err(CaptureError::Conflict(
                    "compacted envelope identity conflicts with retained bytes".into(),
                ));
            }
            if selected.len() < limit {
                selected.insert(key.clone());
            }
        }
        for (key, envelope) in &by_key {
            if matches!(
                envelope,
                CaptureEnvelopeV1::EventBatch(_) | CaptureEnvelopeV1::Usage(_)
            ) && selected.len() < limit
            {
                selected.insert(key.clone());
            }
        }
        for (identity, start_key) in starts {
            let Some(finish_key) = finishes.get(&identity) else {
                continue;
            };
            if selected.len().saturating_add(2) > limit {
                break;
            }
            selected.insert(start_key);
            selected.insert(finish_key.clone());
        }
        if selected.is_empty() {
            return Ok(CompactionReport::default());
        }

        let max_entries = self.limits.max_records.saturating_mul(16).min(65_536);
        let new_entries = selected
            .iter()
            .filter(|key| !state.entries.contains_key(*key))
            .count();
        if state.entries.len().saturating_add(new_entries) > max_entries {
            return Err(CaptureError::Limit(
                "local capture compaction index reached its configured bound; export the spool or increase the explicit limit"
                    .into(),
            ));
        }
        let mut removed_bytes = 0_u64;
        let mut removed_observations = BTreeSet::new();
        for key in &selected {
            let envelope = by_key.get(key).ok_or_else(|| {
                CaptureError::Conflict("acknowledged envelope disappeared during compaction".into())
            })?;
            let bytes = serde_json::to_vec(envelope)?;
            let compacted = CompactedEnvelopeV1 {
                digest: sha256(&bytes),
                envelope: retain_compacted_envelope(envelope).then(|| envelope.clone()),
            };
            if let Some(existing) = state.entries.get(key)
                && (existing.digest != compacted.digest || existing.envelope != compacted.envelope)
            {
                return Err(CaptureError::Conflict(
                    "compacted envelope identity conflicts with retained bytes".into(),
                ));
            }
            state.entries.insert(key.clone(), compacted);
            removed_bytes = removed_bytes
                .saturating_add(bytes.len() as u64)
                .saturating_add(64);
            let observed_operation_id = match envelope {
                CaptureEnvelopeV1::ActionFinish(finish) => Some(finish.operation_id.as_str()),
                CaptureEnvelopeV1::EventBatch(batch)
                    if claimed_event_operation(&batch.operation_id) =>
                {
                    Some(batch.operation_id.as_str())
                }
                _ => None,
            };
            if let Some(operation_id) = observed_operation_id {
                let observation_key = sha256(operation_id.as_bytes());
                let observation_path = self.root.join("observations").join(&observation_key);
                match read_bounded(&observation_path, 64) {
                    Ok(fingerprint) => {
                        let fingerprint = String::from_utf8(fingerprint).map_err(|_| {
                            CaptureError::Invalid(
                                "native observation fingerprint is not UTF-8".into(),
                            )
                        })?;
                        if !valid_digest(&fingerprint) {
                            return Err(CaptureError::Invalid(
                                "native observation fingerprint is invalid".into(),
                            ));
                        }
                        if let Some(existing) = state
                            .observations
                            .insert(observation_key.clone(), fingerprint.clone())
                            && existing != fingerprint
                        {
                            return Err(CaptureError::Conflict(
                                "compacted native observation conflicts with retained history"
                                    .into(),
                            ));
                        }
                        removed_observations.insert(observation_path);
                        removed_bytes = removed_bytes.saturating_add(64);
                    }
                    Err(CaptureError::Io(error))
                        if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
        if state.observations.len() > 65_536 {
            return Err(CaptureError::Limit(
                "local capture observation index reached its configured bound; export the spool or increase the explicit limit"
                    .into(),
            ));
        }
        let state_bytes = serde_json::to_vec(&state)?;
        if state_bytes.len() as u64 > self.limits.max_total_bytes / 2 {
            return Err(CaptureError::Limit(
                "local capture compaction index reached its byte bound; export the spool or increase the explicit limit"
                    .into(),
            ));
        }
        let (current_records, current_bytes) = usage(&self.root, self.limits.max_records)?;
        let cursor_path = self.root.join("delivery-cursor-v1.json");
        let cursor_compacted = load_delivery_cursor(&self.root)?
            .is_some_and(|cursor| state.entries.contains_key(&cursor));
        let cursor_bytes = if cursor_compacted {
            fs::metadata(&cursor_path).map_or(0, |metadata| metadata.len())
        } else {
            0
        };
        let removed_records = selected
            .len()
            .saturating_mul(2)
            .saturating_add(removed_observations.len())
            .saturating_add(usize::from(cursor_compacted));
        let post_records = current_records.saturating_sub(removed_records);
        let post_bytes = current_bytes
            .saturating_sub(old_state_bytes)
            .saturating_sub(removed_bytes)
            .saturating_sub(cursor_bytes)
            .saturating_add(state_bytes.len() as u64);
        if post_records > self.limits.max_records || post_bytes > self.limits.max_total_bytes {
            return Err(CaptureError::Limit(
                "compaction selection would still exceed the configured spool limit".into(),
            ));
        }
        atomic_replace(&state_path, &state_bytes)?;
        recover_compacted_remainders(&self.root, self.limits)?;
        Ok(CompactionReport {
            envelopes: selected.len(),
            bytes_reclaimed: removed_bytes
                .saturating_sub((state_bytes.len() as u64).saturating_sub(old_state_bytes)),
        })
    }

    /// Allocates one durable, replay-stable stream sequence. Replaying the same event id returns
    /// its original sequence; a different event consumes the next value.
    pub fn sequence_for(&self, session_id: &str, event_id: &str) -> Result<u64> {
        validate_session_key(session_id, event_id)?;
        let path = self.session_state_path(session_id);
        let _lock = self.lock()?;
        let mut state = load_sequence_state(&path)?;
        if let Some(sequence) = state.events.get(event_id) {
            return Ok(*sequence);
        }
        if state.events.len() >= MAX_SEQUENCE_EVENTS || state.next == 0 {
            return Err(CaptureError::Limit(
                "session sequence map reached its retention bound".into(),
            ));
        }
        let sequence = state.next;
        state.next = state
            .next
            .checked_add(1)
            .ok_or_else(|| CaptureError::Limit("session sequence overflow".into()))?;
        state.events.insert(event_id.to_owned(), sequence);
        save_sequence_state(&path, &state, self)?;
        Ok(sequence)
    }

    /// Allocates a fresh durable identity when the provider supplies no replay-stable event
    /// identity. Every invocation gets a new occurrence; the adapter pairs it with an explicit
    /// identity-uncertainty gap because a provider retry cannot be distinguished from a new event.
    pub fn uncertain_event_id(&self, session_id: &str, fingerprint: &str) -> Result<String> {
        validate_session_key(session_id, fingerprint)?;
        let path = self.session_state_path(session_id);
        let _lock = self.lock()?;
        let mut state = load_sequence_state(&path)?;
        if state.events.len() >= MAX_SEQUENCE_EVENTS || state.next == 0 {
            return Err(CaptureError::Limit(
                "session sequence map reached its retention bound".into(),
            ));
        }
        state.uncertain_occurrences = state
            .uncertain_occurrences
            .checked_add(1)
            .ok_or_else(|| CaptureError::Limit("uncertain occurrence overflow".into()))?;
        let event_id = format!(
            "evt:{}",
            sha256(
                format!(
                    "{session_id}\0uncertain\0{}\0{fingerprint}",
                    state.uncertain_occurrences
                )
                .as_bytes()
            )
        );
        let sequence = state.next;
        state.next = state
            .next
            .checked_add(1)
            .ok_or_else(|| CaptureError::Limit("session sequence overflow".into()))?;
        state.events.insert(event_id.clone(), sequence);
        save_sequence_state(&path, &state, self)?;
        Ok(event_id)
    }

    pub fn session_origin_observed(&self, session_id: &str) -> Result<bool> {
        validate_session_key(session_id, "start")?;
        let path = self.session_state_path(session_id);
        let _lock = self.lock()?;
        Ok(load_sequence_state(&path)?.start_observed)
    }

    pub fn mark_session_origin_observed(&self, session_id: &str) -> Result<()> {
        validate_session_key(session_id, "start")?;
        let path = self.session_state_path(session_id);
        let _lock = self.lock()?;
        let mut state = load_sequence_state(&path)?;
        if state.start_observed {
            return Ok(());
        }
        state.start_observed = true;
        save_sequence_state(&path, &state, self)
    }

    pub fn provider_history_cursor(
        &self,
        session_id: &str,
    ) -> Result<Option<ProviderHistoryCursorV1>> {
        validate_session_key(session_id, "provider-history")?;
        let path = self.session_state_path(session_id);
        let _lock = self.lock()?;
        Ok(load_sequence_state(&path)?.provider_history)
    }

    /// Advances one exact provider-history file only after its complete records were durably
    /// queued. Replacement and truncation remain conflicts rather than being reattributed.
    pub fn advance_provider_history_cursor(
        &self,
        session_id: &str,
        expected: Option<&ProviderHistoryCursorV1>,
        next: ProviderHistoryCursorV1,
    ) -> Result<()> {
        validate_session_key(session_id, "provider-history")?;
        if !valid_provider_history_cursor(&next) {
            return Err(CaptureError::Invalid(
                "provider history cursor is invalid".into(),
            ));
        }
        let path = self.session_state_path(session_id);
        let _lock = self.lock()?;
        let mut state = load_sequence_state(&path)?;
        if state.provider_history.as_ref() != expected {
            if state.provider_history.as_ref() == Some(&next) {
                return Ok(());
            }
            return Err(CaptureError::Conflict(
                "provider history cursor changed concurrently".into(),
            ));
        }
        if let Some(previous) = expected
            && (previous.path_digest != next.path_digest
                || previous.file_device != next.file_device
                || previous.file_inode != next.file_inode
                || previous.work_id != next.work_id
                || previous.generation != next.generation
                || previous.canonical_session_id != next.canonical_session_id
                || next.offset < previous.offset)
        {
            return Err(CaptureError::Conflict(
                "provider history was replaced or truncated".into(),
            ));
        }
        state.provider_history = Some(next);
        save_sequence_state(&path, &state, self)
    }

    /// Rebinds one unchanged provider-history file to a new Work generation at a snapshot
    /// boundary. The caller records the skipped association boundary before this CAS.
    pub(crate) fn rebind_provider_history_cursor(
        &self,
        session_id: &str,
        expected: &ProviderHistoryCursorV1,
        next: ProviderHistoryCursorV1,
    ) -> Result<()> {
        validate_session_key(session_id, "provider-history")?;
        if !valid_provider_history_cursor(&next)
            || expected.canonical_session_id != session_id
            || next.canonical_session_id != session_id
        {
            return Err(CaptureError::Invalid(
                "provider history rebind cursor is invalid".into(),
            ));
        }
        if expected.path_digest != next.path_digest
            || expected.file_device != next.file_device
            || expected.file_inode != next.file_inode
            || next.offset < expected.offset
            || (expected.work_id == next.work_id && expected.generation == next.generation)
        {
            return Err(CaptureError::Conflict(
                "provider history rebind changed file identity, moved backward, or kept the same association"
                    .into(),
            ));
        }
        let path = self.session_state_path(session_id);
        let _lock = self.lock()?;
        let mut state = load_sequence_state(&path)?;
        if state.provider_history.as_ref() != Some(expected) {
            if state.provider_history.as_ref() == Some(&next) {
                return Ok(());
            }
            return Err(CaptureError::Conflict(
                "provider history cursor changed concurrently".into(),
            ));
        }
        state.provider_history = Some(next);
        save_sequence_state(&path, &state, self)
    }

    /// Allocates one spool-local durable order for a stable observed operation. The value records
    /// local causal observation only; it does not compare clocks or authority across spools.
    pub fn observed_order_for(&self, operation_id: &str) -> Result<u64> {
        if !bounded_identifier(operation_id, 255) {
            return Err(CaptureError::Invalid(
                "observed operation id is invalid".into(),
            ));
        }
        let path = self.root.join("order-v1.json");
        let _lock = self.lock()?;
        let mut state = load_observed_order_state(&path, self.limits.max_total_bytes)?;
        if let Some(order) = state.orders.get(operation_id) {
            return Ok(*order);
        }
        let compacted = load_compaction_state(&self.root, self.limits.max_total_bytes)?;
        if let Some(order) = compacted_observed_order(&compacted, operation_id)? {
            return Ok(order);
        }
        if state.orders.len() >= 65_536 || state.next == 0 {
            return Err(CaptureError::Limit(
                "observed action order reached its retention bound".into(),
            ));
        }
        let order = state.next;
        state.next = state
            .next
            .checked_add(1)
            .ok_or_else(|| CaptureError::Limit("observed action order overflow".into()))?;
        state.orders.insert(operation_id.to_owned(), order);
        let bytes = serde_json::to_vec(&state)?;
        if bytes.len() as u64 > self.limits.max_total_bytes / 4 {
            return Err(CaptureError::Limit(
                "observed action order exceeds its byte bound".into(),
            ));
        }
        self.reserve_replace(&path, bytes.len() as u64)?;
        atomic_replace(&path, &bytes)?;
        Ok(order)
    }

    /// Removes an order allocated for an operation that definitively never obtained a durable
    /// action start. Monotonic order values are never reused.
    pub(crate) fn release_observed_order(
        &self,
        operation_id: &str,
        expected_order: u64,
    ) -> Result<()> {
        if !bounded_identifier(operation_id, 255) || expected_order == 0 {
            return Err(CaptureError::Invalid(
                "released observed action order is invalid".into(),
            ));
        }
        let _lock = self.lock()?;
        let start_key = envelope_key_parts("start", operation_id);
        if self
            .root
            .join("outbox")
            .join(format!("{start_key}.json"))
            .exists()
            || load_compaction_state(&self.root, self.limits.max_total_bytes)?
                .entries
                .contains_key(&start_key)
        {
            return Err(CaptureError::Conflict(
                "cannot release an order with a retained action start".into(),
            ));
        }
        let path = self.root.join("order-v1.json");
        let mut state = load_observed_order_state(&path, self.limits.max_total_bytes)?;
        match state.orders.get(operation_id) {
            None => return Ok(()),
            Some(order) if *order == expected_order => {}
            Some(_) => {
                return Err(CaptureError::Conflict(
                    "released observed action order differs from retained order".into(),
                ));
            }
        }
        state.orders.remove(operation_id);
        let bytes = serde_json::to_vec(&state)?;
        self.reserve_replace(&path, bytes.len() as u64)?;
        atomic_replace(&path, &bytes)
    }

    fn session_state_path(&self, session_id: &str) -> PathBuf {
        self.root
            .join("sessions")
            .join(format!("{}.json", sha256(session_id.as_bytes())))
    }

    fn lock(&self) -> Result<SpoolLock> {
        let file = self.acquire_bound_lock("spool.lock", self.identity.spool_lock, false)?;
        Ok(SpoolLock(file))
    }

    fn acquire_bound_lock(
        &self,
        name: &str,
        expected: FileIdentityV1,
        nonblocking: bool,
    ) -> Result<File> {
        let file = {
            let claim = DirectoryClaim::lock(open_private_directory(&self.root)?)?;
            self.validate_identity_claim(&claim.0)?;
            let file = openat_existing_private_file(&claim.0, OsStr::new(name))?;
            if file_identity(&file)? != expected {
                return Err(CaptureError::Conflict(format!(
                    "spool_v4_identity_mismatch: {name} identity changed"
                )));
            }
            drop(claim);
            file
        };
        let operation = if nonblocking {
            libc::LOCK_EX | libc::LOCK_NB
        } else {
            libc::LOCK_EX
        };
        // SAFETY: `file` is owned and remains live on success.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        {
            let claim = DirectoryClaim::lock(open_private_directory(&self.root)?)?;
            self.validate_identity_claim(&claim.0)?;
            let current = openat_existing_private_file(&claim.0, OsStr::new(name))?;
            if file_identity(&file)? != expected || file_identity(&current)? != expected {
                return Err(CaptureError::Conflict(format!(
                    "spool_v4_identity_mismatch: {name} was replaced during acquisition"
                )));
            }
        }
        Ok(file)
    }

    fn validate_identity_claim(&self, directory: &File) -> Result<()> {
        if file_identity(directory)? != self.identity.root
            || path_identity(&self.root)? != self.identity.root
        {
            return Err(CaptureError::Conflict(
                "spool_v4_identity_mismatch: capture spool root identity changed".into(),
            ));
        }
        let bytes = read_bounded_at(
            directory,
            OsStr::new("capture-spool-v4.json"),
            MAX_DEFERRED_BYTES,
        )
        .map_err(|_| {
            CaptureError::Conflict(
                "spool_v4_identity_mismatch: capture spool identity commit is unavailable".into(),
            )
        })?;
        let current: SpoolIdentityV1 = serde_json::from_slice(&bytes).map_err(|_| {
            CaptureError::Conflict(
                "spool_v4_identity_mismatch: capture spool identity commit is invalid".into(),
            )
        })?;
        if current != self.identity {
            return Err(CaptureError::Conflict(
                "spool_v4_identity_mismatch: capture spool identity commit changed".into(),
            ));
        }
        validate_private_authority_layout(directory, true)?;
        Ok(())
    }

    fn envelope(&self, kind: &str, operation_id: &str) -> Result<Option<CaptureEnvelopeV1>> {
        let _lock = self.lock()?;
        retained_envelope(&self.root, self.limits, kind, operation_id)
    }

    fn reserve(&self, additional_bytes: u64, additional_records: usize) -> Result<()> {
        let outstanding = || outstanding_actions(&self.root, self.limits.max_records);
        self.reserve_with_action_capacity(additional_bytes, additional_records, outstanding, 0, 0)
    }

    fn reserve_replace(&self, path: &Path, replacement_bytes: u64) -> Result<()> {
        match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(CaptureError::Invalid(
                    "capture replacement target is not a regular file".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // The old target and the complete sibling temp coexist until rename. Admission therefore
        // reserves the physical peak rather than only the eventual size delta.
        self.reserve(replacement_bytes, 1)
    }

    fn reserve_artifact(&self, additional_bytes: u64, additional_records: usize) -> Result<()> {
        let outstanding = || outstanding_actions(&self.root, self.limits.max_records);
        self.reserve_with_action_capacity(
            additional_bytes,
            additional_records,
            outstanding,
            BASE_METADATA_RESERVE_BYTES.min(self.limits.max_total_bytes / 4),
            BASE_METADATA_RESERVE_RECORDS.min(self.limits.max_records / 4),
        )
    }

    fn reserve_envelope(
        &self,
        envelope: &CaptureEnvelopeV1,
        bytes: u64,
        priority: bool,
    ) -> Result<()> {
        let outstanding = || {
            let mut outstanding = outstanding_action_keys(&self.root, self.limits.max_records)?;
            match envelope {
                CaptureEnvelopeV1::ActionStart(start) => {
                    outstanding.insert(action_key_start(start));
                }
                CaptureEnvelopeV1::ActionFinish(finish) => {
                    outstanding.remove(&action_key_finish(finish));
                }
                CaptureEnvelopeV1::EventBatch(_) | CaptureEnvelopeV1::Usage(_) => {}
            };
            Ok(outstanding.len())
        };
        let reserve_health = !priority && !matches!(envelope, CaptureEnvelopeV1::ActionFinish(_));
        let (base_reserved_bytes, base_reserved_records) = if reserve_health {
            (
                BASE_METADATA_RESERVE_BYTES.min(self.limits.max_total_bytes / 4),
                BASE_METADATA_RESERVE_RECORDS.min(self.limits.max_records / 4),
            )
        } else {
            (0, 0)
        };
        self.reserve_with_action_capacity(
            bytes,
            1,
            outstanding,
            base_reserved_bytes,
            base_reserved_records,
        )
    }

    fn reserve_with_action_capacity(
        &self,
        additional_bytes: u64,
        additional_records: usize,
        outstanding_actions: impl FnOnce() -> Result<usize>,
        base_reserved_bytes: u64,
        base_reserved_records: usize,
    ) -> Result<()> {
        let (records, bytes) = usage(&self.root, self.limits.max_records)?;
        let fits = |outstanding_actions: usize| -> Result<()> {
            let reserved_records = outstanding_actions.saturating_add(base_reserved_records);
            if records
                .saturating_add(additional_records)
                .saturating_add(reserved_records)
                > self.limits.max_records
            {
                return Err(CaptureError::Limit(
                    "local capture spool record limit reached; acknowledge/export or increase the explicit limit"
                        .into(),
                ));
            }
            let reserved_bytes = u64::try_from(outstanding_actions)
                .unwrap_or(u64::MAX)
                .saturating_mul(ACTION_FINISH_RESERVE_BYTES)
                .saturating_add(base_reserved_bytes);
            if bytes
                .saturating_add(additional_bytes)
                .saturating_add(reserved_bytes)
                > self.limits.max_total_bytes
            {
                return Err(CaptureError::Limit(
                    "local capture spool byte limit reached; no retained data was evicted".into(),
                ));
            }
            Ok(())
        };
        // Every outstanding action keeps its start in the outbox, so the outbox file count plus the
        // start being admitted bounds the exact count. Both checks are monotone in that count, so
        // passing with the bound proves the exact check passes without parsing any envelope.
        let bound = regular_file_names(&self.root.join("outbox"), self.limits.max_records)?
            .len()
            .saturating_add(1);
        if fits(bound).is_ok() {
            return Ok(());
        }
        fits(outstanding_actions()?)
    }
}

#[derive(Clone)]
struct InspectedFile {
    relative: PathBuf,
    size: Option<u64>,
    regular: bool,
    digest: SpoolDigestV1,
}

struct InspectionSnapshot {
    directory: File,
    root_identity: FileIdentityV1,
    files: Vec<InspectedFile>,
    bytes: u64,
    issues: Vec<String>,
    issues_omitted: usize,
}

/// Inspects a spool without initializing, recovering, chmodding, or taking an exclusive lock.
/// The report is deliberately non-authoritative: copied directories cannot observe holders of
/// the original lock inodes and retained attempts never prove that a child stopped.
pub fn inspect_spool(root: impl AsRef<Path>) -> Result<SpoolInspectionV1> {
    let root = root.as_ref();
    if !root.is_absolute() {
        return Err(CaptureError::Invalid(
            "capture spool inspection path must be absolute".into(),
        ));
    }
    let snapshot = inspection_snapshot(root)?;
    let mut by_name = BTreeMap::new();
    for file in &snapshot.files {
        by_name.insert(relative_path_bytes(&file.relative), file);
    }
    let format =
        read_snapshot_json::<SpoolIdentityV1>(&snapshot, Path::new("capture-spool-v4.json"));
    let committed_identity = format.as_ref().ok().cloned();
    let classification = if let Some(identity) = committed_identity {
        if validate_format_v4(&identity).is_ok()
            && identity.root == snapshot.root_identity
            && snapshot_identity(&snapshot, Path::new("spool.lock")) == Some(identity.spool_lock)
            && snapshot_identity(&snapshot, Path::new("managed-capture.lock"))
                == Some(identity.managed_lock)
        {
            SpoolClassificationV1::V4IdentityMatch
        } else {
            SpoolClassificationV1::V4IdentityMismatch
        }
    } else if by_name.contains_key(b"managed-occurrence-v3.json".as_slice()) {
        SpoolClassificationV1::V3
    } else if by_name.contains_key(b"managed-occurrence-v2.json".as_slice()) {
        SpoolClassificationV1::V2
    } else if snapshot.files.is_empty() || pristine_partial_v4(&snapshot.files) {
        SpoolClassificationV1::PristinePartialV4
    } else if snapshot
        .files
        .iter()
        .any(|file| evidence_relative_path(&file.relative))
    {
        SpoolClassificationV1::EvidencePartial
    } else {
        SpoolClassificationV1::UnknownNonempty
    };

    let namespace =
        read_snapshot_json::<SpoolNamespaceV1>(&snapshot, Path::new("namespace-v1.json")).ok();
    let mut pending = Vec::new();
    let mut deferred = Vec::new();
    let mut quarantine = Vec::new();
    let mut attempts = Vec::new();
    let mut acknowledged = 0usize;
    let mut live_claims = 0usize;
    let mut temps = 0usize;
    let mut pending_present = 0usize;
    let mut deferred_present = 0usize;
    let mut quarantine_present = 0usize;
    let mut attempt_present = 0usize;
    let mut issues = snapshot.issues.clone();
    let mut issues_omitted = snapshot.issues_omitted;
    for file in &snapshot.files {
        let components: Vec<_> = file.relative.components().collect();
        let first = components.first().and_then(|part| match part {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        });
        let name = file.relative.file_name().and_then(OsStr::to_str);
        if name.is_some_and(|name| name.starts_with(".capture-") && name.ends_with(".tmp")) {
            temps = temps.saturating_add(1);
        }
        if !file.regular {
            continue;
        }
        match first {
            Some("outbox") => {
                let acknowledgement = name
                    .and_then(|name| name.strip_suffix(".json"))
                    .map(|key| Path::new("acknowledged").join(key));
                if let Some(acknowledgement) = acknowledgement
                    && by_name.contains_key(relative_path_bytes(&acknowledgement).as_slice())
                {
                    let exact =
                        read_snapshot_bytes(&snapshot, &file.relative, MAX_INSPECTION_FILE_BYTES)
                            .and_then(|bytes| {
                                read_snapshot_bytes(&snapshot, &acknowledgement, 64)
                                    .map(|marker| marker == sha256(&bytes).as_bytes())
                            })
                            .unwrap_or(false);
                    if exact {
                        continue;
                    }
                    push_issue(
                        &mut issues,
                        &mut issues_omitted,
                        "invalid acknowledgement marker",
                    );
                }
                pending_present = pending_present.saturating_add(1);
                if pending.len() < MAX_INSPECTION_RETURNED {
                    match read_snapshot_json::<CaptureEnvelopeV1>(&snapshot, &file.relative) {
                        Ok(envelope) if envelope.validate().is_ok() => pending.push(envelope),
                        _ => push_issue(
                            &mut issues,
                            &mut issues_omitted,
                            "unreadable pending envelope",
                        ),
                    }
                }
            }
            Some("deferred") => {
                deferred_present = deferred_present.saturating_add(1);
                if deferred.len() < MAX_INSPECTION_RETURNED {
                    match read_snapshot_bytes(&snapshot, &file.relative, MAX_DEFERRED_BYTES)
                        .and_then(|bytes| decode_deferred_record(&bytes))
                    {
                        Ok(record) => deferred.push(record.hook),
                        Err(_) => push_issue(
                            &mut issues,
                            &mut issues_omitted,
                            "unreadable deferred record",
                        ),
                    }
                }
            }
            Some("quarantine") => {
                quarantine_present = quarantine_present.saturating_add(1);
                if quarantine.len() < MAX_INSPECTION_RETURNED {
                    quarantine.push(QuarantineEntryV1 {
                        name: name
                            .map(str::to_owned)
                            .unwrap_or_else(|| relative_path_hex(&file.relative)),
                        size_bytes: file.size.unwrap_or(0),
                    });
                }
            }
            Some("recipe-attempts") => {
                attempt_present = attempt_present.saturating_add(1);
                if attempts.len() < MAX_INSPECTION_RETURNED {
                    match read_snapshot_json::<RecipeAttemptV1>(&snapshot, &file.relative) {
                        Ok(attempt) if validate_recipe_attempt(&attempt).is_ok() => {
                            let phase = match attempt.phase {
                                RecipeAttemptPhaseV1::Reserved => "reserved".to_owned(),
                                RecipeAttemptPhaseV1::Spawned { pid, pgid } => {
                                    format!("spawned:{pid}:{pgid}")
                                }
                            };
                            let (pid, pgid) = match attempt.phase {
                                RecipeAttemptPhaseV1::Reserved => (None, None),
                                RecipeAttemptPhaseV1::Spawned { pid, pgid } => {
                                    (Some(pid), Some(pgid))
                                }
                            };
                            attempts.push(SpoolAttemptInspectionV1 {
                                fence_id: attempt.fence_id,
                                action_id: attempt.action_id,
                                work_id: attempt.work_id,
                                generation: attempt.generation,
                                recipe_id: attempt.recipe_id,
                                root_device: attempt.root_device,
                                root_inode: attempt.root_inode,
                                host_id: attempt.host_id,
                                boot_id: attempt.boot_id,
                                phase,
                                pid,
                                pgid,
                                start_present: None,
                                finish_present: None,
                                resolution: "unknown".into(),
                            });
                        }
                        _ => push_issue(
                            &mut issues,
                            &mut issues_omitted,
                            "unreadable recipe attempt",
                        ),
                    }
                }
            }
            Some("acknowledged") => acknowledged = acknowledged.saturating_add(1),
            Some("managed-invocations") => live_claims = live_claims.saturating_add(1),
            _ => {}
        }
    }
    let probe = probe_snapshot(&snapshot, committed_identity.as_ref());
    let identity_matches = classification == SpoolClassificationV1::V4IdentityMatch;
    Ok(SpoolInspectionV1 {
        version: 1,
        mode: "inspect_only".into(),
        classification,
        observed_root: snapshot.root_identity,
        committed_identity,
        namespace,
        physical_records: snapshot.files.len(),
        physical_bytes: snapshot.bytes,
        entries: snapshot.files.iter().take(MAX_INSPECTION_RETURNED).map(public_entry).collect(),
        entry_count: bounded_count(snapshot.files.len(), MAX_INSPECTION_RETURNED),
        pending,
        pending_count: bounded_count(pending_present, MAX_INSPECTION_RETURNED),
        deferred,
        deferred_count: bounded_count(deferred_present, MAX_INSPECTION_RETURNED),
        quarantine,
        quarantine_count: bounded_count(quarantine_present, MAX_INSPECTION_RETURNED),
        attempts,
        attempt_count: bounded_count(attempt_present, MAX_INSPECTION_RETURNED),
        acknowledged_count: bounded_count(acknowledged, 0),
        live_claim_count: bounded_count(live_claims, 0),
        temp_count: bounded_count(temps, 0),
        probe,
        cannot_observe_original_holders: !identity_matches,
        resumable: false,
        authority: "none".into(),
        completeness: "unknown".into(),
        issues,
        issues_omitted,
        recovery: vec![
            "inspection and export do not authorize replay or prove child termination".into(),
            "M1 does not support switching an existing checkout binding to a different spool identity"
                .into(),
        ],
    })
}

/// Copies a bounded, descriptor-relative byte snapshot to a new private archive directory.
/// The archive is evidence-only and is never resumable execution authority.
pub fn export_spool_archive(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<SpoolExportManifestV1> {
    let source = source.as_ref();
    let destination = destination.as_ref();
    if !destination.is_absolute() || destination.exists() {
        return Err(CaptureError::Invalid(
            "capture export destination must be an absent absolute path".into(),
        ));
    }
    let source_directory = open_private_directory(source)?;
    let source_identity = file_identity(&source_directory)?;
    let destination_parent = destination
        .parent()
        .ok_or_else(|| CaptureError::Invalid("capture export destination has no parent".into()))?
        .canonicalize()?;
    let destination_name = destination
        .file_name()
        .ok_or_else(|| CaptureError::Invalid("capture export destination has no name".into()))?
        .to_owned();
    let destination_parent_directory = open_private_directory(&destination_parent)?;
    let destination_parent_identity = file_identity(&destination_parent_directory)?;
    let mut ancestor = Some(destination_parent.as_path());
    while let Some(path) = ancestor {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_dir()
            && (FileIdentityV1 {
                device: metadata.dev(),
                inode: metadata.ino(),
            }) == source_identity
        {
            return Err(CaptureError::Invalid(
                "capture export destination must not be nested inside the source spool".into(),
            ));
        }
        ancestor = path.parent();
    }
    let before = inspect_spool(source)?;
    let snapshot = inspection_snapshot(source)?;
    if snapshot.bytes > MAX_INSPECTION_TOTAL_BYTES || snapshot.files.len() > MAX_PHYSICAL_RECORDS {
        return Err(CaptureError::Limit(
            "capture export exceeds its bounded archive limit".into(),
        ));
    }
    if snapshot
        .files
        .iter()
        .any(|file| !file.regular || file.size.is_none())
    {
        return Err(CaptureError::Conflict(
            "capture export contains an unreadable or non-regular entry".into(),
        ));
    }
    if snapshot
        .files
        .iter()
        .any(|file| matches!(file.digest, SpoolDigestV1::OversizedUnhashed))
    {
        return Err(CaptureError::Limit(
            "capture export contains an entry above the exact-copy digest bound".into(),
        ));
    }
    if snapshot
        .files
        .iter()
        .any(|file| !matches!(file.digest, SpoolDigestV1::Available(_)))
    {
        return Err(CaptureError::Conflict(
            "capture export contains an entry whose exact bytes could not be hashed".into(),
        ));
    }
    #[cfg(test)]
    if let Some((_, barrier)) = EXPORT_BEFORE_INSTALL_BARRIER
        .lock()
        .unwrap()
        .clone()
        .filter(|(target, _)| target == destination)
    {
        barrier.wait();
        barrier.wait();
    }
    let installation = install_export_destination(
        &destination_parent_directory,
        &destination_name,
        destination,
    )?;
    let destination_directory = &installation.directory;
    #[cfg(test)]
    if let Some((_, barrier)) = EXPORT_AFTER_INSTALL_BARRIER
        .lock()
        .unwrap()
        .clone()
        .filter(|(target, _)| target == destination)
    {
        barrier.wait();
        barrier.wait();
    }
    let export_result = (|| -> Result<SpoolExportManifestV1> {
        let mut exported = Vec::with_capacity(snapshot.files.len());
        let mut directory_identities = BTreeMap::new();
        for file in &snapshot.files {
            let parent = file.relative.parent().unwrap_or_else(|| Path::new(""));
            let output_parent = open_or_create_export_parent(
                destination_directory,
                parent,
                &mut directory_identities,
            )?;
            let target_name = file.relative.file_name().ok_or_else(|| {
                CaptureError::Invalid("capture export entry has no filename".into())
            })?;
            let mut input = open_relative_file(&snapshot.directory, &file.relative)?;
            let mut output = openat_file(
                &output_parent,
                target_name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )?;
            let mut hasher = Sha256::new();
            let mut copied = 0u64;
            let mut buffer = [0u8; 16 * 1024];
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                copied = copied.saturating_add(count as u64);
                if copied > MAX_INSPECTION_TOTAL_BYTES {
                    return Err(CaptureError::Limit(
                        "capture export changed beyond its bounded archive limit".into(),
                    ));
                }
                hasher.update(&buffer[..count]);
                output.write_all(&buffer[..count])?;
            }
            if Some(copied) != file.size {
                return Err(CaptureError::Conflict(
                    "capture export source changed while it was copied".into(),
                ));
            }
            output.sync_all()?;
            output_parent.sync_all()?;
            let copied_digest = format!("{:x}", hasher.finalize());
            if matches!(
                &file.digest,
                SpoolDigestV1::Available(expected) if expected != &copied_digest
            ) {
                return Err(CaptureError::Conflict(
                    "capture export source bytes changed while they were copied".into(),
                ));
            }
            exported.push(SpoolEntryInspectionV1 {
                relative_path: file.relative.to_str().map(str::to_owned),
                relative_path_hex: relative_path_hex(&file.relative),
                size_bytes: Some(copied),
                digest: SpoolDigestV1::Available(copied_digest),
                regular_file: true,
            });
        }
        destination_directory.sync_all()?;
        let after = inspect_spool(source)?;
        let source_in_use = before.probe == SpoolProbeStateV1::Busy
            || after.probe == SpoolProbeStateV1::Busy
            || before.entry_count.present != after.entry_count.present
            || before.physical_bytes != after.physical_bytes;
        let manifest = SpoolExportManifestV1 {
            version: 1,
            source_path: source.to_string_lossy().into_owned(),
            observed_root: snapshot.root_identity,
            classification: before.classification,
            entries: exported,
            entry_count: bounded_count(snapshot.files.len(), snapshot.files.len()),
            attempts: before.attempts.clone(),
            attempt_count: before.attempt_count,
            probe_start: before.probe,
            probe_end: after.probe,
            source_in_use,
            resumable: false,
            authority: "none".into(),
            completeness: "unknown".into(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        let mut manifest_file = openat_file(
            destination_directory,
            OsStr::new("capture-export-manifest-v1.json"),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )?;
        manifest_file.write_all(&manifest_bytes)?;
        manifest_file.sync_all()?;
        destination_directory.sync_all()?;
        Ok(manifest)
    })();
    match export_result {
        Ok(manifest) => {
            if !matches!(
                path_identity(&destination_parent),
                Ok(identity) if identity == destination_parent_identity
            ) {
                return Err(export_failure(
                    &destination_parent_directory,
                    installed_retention_state(&installation),
                    "capture export destination parent identity changed after copying",
                    "completed output location is uncertain",
                ));
            }
            let current = match openat_file(
                &destination_parent_directory,
                &destination_name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            ) {
                Ok(file) => file,
                Err(_) => {
                    return Err(export_failure(
                        &destination_parent_directory,
                        installed_retention_state(&installation),
                        "capture export destination reopen failed after copying",
                        "completed output location is uncertain",
                    ));
                }
            };
            let current_identity = file_identity(&current).map_err(|error| {
                export_failure(
                    &destination_parent_directory,
                    installed_retention_state(&installation),
                    "capture export destination identity read failed after copying",
                    error,
                )
            })?;
            if current_identity != installation.destination.identity {
                return Err(export_failure(
                    &destination_parent_directory,
                    installed_retention_state(&installation),
                    "capture export destination identity changed after copying",
                    format!(
                        "observed {}:{}, expected {}:{}",
                        current_identity.device,
                        current_identity.inode,
                        installation.destination.identity.device,
                        installation.destination.identity.inode
                    ),
                ));
            }
            Ok(manifest)
        }
        Err(error) => Err(export_failure(
            &destination_parent_directory,
            installed_retention_state(&installation),
            "capture export copy or manifest creation failed",
            error,
        )),
    }
}

fn inspection_snapshot(root: &Path) -> Result<InspectionSnapshot> {
    let directory = open_private_directory(root)?;
    let root_identity = file_identity(&directory)?;
    let mut snapshot = InspectionSnapshot {
        directory,
        root_identity,
        files: Vec::new(),
        bytes: 0,
        issues: Vec::new(),
        issues_omitted: 0,
    };
    let mut relative = PathBuf::new();
    scan_inspection_directory(
        &snapshot.directory.try_clone()?,
        &mut relative,
        0,
        &mut snapshot,
    )?;
    snapshot.files.sort_by(|left, right| {
        relative_path_bytes(&left.relative).cmp(&relative_path_bytes(&right.relative))
    });
    Ok(snapshot)
}

fn scan_inspection_directory(
    directory: &File,
    relative: &mut PathBuf,
    depth: usize,
    snapshot: &mut InspectionSnapshot,
) -> Result<()> {
    if depth > MAX_INSPECTION_DEPTH {
        return Err(CaptureError::Limit(
            "capture inspection directory depth limit reached".into(),
        ));
    }
    for name in directory_entry_names(directory)? {
        relative.push(&name);
        let subdirectory = openat_file(
            directory,
            &name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        );
        if let Ok(subdirectory) = subdirectory {
            scan_inspection_directory(&subdirectory, relative, depth + 1, snapshot)?;
            relative.pop();
            continue;
        }
        let opened = openat_file(
            directory,
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        );
        match opened {
            Ok(file) if file.metadata()?.file_type().is_file() => {
                let size = file.metadata()?.len();
                snapshot.bytes = snapshot.bytes.saturating_add(size);
                let digest = if size <= MAX_INSPECTION_FILE_BYTES {
                    match read_bounded_from_file(&file, MAX_INSPECTION_FILE_BYTES) {
                        Ok(bytes) => SpoolDigestV1::Available(sha256(&bytes)),
                        Err(_) => SpoolDigestV1::Unreadable,
                    }
                } else {
                    SpoolDigestV1::OversizedUnhashed
                };
                snapshot.files.push(InspectedFile {
                    relative: relative.clone(),
                    size: Some(size),
                    regular: true,
                    digest,
                });
            }
            _ => {
                snapshot.files.push(InspectedFile {
                    relative: relative.clone(),
                    size: None,
                    regular: false,
                    digest: SpoolDigestV1::Unreadable,
                });
                push_issue(
                    &mut snapshot.issues,
                    &mut snapshot.issues_omitted,
                    "unreadable or non-regular capture entry",
                );
            }
        }
        if snapshot.files.len() > MAX_PHYSICAL_RECORDS
            || snapshot.bytes > MAX_INSPECTION_TOTAL_BYTES
        {
            return Err(CaptureError::Limit(
                "capture inspection exceeds its physical bound".into(),
            ));
        }
        relative.pop();
    }
    Ok(())
}

fn public_entry(file: &InspectedFile) -> SpoolEntryInspectionV1 {
    SpoolEntryInspectionV1 {
        relative_path: file.relative.to_str().map(str::to_owned),
        relative_path_hex: relative_path_hex(&file.relative),
        size_bytes: file.size,
        digest: file.digest.clone(),
        regular_file: file.regular,
    }
}

fn read_snapshot_bytes(
    snapshot: &InspectionSnapshot,
    relative: &Path,
    max: u64,
) -> Result<Vec<u8>> {
    let file = open_relative_file(&snapshot.directory, relative)?;
    read_bounded_from_file(&file, max)
}

fn read_snapshot_json<T: for<'de> Deserialize<'de>>(
    snapshot: &InspectionSnapshot,
    relative: &Path,
) -> Result<T> {
    serde_json::from_slice(&read_snapshot_bytes(
        snapshot,
        relative,
        MAX_INSPECTION_FILE_BYTES,
    )?)
    .map_err(Into::into)
}

fn open_relative_file(root: &File, relative: &Path) -> Result<File> {
    let mut directory = root.try_clone()?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(CaptureError::Invalid(
                "capture relative path is invalid".into(),
            ));
        };
        if components.peek().is_some() {
            directory = openat_file(
                &directory,
                name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )?;
        } else {
            return openat_file(
                &directory,
                name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            );
        }
    }
    Err(CaptureError::Invalid(
        "capture relative path is empty".into(),
    ))
}

fn snapshot_identity(snapshot: &InspectionSnapshot, relative: &Path) -> Option<FileIdentityV1> {
    open_relative_file(&snapshot.directory, relative)
        .and_then(|file| file_identity(&file))
        .ok()
}

fn probe_snapshot(
    snapshot: &InspectionSnapshot,
    committed: Option<&SpoolIdentityV1>,
) -> SpoolProbeStateV1 {
    let Some(committed) = committed else {
        return SpoolProbeStateV1::Unavailable;
    };
    if committed.root != snapshot.root_identity
        || snapshot_identity(snapshot, Path::new("spool.lock")) != Some(committed.spool_lock)
        || snapshot_identity(snapshot, Path::new("managed-capture.lock"))
            != Some(committed.managed_lock)
    {
        return SpoolProbeStateV1::Unavailable;
    }
    for name in ["spool.lock", "managed-capture.lock"] {
        let Ok(file) = open_relative_file(&snapshot.directory, Path::new(name)) else {
            return SpoolProbeStateV1::Unavailable;
        };
        // SAFETY: this is a nonblocking shared directory-local probe. It never mutates source.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == -1 {
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::WouldBlock {
                SpoolProbeStateV1::Busy
            } else {
                SpoolProbeStateV1::Unavailable
            };
        }
    }
    SpoolProbeStateV1::ClearDirectoryLocal
}

fn pristine_partial_v4(files: &[InspectedFile]) -> bool {
    files.iter().all(|file| {
        let path = file.relative.to_string_lossy();
        matches!(
            path.as_ref(),
            "spool.lock"
                | "managed-capture.lock"
                | "compaction-v1.json"
                | "order-v1.json"
                | "observation-scope-v1.json"
        )
    })
}

fn evidence_relative_path(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(std::path::Component::Normal(name))
            if matches!(name.to_str(), Some("outbox" | "acknowledged" | "sessions" | "associations" | "deferred" | "observations" | "managed-invocations" | "recipe-attempts" | "quarantine"))
    ) || path == Path::new("namespace-v1.json")
}

fn relative_path_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for name in path.components().filter_map(|component| match component {
        std::path::Component::Normal(name) => Some(name),
        _ => None,
    }) {
        if !bytes.is_empty() {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(name.as_bytes());
    }
    bytes
}

fn relative_path_hex(path: &Path) -> String {
    relative_path_bytes(path)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn bounded_count(present: usize, returned_limit: usize) -> BoundedCountV1 {
    let returned = present.min(returned_limit);
    BoundedCountV1 {
        present,
        returned,
        omitted: present.saturating_sub(returned),
        truncated: returned < present,
    }
}

fn push_issue(issues: &mut Vec<String>, omitted: &mut usize, issue: &str) {
    if issues.len() < MAX_INSPECTION_RETURNED {
        issues.push(issue.to_owned());
    } else {
        *omitted = omitted.saturating_add(1);
    }
}

fn envelope_key(envelope: &CaptureEnvelopeV1) -> String {
    let (kind, operation) = match envelope {
        CaptureEnvelopeV1::EventBatch(value) => ("event", value.operation_id.as_str()),
        CaptureEnvelopeV1::ActionStart(value) => ("start", value.operation_id.as_str()),
        CaptureEnvelopeV1::ActionFinish(value) => ("finish", value.operation_id.as_str()),
        CaptureEnvelopeV1::Usage(value) => ("usage", value.observation_id.as_str()),
    };
    envelope_key_parts(kind, operation)
}

fn check_priority_envelope(envelope: &CaptureEnvelopeV1) -> Result<()> {
    if matches!(envelope, CaptureEnvelopeV1::EventBatch(batch) if batch.events.iter().all(|event| event.kind == CapturedEventKindV1::Gap))
    {
        Ok(())
    } else {
        Err(CaptureError::Invalid(
            "priority capture append accepts gap events only".into(),
        ))
    }
}

/// Canonical bytes of an event envelope with every observation time zeroed.
fn without_observation_time(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut envelope: CaptureEnvelopeV1 = serde_json::from_slice(bytes)?;
    if let CaptureEnvelopeV1::EventBatch(batch) = &mut envelope {
        for event in &mut batch.events {
            event.observed_at = 0;
        }
    }
    Ok(serde_json::to_vec(&envelope)?)
}

fn envelope_key_parts(kind: &str, operation: &str) -> String {
    sha256(format!("{kind}\0{operation}").as_bytes())
}

fn exact_acknowledgement(root: &Path, key: &str, envelope: &[u8]) -> Result<bool> {
    let marker = match read_bounded(&root.join("acknowledged").join(key), 64) {
        Ok(marker) => marker,
        Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if marker != sha256(envelope).as_bytes() {
        return Err(CaptureError::Conflict(
            "acknowledgement differs from the retained envelope".into(),
        ));
    }
    Ok(true)
}

fn claimed_event_operation(operation_id: &str) -> bool {
    let mut parts = operation_id.split(':');
    matches!(parts.next(), Some("managed" | "native"))
        && matches!(parts.next(), Some("event"))
        && parts.next().is_some_and(valid_digest)
        && matches!(parts.next(), Some("pre" | "post"))
        && parts.next().is_none()
}

fn provider_history_gap_operation(operation_id: &str) -> bool {
    let mut parts = operation_id.split(':');
    matches!(parts.next(), Some("provider"))
        && matches!(parts.next(), Some("history"))
        && parts.next().is_some_and(valid_digest)
        && matches!(parts.next(), Some("gap"))
        && parts.next().is_none()
}

fn replayable_event_operation(operation_id: &str) -> bool {
    claimed_event_operation(operation_id) || provider_history_gap_operation(operation_id)
}

fn retain_compacted_envelope(envelope: &CaptureEnvelopeV1) -> bool {
    !matches!(envelope, CaptureEnvelopeV1::EventBatch(_))
        || matches!(envelope, CaptureEnvelopeV1::EventBatch(batch) if replayable_event_operation(&batch.operation_id))
}

/// Loads one retained envelope while the caller holds the spool lock.
fn retained_envelope(
    root: &Path,
    limits: SpoolLimits,
    kind: &str,
    operation_id: &str,
) -> Result<Option<CaptureEnvelopeV1>> {
    let key = envelope_key_parts(kind, operation_id);
    let path = root.join("outbox").join(format!("{key}.json"));
    let bytes = match read_bounded(&path, limits.max_total_bytes) {
        Ok(bytes) => bytes,
        Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            let state = load_compaction_state(root, limits.max_total_bytes)?;
            return Ok(state
                .entries
                .get(&key)
                .and_then(|compacted| compacted.envelope.clone()));
        }
        Err(error) => return Err(error),
    };
    let envelope: CaptureEnvelopeV1 = serde_json::from_slice(&bytes)?;
    envelope
        .validate()
        .map_err(|error| CaptureError::Invalid(error.to_string()))?;
    Ok(Some(envelope))
}

fn validate_session_key(session_id: &str, value: &str) -> Result<()> {
    if session_id.is_empty() || session_id.len() > 255 || value.is_empty() || value.len() > 255 {
        return Err(CaptureError::Invalid(
            "session and event ids must be 1..255 bytes".into(),
        ));
    }
    Ok(())
}

fn validate_namespace(namespace: &SpoolNamespaceV1) -> Result<()> {
    if namespace.version != 1
        || !bounded_text(&namespace.hub, 2_048)
        || !bounded_text(&namespace.repository, 1_024)
        || !bounded_identifier(&namespace.worktree_id, 255)
    {
        return Err(CaptureError::Invalid(
            "capture spool namespace is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_managed_invocation(marker: &ManagedInvocationV1) -> Result<()> {
    if !matches!(marker.version, 1..=4)
        || !valid_digest(&marker.marker_id)
        || marker
            .operation_prefix
            .as_deref()
            .is_some_and(|value| !bounded_identifier(value, 255))
        || !bounded_identifier(&marker.observed_session_id, 255)
        || !bounded_identifier(&marker.canonical_session_id, 255)
        || !bounded_identifier(&marker.work_id, 255)
        || marker.generation <= 0
        || !bounded_identifier(&marker.tool_use_id, 255)
        || !valid_digest(&marker.command_digest)
        || !bounded_text(&marker.repository, 1_024)
        || !bounded_identifier(&marker.worktree_id, 255)
        || marker
            .base_ref
            .as_deref()
            .is_some_and(|value| !bounded_text(value, 1_024))
        || marker
            .root_path
            .as_deref()
            .is_some_and(|value| !bounded_text(value, 4_096))
        || marker.root_inode == 0
        || marker
            .setup_digest
            .as_deref()
            .is_some_and(|value| !valid_digest(value))
        || marker
            .recipe_digest
            .as_deref()
            .is_some_and(|value| !valid_digest(value))
        || marker
            .parent_session_id
            .as_deref()
            .is_some_and(|id| !bounded_identifier(id, 255))
        || marker.observed_at < 0
        || marker
            .pre_fingerprint
            .as_deref()
            .is_some_and(|value| !valid_digest(value))
        || marker
            .post_fingerprint
            .as_deref()
            .is_some_and(|value| !valid_digest(value))
        || marker.version < 3
            && (marker.pre_fingerprint.is_some() || marker.post_fingerprint.is_some())
        || marker.version == 4 && marker.route.is_none()
        || marker.version < 4 && marker.route.is_some()
        || marker.version == 4
            && (marker.operation_prefix.is_none()
                || marker.root_path.is_none()
                || marker.base_ref.is_none()
                || marker.setup_digest.is_none()
                || marker.recipe_digest.is_none()
                || marker.capture_mode.is_none())
        || matches!(
            marker.route,
            Some(NativeOccurrenceRouteV1::OrdinaryAction { ref result_source, .. })
                if !matches!(result_source.as_str(), "process_exit_code" | "unverified")
        )
    {
        return Err(CaptureError::Invalid(
            "managed invocation marker is invalid".into(),
        ));
    }
    let expected = if marker.version == 1 {
        sha256(
            format!(
                "{}\0{}\0{}\0{}",
                marker.observed_session_id,
                marker.canonical_session_id,
                marker.tool_use_id,
                marker.command_digest
            )
            .as_bytes(),
        )
    } else if marker.version == 2 {
        sha256(
            format!(
                "{}\0{}\0{}",
                marker.observed_session_id, marker.tool_use_id, marker.command_digest
            )
            .as_bytes(),
        )
    } else {
        sha256(format!("{}\0{}", marker.observed_session_id, marker.tool_use_id).as_bytes())
    };
    if marker.marker_id != expected {
        return Err(CaptureError::Invalid(
            "managed invocation marker identity is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_occurrence_receipt(receipt: &NativeOccurrenceReceiptV1) -> bool {
    receipt.version == 1
        && valid_digest(&receipt.marker_id)
        && bounded_identifier(&receipt.observed_session_id, 255)
        && bounded_identifier(&receipt.canonical_session_id, 255)
        && bounded_identifier(&receipt.work_id, 255)
        && receipt.generation > 0
        && bounded_identifier(&receipt.tool_use_id, 255)
        && receipt.pre_fingerprint.as_deref().is_none_or(valid_digest)
        && receipt.post_fingerprint.as_deref().is_none_or(valid_digest)
        && receipt.marker_id
            == sha256(
                format!("{}\0{}", receipt.observed_session_id, receipt.tool_use_id).as_bytes(),
            )
}

fn occurrence_receipt_from_marker(
    marker: &ManagedInvocationV1,
) -> Result<NativeOccurrenceReceiptV1> {
    validate_managed_invocation(marker)?;
    if marker.version != 4 {
        return Err(CaptureError::Conflict(
            "legacy native occurrence claim is not compactable".into(),
        ));
    }
    let receipt = NativeOccurrenceReceiptV1 {
        version: 1,
        marker_id: marker.marker_id.clone(),
        observed_session_id: marker.observed_session_id.clone(),
        canonical_session_id: marker.canonical_session_id.clone(),
        work_id: marker.work_id.clone(),
        generation: marker.generation,
        tool_use_id: marker.tool_use_id.clone(),
        route: marker.route.clone().ok_or_else(|| {
            CaptureError::Conflict("v4 native occurrence has no retained route".into())
        })?,
        pre_fingerprint: marker.pre_fingerprint.clone(),
        post_fingerprint: marker.post_fingerprint.clone(),
    };
    if !validate_occurrence_receipt(&receipt) {
        return Err(CaptureError::Conflict(
            "native occurrence compact receipt is invalid".into(),
        ));
    }
    Ok(receipt)
}

fn cleanup_compacted_occurrence_marker(
    root: &Path,
    receipt: &NativeOccurrenceReceiptV1,
) -> Result<bool> {
    if !validate_occurrence_receipt(receipt) {
        return Err(CaptureError::Conflict(
            "native occurrence compact receipt is invalid".into(),
        ));
    }
    let directory = root.join("managed-invocations");
    let path = directory.join(format!("{}.json", receipt.marker_id));
    let bytes = match read_bounded(&path, MAX_DEFERRED_BYTES) {
        Ok(bytes) => bytes,
        Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let marker: ManagedInvocationV1 = serde_json::from_slice(&bytes)?;
    let retained = occurrence_receipt_from_marker(&marker)?;
    if retained != *receipt {
        return Err(CaptureError::Conflict(
            "compacted native occurrence differs from its live claim remnant".into(),
        ));
    }
    fs::remove_file(path)?;
    File::open(directory)?.sync_all()?;
    Ok(true)
}

fn validate_recipe_fence_binding(binding: &RecipeFenceBindingV1) -> Result<()> {
    if !matches!(binding.version, 1 | 2)
        || !valid_digest(&binding.fence_id)
        || !bounded_text(&binding.repository, 1_024)
        || !bounded_identifier(&binding.worktree_id, 255)
        || !bounded_text(&binding.root_path, 4_096)
        || binding.lock_inode == 0
        || binding
            .work_id
            .as_deref()
            .is_some_and(|value| !bounded_identifier(value, 255))
        || binding.root_inode.is_some_and(|value| value == 0)
        || binding
            .recipe_id
            .as_deref()
            .is_some_and(|value| !bounded_identifier(value, 128))
    {
        return Err(CaptureError::Invalid(
            "configured recipe fence binding is invalid".into(),
        ));
    }
    let expected = if binding.version == 1 {
        let work_id = binding.work_id.as_deref().ok_or_else(|| {
            CaptureError::Invalid("legacy recipe fence is missing its Work identity".into())
        })?;
        binding.root_device.ok_or_else(|| {
            CaptureError::Invalid("legacy recipe fence is missing its root device".into())
        })?;
        binding
            .root_inode
            .filter(|inode| *inode > 0)
            .ok_or_else(|| {
                CaptureError::Invalid("legacy recipe fence is missing its root inode".into())
            })?;
        let recipe_id = binding.recipe_id.as_deref().ok_or_else(|| {
            CaptureError::Invalid("legacy recipe fence is missing its recipe identity".into())
        })?;
        sha256(
            format!(
                "{}\0{}\0{}\0{}\0{}",
                work_id, binding.root_path, binding.repository, binding.worktree_id, recipe_id
            )
            .as_bytes(),
        )
    } else {
        if binding.work_id.is_some()
            || binding.root_device.is_some()
            || binding.root_inode.is_some()
            || binding.recipe_id.is_some()
        {
            return Err(CaptureError::Invalid(
                "checkout recipe fence contains attempt-specific facts".into(),
            ));
        }
        recipe_lock_scope_id(
            &binding.root_path,
            &binding.repository,
            &binding.worktree_id,
        )
    };
    if binding.fence_id != expected {
        return Err(CaptureError::Invalid(
            "configured recipe fence identity is invalid".into(),
        ));
    }
    Ok(())
}

fn recipe_lock_scope_id(root_path: &str, repository: &str, worktree_id: &str) -> String {
    sha256(format!("{root_path}\0{repository}\0{worktree_id}").as_bytes())
}

fn validate_recipe_attempt_reservation(reservation: &RecipeAttemptReservationV1) -> Result<()> {
    if !valid_digest(&reservation.fence_id)
        || !bounded_identifier(&reservation.work_id, 255)
        || reservation.generation <= 0
        || !bounded_identifier(&reservation.observed_session_id, 255)
        || !bounded_identifier(&reservation.canonical_session_id, 255)
        || !bounded_text(&reservation.repository, 1_024)
        || !bounded_identifier(&reservation.worktree_id, 255)
        || !bounded_text(&reservation.root_path, 4_096)
        || reservation.root_inode == 0
        || !bounded_text(&reservation.base_ref, 1_024)
        || !bounded_identifier(&reservation.recipe_id, 128)
        || !valid_digest(&reservation.recipe_digest)
        || !valid_digest(&reservation.setup_digest)
        || !valid_digest(&reservation.command_digest)
        || reservation
            .host_id
            .as_deref()
            .is_some_and(|id| !valid_digest(id))
        || reservation
            .boot_id
            .as_deref()
            .is_some_and(|id| !valid_digest(id))
    {
        return Err(CaptureError::Invalid(
            "configured recipe attempt reservation is invalid".into(),
        ));
    }
    let expected = sha256(
        format!(
            "{}\0{}\0{}\0{}\0{}",
            reservation.work_id,
            reservation.root_path,
            reservation.repository,
            reservation.worktree_id,
            reservation.recipe_id
        )
        .as_bytes(),
    );
    if reservation.fence_id != expected {
        return Err(CaptureError::Invalid(
            "configured recipe fence identity is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_recipe_attempt(attempt: &RecipeAttemptV1) -> Result<()> {
    if attempt.version != 1
        || !valid_digest(&attempt.fence_id)
        || !valid_digest(&attempt.marker_id)
        || !bounded_identifier(&attempt.work_id, 255)
        || attempt.generation <= 0
        || !bounded_identifier(&attempt.observed_session_id, 255)
        || !bounded_identifier(&attempt.canonical_session_id, 255)
        || !bounded_text(&attempt.repository, 1_024)
        || !bounded_identifier(&attempt.worktree_id, 255)
        || !bounded_text(&attempt.root_path, 4_096)
        || attempt.root_inode == 0
        || !bounded_text(&attempt.base_ref, 1_024)
        || !bounded_identifier(&attempt.recipe_id, 128)
        || !valid_digest(&attempt.recipe_digest)
        || !valid_digest(&attempt.setup_digest)
        || attempt
            .host_id
            .as_deref()
            .is_some_and(|id| !valid_digest(id))
        || attempt
            .boot_id
            .as_deref()
            .is_some_and(|id| !valid_digest(id))
        || !bounded_identifier(&attempt.start_operation_id, 255)
        || !bounded_identifier(&attempt.finish_operation_id, 255)
        || !bounded_identifier(&attempt.action_id, 255)
    {
        return Err(CaptureError::Invalid(
            "configured recipe attempt is invalid".into(),
        ));
    }
    if attempt.start_operation_id != format!("recipe:start:{}", attempt.marker_id)
        || attempt.finish_operation_id != format!("recipe:finish:{}", attempt.marker_id)
        || attempt.action_id != format!("act:{}", attempt.marker_id)
        || attempt.fence_id
            != sha256(
                format!(
                    "{}\0{}\0{}\0{}\0{}",
                    attempt.work_id,
                    attempt.root_path,
                    attempt.repository,
                    attempt.worktree_id,
                    attempt.recipe_id
                )
                .as_bytes(),
            )
    {
        return Err(CaptureError::Invalid(
            "configured recipe attempt operation identity is invalid".into(),
        ));
    }
    match attempt.phase {
        RecipeAttemptPhaseV1::Reserved => {}
        RecipeAttemptPhaseV1::Spawned { pid, pgid } if pid != 0 && pgid != 0 => {}
        RecipeAttemptPhaseV1::Spawned { .. } => {
            return Err(CaptureError::Invalid(
                "configured recipe spawned process identity is invalid".into(),
            ));
        }
    }
    Ok(())
}

fn validate_recipe_attempt_binding(
    attempt: &RecipeAttemptV1,
    reservation: &RecipeAttemptReservationV1,
) -> Result<()> {
    if attempt.fence_id != reservation.fence_id
        || attempt.work_id != reservation.work_id
        || attempt.generation != reservation.generation
        || attempt.observed_session_id != reservation.observed_session_id
        || attempt.canonical_session_id != reservation.canonical_session_id
        || attempt.repository != reservation.repository
        || attempt.worktree_id != reservation.worktree_id
        || attempt.root_path != reservation.root_path
        || attempt.root_device != reservation.root_device
        || attempt.root_inode != reservation.root_inode
        || attempt.base_ref != reservation.base_ref
        || attempt.recipe_id != reservation.recipe_id
        || attempt.recipe_digest != reservation.recipe_digest
        || attempt.setup_digest != reservation.setup_digest
        || attempt.coverage != reservation.coverage
        || attempt.host_id != reservation.host_id
    {
        return Err(CaptureError::Conflict(
            "configured recipe binding changed while an attempt is unresolved".into(),
        ));
    }
    Ok(())
}

fn validate_deferred(deferred: &DeferredHookV1) -> Result<()> {
    if !bounded_identifier(&deferred.occurrence_id, 255)
        || deferred
            .reserved_sequence
            .is_some_and(|sequence| sequence == 0)
        || !bounded_identifier(&deferred.session_id, 255)
        || !bounded_identifier(&deferred.hook_event_name, 128)
        || deferred.observed_at < 0
        || !valid_digest(&deferred.input_digest)
        || deferred
            .recipe_digest
            .as_deref()
            .is_some_and(|digest| !valid_digest(digest))
    {
        return Err(CaptureError::Invalid(
            "deferred capture facts are invalid".into(),
        ));
    }
    if (deferred.source.is_some()
        || deferred.recipe_digest.is_some()
        || deferred.action_kind.is_some())
        && deferred.hook_event_name != "PreToolUse"
    {
        return Err(CaptureError::Invalid(
            "deferred action facts must belong to PreToolUse".into(),
        ));
    }
    let is_tool_event = matches!(
        deferred.event_kind,
        Some(
            CapturedEventKindV1::ToolStarted
                | CapturedEventKindV1::ToolFinished
                | CapturedEventKindV1::CheckStarted
                | CapturedEventKindV1::CheckFinished
        )
    );
    if is_tool_event != deferred.tool_kind.is_some() {
        return Err(CaptureError::Invalid(
            "deferred tool event facts are incomplete".into(),
        ));
    }
    Ok(())
}

impl SessionAssociationV1 {
    fn first_capture_mode(&self) -> CaptureModeV1 {
        self.original_capture_mode.unwrap_or(self.capture_mode)
    }
}

/// A session that began unshared never starts sharing: `live` is lowered to `first` otherwise.
fn clamp_shared(first: CaptureModeV1, live: CaptureModeV1) -> CaptureModeV1 {
    if live == CaptureModeV1::SharedConversation && first != CaptureModeV1::SharedConversation {
        first
    } else {
        live
    }
}

fn validate_association(association: &SessionAssociationV1) -> Result<()> {
    if !bounded_identifier(&association.observed_session_id, 255)
        || !bounded_identifier(&association.canonical_session_id, 255)
        || !bounded_identifier(&association.work_id, 255)
        || association.generation < 0
        || association
            .parent_session_id
            .as_deref()
            .is_some_and(|value| !bounded_identifier(value, 255))
    {
        return Err(CaptureError::Invalid(
            "capture session association is invalid".into(),
        ));
    }
    Ok(())
}

fn association_path(root: &Path, observed_session_id: &str) -> PathBuf {
    root.join("associations")
        .join(format!("{}.json", sha256(observed_session_id.as_bytes())))
}

fn load_association(
    root: &Path,
    observed_session_id: &str,
) -> Result<Option<SessionAssociationV1>> {
    let path = association_path(root, observed_session_id);
    let bytes = match read_bounded(&path, MAX_DEFERRED_BYTES) {
        Ok(bytes) => bytes,
        Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let association: SessionAssociationV1 = serde_json::from_slice(&bytes)?;
    validate_association(&association)?;
    if association.observed_session_id != observed_session_id {
        return Err(CaptureError::Conflict(
            "capture association filename does not match its observed session".into(),
        ));
    }
    Ok(Some(association))
}

fn deferred_records(root: &Path, max: usize) -> Result<Vec<DeferredRecordV1>> {
    let mut values = Vec::new();
    let mut orders = BTreeSet::new();
    for name in regular_file_names(&root.join("deferred"), max)? {
        if !name.ends_with(".json") {
            return Err(CaptureError::Invalid(
                "invalid deferred capture filename".into(),
            ));
        }
        let bytes = read_bounded(&root.join("deferred").join(name), MAX_DEFERRED_BYTES)?;
        let value = decode_deferred_record(&bytes)?;
        if value.receipt_order == 0 || !orders.insert(value.receipt_order) {
            return Err(CaptureError::Conflict(
                "deferred receipt order is invalid or duplicated".into(),
            ));
        }
        validate_deferred(&value.hook)?;
        values.push(value);
    }
    Ok(values)
}

fn decode_deferred_record(bytes: &[u8]) -> Result<DeferredRecordV1> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let object = value
        .as_object()
        .ok_or_else(|| CaptureError::Invalid("deferred record must be an object".into()))?;
    let hook = object
        .get("hook")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| CaptureError::Invalid("deferred record hook must be an object".into()))?;
    let has_target = object.contains_key("target");
    let has_reserved_sequence = hook.contains_key("reserved_sequence");
    if has_target != has_reserved_sequence {
        return Err(CaptureError::Invalid(
            "deferred record mixes legacy and current identity fields".into(),
        ));
    }
    let record: DeferredRecordV1 = serde_json::from_value(value)?;
    if has_reserved_sequence {
        if record.hook.reserved_sequence.is_none() {
            return Err(CaptureError::Invalid(
                "current deferred record has no reserved sequence".into(),
            ));
        }
    } else if record.hook.reserved_sequence.is_some() || record.target.is_some() {
        return Err(CaptureError::Invalid(
            "legacy deferred record acquired an association".into(),
        ));
    }
    Ok(record)
}

fn bounded_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn bounded_identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'@')
        })
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn load_sequence_state(path: &Path) -> Result<SequenceState> {
    if path.exists() {
        serde_json::from_slice(&read_bounded(path, MAX_SEQUENCE_BYTES)?).map_err(Into::into)
    } else {
        Ok(SequenceState {
            next: 1,
            ..SequenceState::default()
        })
    }
}

fn valid_provider_history_cursor(cursor: &ProviderHistoryCursorV1) -> bool {
    valid_digest(&cursor.path_digest)
        && cursor.file_device > 0
        && cursor.file_inode > 0
        && bounded_identifier(&cursor.work_id, 64)
        && cursor.work_id.starts_with("wrk_")
        && cursor.generation > 0
        && bounded_identifier(&cursor.canonical_session_id, 255)
}

fn load_observed_order_state(path: &Path, max_bytes: u64) -> Result<ObservedOrderStateV1> {
    let state: ObservedOrderStateV1 = serde_json::from_slice(&read_bounded(path, max_bytes / 4)?)?;
    if !valid_observed_order_state(&state) {
        return Err(CaptureError::Invalid(
            "observed action order state is invalid".into(),
        ));
    }
    Ok(state)
}

fn valid_observed_order_state(state: &ObservedOrderStateV1) -> bool {
    state.version == 1
        && state.next > 0
        && state.orders.len() <= 65_536
        && state
            .orders
            .iter()
            .all(|(key, order)| bounded_identifier(key, 255) && *order > 0 && *order < state.next)
}

fn save_sequence_state(path: &Path, state: &SequenceState, spool: &LocalSpool) -> Result<()> {
    let bytes = serde_json::to_vec(state)?;
    if bytes.len() as u64 > MAX_SEQUENCE_BYTES {
        return Err(CaptureError::Limit(
            "session sequence map exceeds its byte bound".into(),
        ));
    }
    spool.reserve_replace(path, bytes.len() as u64)?;
    atomic_replace(path, &bytes)
}

fn delivery_key(envelope: &CaptureEnvelopeV1) -> (u8, &str, i64, &str, u64, &str) {
    match envelope {
        CaptureEnvelopeV1::ActionStart(value) => (
            0,
            value.work_id.as_str(),
            value.generation,
            value.action_id.as_str(),
            0,
            value.operation_id.as_str(),
        ),
        CaptureEnvelopeV1::EventBatch(value) => (
            1,
            value.work_id.as_str(),
            value.generation,
            value.session.session_id.as_str(),
            value.events.first().map_or(0, |event| event.sequence),
            value.operation_id.as_str(),
        ),
        CaptureEnvelopeV1::ActionFinish(value) => (
            2,
            value.work_id.as_str(),
            value.generation,
            value.action_id.as_str(),
            0,
            value.operation_id.as_str(),
        ),
        CaptureEnvelopeV1::Usage(value) => (
            3,
            value.work_id.as_str(),
            value.generation,
            value.session.as_str(),
            0,
            value.observation_id.as_str(),
        ),
    }
}

fn retained_envelopes(
    root: &Path,
    limits: SpoolLimits,
) -> Result<Vec<(String, CaptureEnvelopeV1)>> {
    let mut values = Vec::new();
    for name in regular_file_names(&root.join("outbox"), limits.max_records)? {
        let key = name
            .strip_suffix(".json")
            .ok_or_else(|| CaptureError::Invalid("invalid outbox filename".into()))?;
        let bytes = read_bounded(&root.join("outbox").join(&name), limits.max_total_bytes)?;
        let envelope: CaptureEnvelopeV1 = serde_json::from_slice(&bytes)?;
        envelope
            .validate()
            .map_err(|error| CaptureError::Invalid(error.to_string()))?;
        if envelope_key(&envelope) != key {
            return Err(CaptureError::Conflict(
                "retained envelope filename does not match its identity".into(),
            ));
        }
        values.push((key.to_owned(), envelope));
    }
    Ok(values)
}

fn load_delivery_cursor(root: &Path) -> Result<Option<String>> {
    let path = root.join("delivery-cursor-v1.json");
    let bytes = match read_bounded(&path, MAX_DEFERRED_BYTES) {
        Ok(bytes) => bytes,
        Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let cursor: DeliveryCursorV1 = serde_json::from_slice(&bytes)?;
    if cursor.version != 1 || !valid_digest(&cursor.envelope_key) {
        return Err(CaptureError::Invalid("delivery cursor is invalid".into()));
    }
    Ok(Some(cursor.envelope_key))
}

fn load_observation_scope(root: &Path) -> Result<ObservationScopeV1> {
    let bytes = read_bounded(&root.join("observation-scope-v1.json"), MAX_DEFERRED_BYTES)?;
    let scope: ObservationScopeV1 = serde_json::from_slice(&bytes)?;
    if scope.version != 1 || !valid_observation_scope_id(&scope.id) {
        return Err(CaptureError::Invalid(
            "capture observation scope is invalid".into(),
        ));
    }
    Ok(scope)
}

fn valid_observation_scope_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn load_compaction_state(root: &Path, max_bytes: u64) -> Result<CompactionStateV1> {
    let path = root.join("compaction-v1.json");
    let bytes = match read_bounded(&path, max_bytes / 2) {
        Ok(bytes) => bytes,
        Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CompactionStateV1 {
                version: 1,
                entries: BTreeMap::new(),
                observations: BTreeMap::new(),
                native_occurrences: BTreeMap::new(),
            });
        }
        Err(error) => return Err(error),
    };
    let state: CompactionStateV1 = serde_json::from_slice(&bytes)?;
    if !valid_compaction_state(&state) {
        return Err(CaptureError::Invalid(
            "capture compaction index is invalid".into(),
        ));
    }
    Ok(state)
}

fn compacted_observed_order(state: &CompactionStateV1, operation_id: &str) -> Result<Option<u64>> {
    let key = envelope_key_parts("start", operation_id);
    let Some(compacted) = state.entries.get(&key) else {
        return Ok(None);
    };
    let Some(CaptureEnvelopeV1::ActionStart(start)) = compacted.envelope.as_ref() else {
        return Err(CaptureError::Conflict(
            "compacted action-start identity has no retained receipt".into(),
        ));
    };
    if start.operation_id != operation_id || start.observed_order == 0 {
        return Err(CaptureError::Conflict(
            "compacted action-start observation order is invalid".into(),
        ));
    }
    Ok(Some(start.observed_order))
}

fn valid_compaction_state(state: &CompactionStateV1) -> bool {
    state.version == 1
        && state.entries.len() <= 65_536
        && state.observations.len() <= 65_536
        && state.native_occurrences.len() <= MAX_SEQUENCE_EVENTS
        && state
            .observations
            .iter()
            .all(|(key, value)| valid_digest(key) && valid_digest(value))
        && state
            .native_occurrences
            .iter()
            .all(|(key, value)| key == &value.marker_id && validate_occurrence_receipt(value))
        && state.entries.iter().all(|(key, value)| {
            valid_digest(key)
                && valid_digest(&value.digest)
                && value.envelope.as_ref().is_none_or(|envelope| {
                    envelope.validate().is_ok()
                        && envelope_key(envelope) == key.as_str()
                        && serde_json::to_vec(envelope)
                            .ok()
                            .is_some_and(|bytes| sha256(&bytes) == value.digest)
                })
        })
}

fn private_directory(path: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(CaptureError::Invalid(format!(
            "{} is not a private directory",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn open_private_directory(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_dir() {
        return Err(CaptureError::Invalid(format!(
            "{} is not a private directory",
            path.display()
        )));
    }
    Ok(file)
}

fn file_identity(file: &File) -> Result<FileIdentityV1> {
    let metadata = file.metadata()?;
    Ok(FileIdentityV1 {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn path_identity(path: &Path) -> Result<FileIdentityV1> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(CaptureError::Conflict(
            "spool_v4_identity_mismatch: capture spool root is no longer a directory".into(),
        ));
    }
    Ok(FileIdentityV1 {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn openat_file(directory: &File, name: &OsStr, flags: i32, mode: libc::mode_t) -> Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| CaptureError::Invalid("capture entry name contains NUL".into()))?;
    // SAFETY: both descriptors/strings are valid for this call; success returns a new owned fd.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags,
            mode as libc::c_uint,
        )
    };
    if fd == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: `openat` returned a new descriptor owned by this function.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn openat_private_regular_read_only(directory: &File, name: &OsStr) -> Result<File> {
    let file = openat_file(
        directory,
        name,
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(CaptureError::Conflict(format!(
            "spool_private_permissions: {} is not a regular private file",
            name.to_string_lossy()
        )));
    }
    validate_private_metadata(
        &metadata,
        &format!("retained metadata {}", name.to_string_lossy()),
    )?;
    Ok(file)
}

fn read_private_bounded_at(directory: &File, name: &OsStr, max: u64) -> Result<Vec<u8>> {
    let file = openat_private_regular_read_only(directory, name)?;
    let initial_len = file.metadata()?.len();
    if initial_len > max {
        return Err(CaptureError::Limit(
            "capture entry exceeds its read bound".into(),
        ));
    }
    let read_limit = max
        .checked_add(1)
        .ok_or_else(|| CaptureError::Limit("capture read bound overflowed".into()))?;
    let mut bytes = Vec::with_capacity(initial_len as usize);
    (&file).take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(CaptureError::Limit(
            "capture entry exceeds its read bound".into(),
        ));
    }
    Ok(bytes)
}

fn mkdirat_private(directory: &File, name: &OsStr) -> Result<()> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| CaptureError::Invalid("capture directory name contains NUL".into()))?;
    // SAFETY: the directory descriptor and NUL-terminated name are valid for this call.
    if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn renameat_noreplace(
    source_directory: &File,
    from: &OsStr,
    destination_directory: &File,
    to: &OsStr,
) -> Result<()> {
    let from = CString::new(from.as_bytes())
        .map_err(|_| CaptureError::Invalid("capture export staging name contains NUL".into()))?;
    let to = CString::new(to.as_bytes()).map_err(|_| {
        CaptureError::Invalid("capture export destination name contains NUL".into())
    })?;
    // SAFETY: both names are relative to the retained directory descriptor and remain valid for
    // the duration of the call. RENAME_EXCL refuses an existing destination atomically.
    let result = unsafe {
        libc::renameatx_np(
            source_directory.as_raw_fd(),
            from.as_ptr(),
            destination_directory.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn renameat_noreplace(
    source_directory: &File,
    from: &OsStr,
    destination_directory: &File,
    to: &OsStr,
) -> Result<()> {
    let from = CString::new(from.as_bytes())
        .map_err(|_| CaptureError::Invalid("capture export staging name contains NUL".into()))?;
    let to = CString::new(to.as_bytes()).map_err(|_| {
        CaptureError::Invalid("capture export destination name contains NUL".into())
    })?;
    // SAFETY: both names are relative to the retained directory descriptor and remain valid for
    // the duration of the call. RENAME_NOREPLACE refuses an existing destination atomically.
    // The raw syscall, because the musl that static release builds link has no renameat2 wrapper.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_directory.as_raw_fd(),
            from.as_ptr(),
            destination_directory.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android"
)))]
fn renameat_noreplace(
    _source_directory: &File,
    _from: &OsStr,
    _destination_directory: &File,
    _to: &OsStr,
) -> Result<()> {
    Err(CaptureError::Invalid(
        "capture export requires atomic no-replace directory installation on this platform".into(),
    ))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
unsafe extern "C" {
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_set_fd_np(fd: libc::c_int, acl: *mut libc::c_void, acl_type: libc::c_int)
    -> libc::c_int;
    fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut libc::c_void;
    fn acl_get_entry(
        acl: *mut libc::c_void,
        entry_id: libc::c_int,
        entry: *mut *mut libc::c_void,
    ) -> libc::c_int;
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn verify_export_acl_empty(directory: &File) -> Result<()> {
    // SAFETY: the descriptor is open, the ACL type is defined by Darwin, and a successful call
    // returns independently allocated storage released below with acl_free.
    let observed = unsafe { acl_get_fd_np(directory.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if observed.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(CaptureError::Io(error));
    }
    let mut entry = std::ptr::null_mut();
    // SAFETY: observed is a live ACL allocation and entry points to writable pointer storage.
    let entry_result = unsafe { acl_get_entry(observed, 0, &mut entry) };
    let entry_error = std::io::Error::last_os_error();
    // SAFETY: observed was returned by acl_get_fd_np and has not yet been released.
    let free_result = unsafe { acl_free(observed) };
    if free_result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    if entry_result == 0 {
        return Err(CaptureError::Conflict(
            "capture export anchor retained an extended ACL entry after clearing".into(),
        ));
    }
    Err(CaptureError::Io(entry_error))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn clear_and_verify_export_acl(directory: &File) -> Result<()> {
    #[cfg(test)]
    if FAIL_EXPORT_ACL_CLEAR.with(std::cell::Cell::get) {
        return Err(CaptureError::Io(std::io::Error::other(
            "injected export ACL clear failure",
        )));
    }
    // SAFETY: acl_init allocates an empty ACL owned by this function on success.
    let empty = unsafe { acl_init(0) };
    if empty.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: the descriptor and ACL allocation are live and ACL_TYPE_EXTENDED is a Darwin type.
    let set_result = unsafe { acl_set_fd_np(directory.as_raw_fd(), empty, ACL_TYPE_EXTENDED) };
    let set_error = std::io::Error::last_os_error();
    // SAFETY: empty was returned by acl_init and has not yet been released.
    let free_result = unsafe { acl_free(empty) };
    if set_result == -1 {
        return Err(CaptureError::Io(set_error));
    }
    if free_result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    verify_export_acl_empty(directory)
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn clear_and_verify_export_acl(_directory: &File) -> Result<()> {
    Ok(())
}

#[derive(Clone, Debug)]
struct ExportNamedIdentity {
    name: OsString,
    identity: FileIdentityV1,
}

struct InstalledExportDestination {
    directory: File,
    anchor: ExportNamedIdentity,
    destination: ExportNamedIdentity,
}

enum ExportRetentionState<'a> {
    AnchorCreated {
        name: &'a OsStr,
    },
    AnchorOpenedUnverified {
        anchor: &'a ExportNamedIdentity,
    },
    AnchorAclClearedUnscanned {
        anchor: &'a ExportNamedIdentity,
    },
    AnchorVerifiedEmpty {
        anchor: &'a ExportNamedIdentity,
    },
    ChildCreated {
        anchor: &'a ExportNamedIdentity,
        child_identity: Option<FileIdentityV1>,
    },
    Installed {
        anchor: &'a ExportNamedIdentity,
        destination: &'a ExportNamedIdentity,
    },
}

fn installed_retention_state(
    installation: &InstalledExportDestination,
) -> ExportRetentionState<'_> {
    ExportRetentionState::Installed {
        anchor: &installation.anchor,
        destination: &installation.destination,
    }
}

fn retained_named_identity(parent: &File, label: &str, retained: &ExportNamedIdentity) -> String {
    let still_named = openat_file(
        parent,
        &retained.name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .and_then(|current| file_identity(&current))
    .is_ok_and(|identity| identity == retained.identity);
    if still_named {
        format!(
            "{label} name {} currently resolves to identity {}:{}",
            retained.name.to_string_lossy(),
            retained.identity.device,
            retained.identity.inode
        )
    } else {
        format!(
            "{label} identity {}:{} is not currently observed at expected name {}; its current location and existence are unknown and it may have been moved or removed; the current {} entry was not modified",
            retained.identity.device,
            retained.identity.inode,
            retained.name.to_string_lossy(),
            retained.name.to_string_lossy()
        )
    }
}

fn export_retention_description(parent: &File, state: ExportRetentionState<'_>) -> String {
    match state {
        ExportRetentionState::AnchorCreated { name } => format!(
            "an export anchor was created under generated name {}; parent durability, current identity/location, privacy, and contents are uncertain; no pathname cleanup was attempted",
            name.to_string_lossy()
        ),
        ExportRetentionState::AnchorOpenedUnverified { anchor } => format!(
            "{}; privacy and contents are unverified",
            retained_named_identity(parent, "opened export anchor", anchor)
        ),
        ExportRetentionState::AnchorAclClearedUnscanned { anchor } => format!(
            "{}; its extended ACL was cleared but its contents are unverified",
            retained_named_identity(parent, "export anchor", anchor)
        ),
        ExportRetentionState::AnchorVerifiedEmpty { anchor } => format!(
            "{}; it was verified private and empty before child creation",
            retained_named_identity(parent, "export anchor", anchor)
        ),
        ExportRetentionState::ChildCreated {
            anchor,
            child_identity,
        } => {
            let child = child_identity.map_or_else(
                || "owned child tree owned-output has unknown identity".to_owned(),
                |identity| {
                    format!(
                        "owned child tree owned-output has identity {}:{}",
                        identity.device, identity.inode
                    )
                },
            );
            format!(
                "{}; {child} and may contain partial archive entries",
                retained_named_identity(parent, "export anchor", anchor)
            )
        }
        ExportRetentionState::Installed {
            anchor,
            destination,
        } => format!(
            "{}; {}; either tree may contain partial archive entries",
            retained_named_identity(parent, "owned export destination", destination),
            retained_named_identity(parent, "export anchor", anchor)
        ),
    }
}

fn export_failure(
    parent: &File,
    state: ExportRetentionState<'_>,
    context: &str,
    error: impl std::fmt::Display,
) -> CaptureError {
    CaptureError::Conflict(format!(
        "{context} ({error}); {}",
        export_retention_description(parent, state)
    ))
}

fn install_export_destination(
    parent: &File,
    destination_name: &OsStr,
    destination: &Path,
) -> Result<InstalledExportDestination> {
    let mut anchor_name = None;
    for _ in 0..MAX_EXPORT_ANCHOR_ATTEMPTS {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| {
            CaptureError::Invalid("capture export anchor randomness unavailable".into())
        })?;
        let candidate = OsString::from(format!(
            ".agentscm-export-anchor-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        match mkdirat_private(parent, &candidate) {
            Ok(()) => {
                anchor_name = Some(candidate);
                break;
            }
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let anchor_name = anchor_name.ok_or_else(|| {
        CaptureError::Conflict(
            "capture export could not reserve a private random anchor within its bounded retry limit"
                .into(),
        )
    })?;
    #[cfg(test)]
    if let Some((_, barrier)) = EXPORT_AFTER_ANCHOR_CREATE_BARRIER
        .lock()
        .unwrap()
        .clone()
        .filter(|(target, _)| target == destination)
    {
        barrier.wait();
        barrier.wait();
    }
    sync_export_directory(
        parent,
        destination,
        "destination parent after anchor creation",
    )
    .map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::AnchorCreated { name: &anchor_name },
            "capture export anchor parent sync failed",
            error,
        )
    })?;
    let anchor_directory = openat_file(
        parent,
        &anchor_name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::AnchorCreated { name: &anchor_name },
            "capture export anchor open failed",
            error,
        )
    })?;
    let anchor_identity = file_identity(&anchor_directory).map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::AnchorCreated { name: &anchor_name },
            "capture export anchor identity read failed",
            error,
        )
    })?;
    let anchor = ExportNamedIdentity {
        name: anchor_name,
        identity: anchor_identity,
    };
    let anchor_metadata = anchor_directory
        .metadata()
        .map_err(CaptureError::from)
        .map_err(|error| {
            export_failure(
                parent,
                ExportRetentionState::AnchorOpenedUnverified { anchor: &anchor },
                "capture export anchor metadata read failed",
                error,
            )
        })?;
    validate_private_metadata(&anchor_metadata, "capture export anchor").map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::AnchorOpenedUnverified { anchor: &anchor },
            "capture export anchor private-mode validation failed",
            error,
        )
    })?;
    clear_and_verify_export_acl(&anchor_directory).map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::AnchorOpenedUnverified { anchor: &anchor },
            "capture export anchor ACL clear or verification failed",
            error,
        )
    })?;
    let unexpected_entries = directory_entry_names(&anchor_directory).map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::AnchorAclClearedUnscanned { anchor: &anchor },
            "capture export anchor emptiness scan failed",
            error,
        )
    })?;
    if !unexpected_entries.is_empty() {
        return Err(export_failure(
            parent,
            ExportRetentionState::AnchorAclClearedUnscanned { anchor: &anchor },
            "capture export anchor admission failed",
            format!(
                "{} unexpected entr{} retained untouched",
                unexpected_entries.len(),
                if unexpected_entries.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            ),
        ));
    }

    let child_name = OsStr::new("owned-output");
    mkdirat_private(&anchor_directory, child_name).map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::AnchorVerifiedEmpty { anchor: &anchor },
            "capture export child creation failed",
            error,
        )
    })?;
    let child_directory = openat_file(
        &anchor_directory,
        child_name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::ChildCreated {
                anchor: &anchor,
                child_identity: None,
            },
            "capture export child open failed",
            error,
        )
    })?;
    let child_identity = file_identity(&child_directory).map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::ChildCreated {
                anchor: &anchor,
                child_identity: None,
            },
            "capture export child identity read failed",
            error,
        )
    })?;
    let child_metadata = child_directory
        .metadata()
        .map_err(CaptureError::from)
        .map_err(|error| {
            export_failure(
                parent,
                ExportRetentionState::ChildCreated {
                    anchor: &anchor,
                    child_identity: Some(child_identity),
                },
                "capture export child metadata read failed",
                error,
            )
        })?;
    validate_private_metadata(&child_metadata, "capture export output").map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::ChildCreated {
                anchor: &anchor,
                child_identity: Some(child_identity),
            },
            "capture export child private-mode validation failed",
            error,
        )
    })?;
    #[cfg(test)]
    if let Some((_, barrier)) = EXPORT_AFTER_CHILD_OPEN_BARRIER
        .lock()
        .unwrap()
        .clone()
        .filter(|(target, _)| target == destination)
    {
        barrier.wait();
        barrier.wait();
    }
    sync_export_directory(
        &anchor_directory,
        destination,
        "export anchor after output creation",
    )
    .map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::ChildCreated {
                anchor: &anchor,
                child_identity: Some(child_identity),
            },
            "capture export anchor sync after child creation failed",
            error,
        )
    })?;
    if let Err(error) = renameat_noreplace(&anchor_directory, child_name, parent, destination_name)
    {
        return Err(export_failure(
            parent,
            ExportRetentionState::ChildCreated {
                anchor: &anchor,
                child_identity: Some(child_identity),
            },
            "capture export destination installation failed",
            error,
        ));
    }
    let destination_path = destination;
    let destination = ExportNamedIdentity {
        name: destination_name.to_owned(),
        identity: child_identity,
    };
    sync_export_directory(
        &anchor_directory,
        destination_path,
        "export anchor after destination installation",
    )
    .map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::Installed {
                anchor: &anchor,
                destination: &destination,
            },
            "capture export source-name removal is crash-uncertain",
            error,
        )
    })?;
    sync_export_directory(
        parent,
        destination_path,
        "destination parent after destination installation",
    )
    .map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::Installed {
                anchor: &anchor,
                destination: &destination,
            },
            "capture export destination name is crash-uncertain",
            error,
        )
    })?;
    #[cfg(test)]
    if let Some((_, barrier)) = EXPORT_BEFORE_DESTINATION_REOPEN_BARRIER
        .lock()
        .unwrap()
        .clone()
        .filter(|(target, _)| target == destination_path)
    {
        barrier.wait();
        barrier.wait();
    }
    let current = openat_file(
        parent,
        destination_name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::Installed {
                anchor: &anchor,
                destination: &destination,
            },
            "capture export destination reopen failed",
            error,
        )
    })?;
    let current_identity = file_identity(&current).map_err(|error| {
        export_failure(
            parent,
            ExportRetentionState::Installed {
                anchor: &anchor,
                destination: &destination,
            },
            "capture export destination identity read failed",
            error,
        )
    })?;
    if current_identity != destination.identity {
        return Err(export_failure(
            parent,
            ExportRetentionState::Installed {
                anchor: &anchor,
                destination: &destination,
            },
            "capture export destination identity changed during exclusive installation",
            format!(
                "observed {}:{}, expected {}:{}",
                current_identity.device,
                current_identity.inode,
                destination.identity.device,
                destination.identity.inode
            ),
        ));
    }
    Ok(InstalledExportDestination {
        directory: child_directory,
        anchor,
        destination,
    })
}

fn sync_export_directory(
    directory: &File,
    _destination: &Path,
    _stage: &'static str,
) -> Result<()> {
    #[cfg(test)]
    if FAIL_EXPORT_SYNC_FOR
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|(target, stage)| target == _destination && *stage == _stage)
    {
        return Err(CaptureError::Io(std::io::Error::other(format!(
            "injected export sync failure at {_stage}"
        ))));
    }
    directory.sync_all().map_err(Into::into)
}

fn open_or_create_export_parent(
    root: &File,
    relative: &Path,
    identities: &mut BTreeMap<PathBuf, FileIdentityV1>,
) -> Result<File> {
    let mut current = root.try_clone()?;
    let mut accumulated = PathBuf::new();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(CaptureError::Invalid(
                "capture export entry has an unsafe relative parent".into(),
            ));
        };
        accumulated.push(name);
        if !identities.contains_key(&accumulated) {
            mkdirat_private(&current, name).map_err(|error| {
                CaptureError::Conflict(format!(
                    "capture export could not exclusively create {} ({error}); partial output retained",
                    accumulated.display()
                ))
            })?;
            current.sync_all()?;
        }
        let next = openat_file(
            &current,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
        let identity = file_identity(&next)?;
        match identities.get(&accumulated) {
            Some(expected) if *expected != identity => {
                return Err(CaptureError::Conflict(format!(
                    "capture export directory {} was replaced; partial output retained",
                    accumulated.display()
                )));
            }
            Some(_) => {}
            None => {
                identities.insert(accumulated.clone(), identity);
            }
        }
        current = next;
    }
    Ok(current)
}

fn openat_existing_private_file(directory: &File, name: &OsStr) -> Result<File> {
    let file = openat_file(
        directory,
        name,
        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(CaptureError::Conflict(format!(
            "spool_private_permissions: {} is not a regular private file",
            name.to_string_lossy()
        )));
    }
    validate_private_metadata(
        &metadata,
        &format!("retained metadata {}", name.to_string_lossy()),
    )?;
    Ok(file)
}

fn read_bounded_from_file(mut file: &File, max: u64) -> Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(CaptureError::Invalid(
            "capture entry is not a regular file".into(),
        ));
    }
    if metadata.len() > max {
        return Err(CaptureError::Limit(
            "capture entry exceeds its read bound".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_bounded_at(directory: &File, name: &OsStr, max: u64) -> Result<Vec<u8>> {
    let file = openat_file(
        directory,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    read_bounded_from_file(&file, max)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn directory_errno_location() -> *mut libc::c_int {
    // SAFETY: Darwin exposes the calling thread's errno slot through `__error`.
    unsafe { libc::__error() }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn directory_errno_location() -> *mut libc::c_int {
    // SAFETY: Linux exposes the calling thread's errno slot through `__errno_location`.
    unsafe { libc::__errno_location() }
}

fn directory_entry_names(directory: &File) -> Result<Vec<OsString>> {
    #[cfg(test)]
    let directory_identity = file_identity(directory)?;
    // `dup` would share the directory-stream offset with the retained descriptor. Reopen `.`
    // relative to that descriptor so every bounded scan starts from an independent open-file
    // description without returning to the mutable pathname.
    let reopened = openat_file(
        directory,
        OsStr::new("."),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    let reopened = reopened.into_raw_fd();
    // SAFETY: `reopened` is an owned directory descriptor.
    let stream = unsafe { libc::fdopendir(reopened) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        // SAFETY: fdopendir did not take ownership on failure.
        unsafe { libc::close(reopened) };
        return Err(error.into());
    }
    let mut names = Vec::new();
    let mut read_error = None;
    loop {
        // POSIX uses a null result for both end-of-directory and an error. Clear the calling
        // thread's errno slot before every call so only zero means EOF.
        // SAFETY: the platform helper returns this thread's live errno slot.
        unsafe { *directory_errno_location() = 0 };
        #[cfg(test)]
        let inject_read_error = FAIL_EXPORT_READDIR_FOR
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(identity, after_entries)| {
                *identity == directory_identity && *after_entries == names.len()
            });
        #[cfg(test)]
        if inject_read_error {
            // SAFETY: set the calling thread's errno exactly as a failing readdir would.
            unsafe { *directory_errno_location() = libc::EIO };
        }
        // SAFETY: `stream` remains live until the final closedir.
        #[cfg(test)]
        let entry = if inject_read_error {
            std::ptr::null_mut()
        } else {
            unsafe { libc::readdir(stream) }
        };
        #[cfg(not(test))]
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            // Preserve the read error before `closedir` can change errno.
            // SAFETY: the platform helper returns this thread's live errno slot.
            let errno = unsafe { *directory_errno_location() };
            if errno != 0 {
                read_error = Some(std::io::Error::from_raw_os_error(errno));
            }
            break;
        }
        // SAFETY: d_name is NUL terminated for a valid dirent.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(OsString::from_vec(bytes.to_vec()));
        if names.len() > MAX_PHYSICAL_RECORDS {
            // SAFETY: `stream` remains live.
            unsafe { libc::closedir(stream) };
            return Err(CaptureError::Limit(
                "capture directory contains too many entries".into(),
            ));
        }
    }
    // SAFETY: `stream` is live and closed exactly once.
    let close_failed = unsafe { libc::closedir(stream) } == -1;
    if let Some(error) = read_error {
        return Err(error.into());
    }
    if close_failed {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(names)
}

fn read_only_directory_entry_names(
    directory: &File,
    max_entries: usize,
    deadline: Instant,
) -> Result<Vec<OsString>> {
    let reopened = openat_file(
        directory,
        OsStr::new("."),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    let reopened = reopened.into_raw_fd();
    // SAFETY: `reopened` is an owned directory descriptor.
    let stream = unsafe { libc::fdopendir(reopened) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        // SAFETY: fdopendir did not take ownership on failure.
        unsafe { libc::close(reopened) };
        return Err(error.into());
    }
    let mut names = Vec::new();
    let result = loop {
        if Instant::now() >= deadline {
            break Err(CaptureError::Limit(
                "capture evidence projection exceeded its time bound".into(),
            ));
        }
        // SAFETY: the platform helper returns this thread's live errno slot.
        unsafe { *directory_errno_location() = 0 };
        // SAFETY: `stream` remains live until the final `closedir`.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            // SAFETY: the platform helper returns this thread's live errno slot.
            let errno = unsafe { *directory_errno_location() };
            if errno == 0 {
                break Ok(());
            }
            break Err(CaptureError::Io(std::io::Error::from_raw_os_error(errno)));
        }
        // SAFETY: d_name is NUL terminated for a valid dirent.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(OsString::from_vec(bytes.to_vec()));
        if names.len() > max_entries {
            break Err(CaptureError::Limit(
                "capture evidence projection contains too many retained records".into(),
            ));
        }
    };
    // SAFETY: `stream` is live and closed exactly once.
    let close_failed = unsafe { libc::closedir(stream) } == -1;
    result?;
    if close_failed {
        return Err(std::io::Error::last_os_error().into());
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

fn check_read_only_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(CaptureError::Limit(
            "capture evidence projection exceeded its time bound".into(),
        ));
    }
    Ok(())
}

fn validate_format_v4(format: &SpoolIdentityV1) -> Result<()> {
    if format.version != 4
        || format.root.inode == 0
        || format.spool_lock.inode == 0
        || format.managed_lock.inode == 0
    {
        return Err(CaptureError::Conflict(
            "spool_legacy_format: capture spool v4 identity commit is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_pristine_v4_layout(directory: &File) -> Result<()> {
    let expected: BTreeSet<Vec<u8>> = SPOOL_DIRECTORIES
        .iter()
        .map(|name| name.as_bytes().to_vec())
        .chain(
            [
                "spool.lock",
                "managed-capture.lock",
                "compaction-v1.json",
                "order-v1.json",
                "observation-scope-v1.json",
            ]
            .into_iter()
            .map(|name| name.as_bytes().to_vec()),
        )
        .collect();
    let actual: BTreeSet<Vec<u8>> = directory_entry_names(directory)?
        .into_iter()
        .map(|name| name.as_bytes().to_vec())
        .collect();
    if actual != expected {
        return Err(CaptureError::Conflict(
            "spool_legacy_format: capture spool changed during v4 initialization".into(),
        ));
    }
    for name in SPOOL_DIRECTORIES {
        let subdirectory = openat_file(
            directory,
            OsStr::new(name),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
        if !directory_entry_names(&subdirectory)?.is_empty() {
            return Err(CaptureError::Conflict(
                "spool_legacy_format: capture evidence appeared during v4 initialization".into(),
            ));
        }
    }
    Ok(())
}

fn validate_private_metadata(metadata: &fs::Metadata, label: &str) -> Result<()> {
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid || metadata.mode() & 0o077 != 0 {
        return Err(CaptureError::Conflict(format!(
            "spool_private_permissions: {label} must be owned by the effective user and inaccessible to group/other"
        )));
    }
    Ok(())
}

/// Validates authority-bearing ownership and modes through the retained root descriptor. Protected
/// operations use the full bounded scan so unsafe retained evidence is never interpreted, mutated,
/// recovered, or delivered.
fn validate_private_authority_layout(directory: &File, scan_files: bool) -> Result<()> {
    validate_private_metadata(&directory.metadata()?, "capture spool root")?;
    let mut scanned = 0usize;
    for name in SPOOL_DIRECTORIES {
        let child = openat_file(
            directory,
            OsStr::new(name),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|_| {
            CaptureError::Conflict(format!(
                "spool_private_permissions: capture directory {name} is unavailable"
            ))
        })?;
        validate_private_metadata(&child.metadata()?, &format!("capture directory {name}"))?;
        if !scan_files {
            continue;
        }
        for entry_name in directory_entry_names(&child)? {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_PHYSICAL_RECORDS {
                return Err(CaptureError::Limit(
                    "capture spool privacy validation contains too many physical records".into(),
                ));
            }
            let file = match openat_file(
                &child,
                &entry_name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            ) {
                Ok(file) => file,
                // A concurrent writer renamed or removed it after the listing; nothing is exposed.
                Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(_) => {
                    return Err(CaptureError::Conflict(format!(
                        "spool_private_permissions: retained entry {} is unavailable",
                        entry_name.to_string_lossy()
                    )));
                }
            };
            let metadata = file.metadata()?;
            if !metadata.file_type().is_file() {
                return Err(CaptureError::Conflict(format!(
                    "spool_private_permissions: retained entry {} is not a regular file",
                    entry_name.to_string_lossy()
                )));
            }
            validate_private_metadata(
                &metadata,
                &format!("retained entry {}", entry_name.to_string_lossy()),
            )?;
        }
    }
    if scan_files {
        for entry_name in directory_entry_names(directory)? {
            if SPOOL_DIRECTORIES
                .iter()
                .any(|name| entry_name == OsStr::new(name))
            {
                continue;
            }
            scanned = scanned.saturating_add(1);
            if scanned > MAX_PHYSICAL_RECORDS {
                return Err(CaptureError::Limit(
                    "capture spool privacy validation contains too many physical records".into(),
                ));
            }
            let file = match openat_file(
                directory,
                &entry_name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            ) {
                Ok(file) => file,
                // A concurrent writer renamed or removed it after the listing; nothing is exposed.
                Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(_) => {
                    return Err(CaptureError::Conflict(format!(
                        "spool_private_permissions: retained metadata {} is unavailable",
                        entry_name.to_string_lossy()
                    )));
                }
            };
            let metadata = file.metadata()?;
            if !metadata.file_type().is_file() {
                return Err(CaptureError::Conflict(format!(
                    "spool_private_permissions: retained metadata {} is not a regular file",
                    entry_name.to_string_lossy()
                )));
            }
            validate_private_metadata(
                &metadata,
                &format!("retained metadata {}", entry_name.to_string_lossy()),
            )?;
        }
    }
    Ok(())
}

fn validate_bound_layout(root: &Path, directory: &File, format: &SpoolIdentityV1) -> Result<()> {
    if file_identity(directory)? != format.root || path_identity(root)? != format.root {
        return Err(CaptureError::Conflict(
            "spool_v4_identity_mismatch: capture spool root identity changed".into(),
        ));
    }
    for (name, expected) in [
        (OsStr::new("spool.lock"), format.spool_lock),
        (OsStr::new("managed-capture.lock"), format.managed_lock),
    ] {
        let file = openat_existing_private_file(directory, name)?;
        if file_identity(&file)? != expected {
            return Err(CaptureError::Conflict(format!(
                "spool_v4_identity_mismatch: {} identity changed",
                name.to_string_lossy()
            )));
        }
    }
    validate_private_authority_layout(directory, true)?;
    validate_initialized_spool(root, format)
}

fn create_private_lock_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .mode(0o600)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(CaptureError::Invalid(format!(
            "{} is not a regular private lock file",
            path.display()
        )));
    }
    Ok(file)
}

fn open_existing_private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(CaptureError::Conflict(format!(
            "spool_private_permissions: {} is not a regular private file",
            path.display()
        )));
    }
    validate_private_metadata(&metadata, &format!("retained file {}", path.display()))?;
    Ok(file)
}

fn validate_initialized_spool(root: &Path, format: &SpoolIdentityV1) -> Result<()> {
    if root.join("managed-occurrence-v2.json").exists() {
        return Err(CaptureError::Conflict(
            "capture spool contains an older managed occurrence format and is inspect/export only"
                .into(),
        ));
    }
    for name in SPOOL_DIRECTORIES {
        let metadata = fs::symlink_metadata(root.join(name))?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(CaptureError::Conflict(
                "capture spool initialized layout is incomplete".into(),
            ));
        }
    }
    for name in [
        "spool.lock",
        "managed-capture.lock",
        "compaction-v1.json",
        "order-v1.json",
        "observation-scope-v1.json",
        "capture-spool-v4.json",
    ] {
        open_existing_private_file(&root.join(name)).map_err(|_| {
            CaptureError::Conflict("capture spool initialized layout is incomplete".into())
        })?;
    }
    let spool_lock = open_existing_private_file(&root.join("spool.lock"))?;
    let managed_lock = open_existing_private_file(&root.join("managed-capture.lock"))?;
    if file_identity(&spool_lock)? != format.spool_lock
        || file_identity(&managed_lock)? != format.managed_lock
    {
        return Err(CaptureError::Conflict(
            "spool_v4_identity_mismatch: capture spool fixed lock identity changed".into(),
        ));
    }
    Ok(())
}

fn open_private_lock_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .mode(0o600)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(CaptureError::Conflict(format!(
            "spool_private_permissions: {} is not a regular private lock file",
            path.display()
        )));
    }
    validate_private_metadata(&metadata, &format!("retained lock {}", path.display()))?;
    Ok(file)
}

fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(CaptureError::Invalid(format!(
            "{} is not a regular retained file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(CaptureError::Limit(format!(
            "{} exceeds {max} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Err(CaptureError::Conflict(format!(
            "{} already exists",
            path.display()
        )));
    }
    atomic_replace_inner(path, bytes, true)
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_replace_inner(path, bytes, false)
}

fn atomic_replace_inner(path: &Path, bytes: &[u8], new_only: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| CaptureError::Invalid("retained path has no parent".into()))?;
    let target = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CaptureError::Invalid("retained filename is not UTF-8".into()))?;
    let temp = parent.join(format!(
        ".capture-{target}-{}-{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    if let Err(error) = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        if new_only && path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "retained path already exists",
            ));
        }
        fs::rename(&temp, path)?;
        #[cfg(test)]
        FAIL_DIRECTORY_SYNC_FOR.with(|target| {
            let should_fail = target.borrow().as_deref() == Some(path);
            if should_fail {
                target.borrow_mut().take();
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            Ok(())
        })?;
        File::open(parent)?.sync_all()
    })() {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

fn recover_spool_temps(root: &Path, limits: SpoolLimits) -> Result<()> {
    let directories = [
        None,
        Some("artifacts"),
        Some("outbox"),
        Some("acknowledged"),
        Some("sessions"),
        Some("associations"),
        Some("deferred"),
        Some("observations"),
        Some("managed-invocations"),
        Some("recipe-attempts"),
        Some("recipe-fences"),
    ];
    let mut scanned = 0usize;
    for directory_name in directories {
        let directory = directory_name.map_or_else(|| root.to_owned(), |name| root.join(name));
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            scanned = scanned.saturating_add(1);
            if scanned > MAX_PHYSICAL_RECORDS {
                return Err(CaptureError::Limit(
                    "capture spool recovery contains too many physical records".into(),
                ));
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| CaptureError::Invalid("capture filename is not UTF-8".into()))?;
            if !name.starts_with(".capture-") {
                continue;
            }
            let file_type = entry.file_type()?;
            if !file_type.is_file() || file_type.is_symlink() {
                quarantine_entry(root, &entry.path(), &name)?;
                continue;
            }
            let Some(target_name) = owned_temp_target(&name) else {
                quarantine_entry(root, &entry.path(), &name)?;
                continue;
            };
            let bytes = match read_bounded(&entry.path(), limits.max_total_bytes) {
                Ok(bytes) => bytes,
                Err(_) => {
                    quarantine_entry(root, &entry.path(), &name)?;
                    continue;
                }
            };
            if !valid_recovered_temp(directory_name, &target_name, &bytes, limits) {
                quarantine_entry(root, &entry.path(), &name)?;
                continue;
            }
            let target = directory.join(&target_name);
            if target.exists() {
                if read_bounded(&target, limits.max_total_bytes)? == bytes {
                    fs::remove_file(entry.path())?;
                    File::open(&directory)?.sync_all()?;
                } else {
                    quarantine_entry(root, &entry.path(), &name)?;
                }
            } else {
                fs::rename(entry.path(), target)?;
                File::open(&directory)?.sync_all()?;
            }
        }
    }
    Ok(())
}

fn owned_temp_target(name: &str) -> Option<String> {
    let rest = name.strip_prefix(".capture-")?;
    let body = rest.strip_suffix(".tmp")?;
    let (target_and_pid, counter) = body.rsplit_once('-')?;
    let (target, pid) = target_and_pid.rsplit_once('-')?;
    if target.is_empty()
        || target.len() > 255
        || target == "."
        || target == ".."
        || target.contains('/')
        || pid.is_empty()
        || counter.is_empty()
        || !pid.bytes().all(|byte| byte.is_ascii_digit())
        || !counter.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some(target.to_owned())
}

fn valid_recovered_temp(
    directory: Option<&str>,
    target: &str,
    bytes: &[u8],
    limits: SpoolLimits,
) -> bool {
    match directory {
        Some("artifacts") => {
            bytes.len() as u64 <= limits.max_artifact_bytes && sha256(bytes) == target
        }
        Some("outbox") => serde_json::from_slice::<CaptureEnvelopeV1>(bytes)
            .ok()
            .filter(|envelope| envelope.validate().is_ok())
            .is_some_and(|envelope| format!("{}.json", envelope_key(&envelope)) == target),
        Some("acknowledged") => {
            valid_digest(target) && std::str::from_utf8(bytes).is_ok_and(valid_digest)
        }
        Some("sessions") => {
            valid_hash_json_name(target)
                && serde_json::from_slice::<SequenceState>(bytes)
                    .ok()
                    .is_some_and(|state| valid_sequence_state(&state))
        }
        Some("associations") => serde_json::from_slice::<SessionAssociationV1>(bytes)
            .ok()
            .filter(|association| validate_association(association).is_ok())
            .is_some_and(|association| {
                format!(
                    "{}.json",
                    sha256(association.observed_session_id.as_bytes())
                ) == target
            }),
        Some("deferred") => decode_deferred_record(bytes)
            .ok()
            .filter(|record| {
                record.receipt_order > 0
                    && validate_deferred(&record.hook).is_ok()
                    && record
                        .target
                        .as_ref()
                        .is_none_or(|association| validate_association(association).is_ok())
            })
            .is_some_and(|record| {
                format!("{}.json", sha256(record.hook.occurrence_id.as_bytes())) == target
            }),
        Some("observations") => {
            valid_digest(target) && std::str::from_utf8(bytes).is_ok_and(valid_digest)
        }
        Some("managed-invocations") => serde_json::from_slice::<ManagedInvocationV1>(bytes)
            .ok()
            .filter(|marker| validate_managed_invocation(marker).is_ok())
            .is_some_and(|marker| format!("{}.json", marker.marker_id) == target),
        Some("recipe-attempts") => serde_json::from_slice::<RecipeAttemptV1>(bytes)
            .ok()
            .filter(|attempt| validate_recipe_attempt(attempt).is_ok())
            .is_some_and(|attempt| format!("{}.json", attempt.fence_id) == target),
        Some("recipe-fences") => serde_json::from_slice::<RecipeFenceBindingV1>(bytes)
            .ok()
            .filter(|binding| validate_recipe_fence_binding(binding).is_ok())
            .is_some_and(|binding| format!("{}.json", binding.fence_id) == target),
        None if target == "namespace-v1.json" => serde_json::from_slice::<SpoolNamespaceV1>(bytes)
            .ok()
            .is_some_and(|namespace| validate_namespace(&namespace).is_ok()),
        None if target == "delivery-cursor-v1.json" => {
            serde_json::from_slice::<DeliveryCursorV1>(bytes)
                .ok()
                .is_some_and(|cursor| cursor.version == 1 && valid_digest(&cursor.envelope_key))
        }
        None if target == "compaction-v1.json" => {
            serde_json::from_slice::<CompactionStateV1>(bytes)
                .ok()
                .is_some_and(|state| valid_compaction_state(&state))
        }
        None if target == "order-v1.json" => serde_json::from_slice::<ObservedOrderStateV1>(bytes)
            .ok()
            .is_some_and(|state| valid_observed_order_state(&state)),
        None if target == "observation-scope-v1.json" => {
            serde_json::from_slice::<ObservationScopeV1>(bytes)
                .ok()
                .is_some_and(|scope| scope.version == 1 && valid_observation_scope_id(&scope.id))
        }
        _ => false,
    }
}

fn valid_hash_json_name(name: &str) -> bool {
    name.len() == 69 && name.ends_with(".json") && valid_digest(&name[..64])
}

fn valid_sequence_state(state: &SequenceState) -> bool {
    state.next > 0
        && state.events.len() <= MAX_SEQUENCE_EVENTS
        && state
            .events
            .values()
            .all(|sequence| *sequence > 0 && *sequence < state.next)
}

fn valid_quarantine_name(name: &str) -> bool {
    let Some((digest, counter)) = name.split_once('.') else {
        return false;
    };
    valid_digest(digest) && !counter.is_empty() && counter.bytes().all(|byte| byte.is_ascii_digit())
}

fn quarantine_entries(root: &Path, limit: usize) -> Result<Vec<QuarantineEntryV1>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(root.join("quarantine"))? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| CaptureError::Invalid("quarantine filename is not UTF-8".into()))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !valid_quarantine_name(&name)
            || !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
        {
            return Err(CaptureError::Invalid(
                "quarantine contains an invalid retained entry".into(),
            ));
        }
        entries.push(QuarantineEntryV1 {
            name,
            size_bytes: metadata.len(),
        });
        if entries.len() == limit {
            break;
        }
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

fn quarantine_entry(root: &Path, source: &Path, name: &str) -> Result<()> {
    let quarantine = root.join("quarantine");
    let digest = sha256(name.as_bytes());
    let destination = loop {
        let candidate = quarantine.join(format!(
            "{digest}.{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if !candidate.exists() {
            break candidate;
        }
    };
    fs::rename(source, destination)?;
    File::open(
        source.parent().ok_or_else(|| {
            CaptureError::Invalid("capture quarantine source has no parent".into())
        })?,
    )?
    .sync_all()?;
    File::open(&quarantine)?.sync_all()?;
    Ok(())
}

fn regular_file_names(directory: &Path, max: usize) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = entry.file_type()?;
        if !metadata.is_file() || metadata.is_symlink() {
            return Err(CaptureError::Invalid(format!(
                "{} contains a non-regular entry",
                directory.display()
            )));
        }
        names.push(
            entry
                .file_name()
                .into_string()
                .map_err(|_| CaptureError::Invalid("capture filename is not UTF-8".into()))?,
        );
        if names.len() > max {
            return Err(CaptureError::Limit(
                "capture spool contains too many records".into(),
            ));
        }
    }
    Ok(names)
}

type ActionIdentity = (String, i64, String);

fn action_key_start(start: &ActionStartV1) -> ActionIdentity {
    (
        start.work_id.clone(),
        start.generation,
        start.action_id.clone(),
    )
}

fn action_key_finish(finish: &ActionFinishV1) -> ActionIdentity {
    (
        finish.work_id.clone(),
        finish.generation,
        finish.action_id.clone(),
    )
}

fn outstanding_action_keys(root: &Path, max: usize) -> Result<BTreeSet<ActionIdentity>> {
    let mut starts = BTreeSet::new();
    let mut finishes = BTreeSet::new();
    for name in regular_file_names(&root.join("outbox"), max)? {
        if name.starts_with(".capture-") {
            continue;
        }
        let bytes = read_bounded(&root.join("outbox").join(name), MAX_SEQUENCE_BYTES)?;
        match serde_json::from_slice::<CaptureEnvelopeV1>(&bytes)? {
            CaptureEnvelopeV1::ActionStart(start) => {
                starts.insert(action_key_start(&start));
            }
            CaptureEnvelopeV1::ActionFinish(finish) => {
                finishes.insert(action_key_finish(&finish));
            }
            CaptureEnvelopeV1::EventBatch(_) | CaptureEnvelopeV1::Usage(_) => {}
        }
    }
    Ok(starts.difference(&finishes).cloned().collect())
}

fn outstanding_actions(root: &Path, max: usize) -> Result<usize> {
    Ok(outstanding_action_keys(root, max)?.len())
}

/// Completes the deletion half of a compaction transaction whose index was already made durable.
/// The caller holds the global spool lock, so no live writer can own these exact remnants.
fn recover_compacted_remainders(root: &Path, limits: SpoolLimits) -> Result<()> {
    let state = load_compaction_state(root, limits.max_total_bytes)?;
    let mut changed_occurrences = false;
    for receipt in state.native_occurrences.values() {
        changed_occurrences |= cleanup_compacted_occurrence_marker(root, receipt)?;
    }
    let mut changed_outbox = false;
    let mut changed_acknowledged = false;
    for key in regular_file_names(&root.join("acknowledged"), limits.max_records)? {
        let Some(compacted) = state.entries.get(&key) else {
            continue;
        };
        let marker = root.join("acknowledged").join(&key);
        if read_bounded(&marker, 64)? != compacted.digest.as_bytes() {
            return Err(CaptureError::Conflict(
                "compaction acknowledgement differs from indexed history".into(),
            ));
        }
        let retained = root.join("outbox").join(format!("{key}.json"));
        match read_bounded(&retained, limits.max_total_bytes) {
            Ok(bytes) => {
                if sha256(&bytes) != compacted.digest {
                    return Err(CaptureError::Conflict(
                        "compaction remnant differs from indexed history".into(),
                    ));
                }
                let envelope: CaptureEnvelopeV1 = serde_json::from_slice(&bytes)?;
                envelope
                    .validate()
                    .map_err(|error| CaptureError::Invalid(error.to_string()))?;
                if envelope_key(&envelope) != key
                    || compacted
                        .envelope
                        .as_ref()
                        .is_some_and(|expected| expected != &envelope)
                {
                    return Err(CaptureError::Conflict(
                        "compaction remnant identity differs from indexed history".into(),
                    ));
                }
                fs::remove_file(retained)?;
                changed_outbox = true;
            }
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::remove_file(marker)?;
        changed_acknowledged = true;
    }
    let mut changed_observations = false;
    for key in regular_file_names(&root.join("observations"), limits.max_records)? {
        let Some(fingerprint) = state.observations.get(&key) else {
            continue;
        };
        let path = root.join("observations").join(&key);
        if read_bounded(&path, 64)? != fingerprint.as_bytes() {
            return Err(CaptureError::Conflict(
                "compaction observation differs from indexed history".into(),
            ));
        }
        fs::remove_file(path)?;
        changed_observations = true;
    }
    if changed_outbox {
        File::open(root.join("outbox"))?.sync_all()?;
    }
    if changed_acknowledged {
        File::open(root.join("acknowledged"))?.sync_all()?;
    }
    if changed_observations {
        File::open(root.join("observations"))?.sync_all()?;
    }
    let cursor_path = root.join("delivery-cursor-v1.json");
    let changed_cursor = match load_delivery_cursor(root)? {
        Some(cursor) if state.entries.contains_key(&cursor) => {
            fs::remove_file(&cursor_path)?;
            true
        }
        _ => false,
    };
    let order_path = root.join("order-v1.json");
    let mut order_state = load_observed_order_state(&order_path, limits.max_total_bytes)?;
    let mut reclaimed_orders = Vec::new();
    for (operation_id, order) in &order_state.orders {
        if let Some(compacted) = compacted_observed_order(&state, operation_id)? {
            if compacted != *order {
                return Err(CaptureError::Conflict(
                    "compacted action-start order differs from active observation order".into(),
                ));
            }
            reclaimed_orders.push(operation_id.clone());
        }
    }
    let changed_orders = !reclaimed_orders.is_empty();
    if changed_orders {
        for operation_id in reclaimed_orders {
            order_state.orders.remove(&operation_id);
        }
        atomic_replace(&order_path, &serde_json::to_vec(&order_state)?)?;
    }
    if changed_outbox
        || changed_acknowledged
        || changed_observations
        || changed_cursor
        || changed_orders
        || changed_occurrences
    {
        File::open(root)?.sync_all()?;
    }
    Ok(())
}

fn usage(root: &Path, _configured_max: usize) -> Result<(usize, u64)> {
    let mut records = 0usize;
    let mut bytes = 0u64;
    for name in [
        "artifacts",
        "outbox",
        "acknowledged",
        "sessions",
        "associations",
        "deferred",
        "observations",
        "managed-invocations",
        "recipe-attempts",
        "recipe-fences",
        "quarantine",
    ] {
        for entry in fs::read_dir(root.join(name))? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(CaptureError::Invalid(
                    "capture spool contains a non-regular retained entry".into(),
                ));
            }
            records = records.saturating_add(1);
            bytes = bytes.saturating_add(metadata.len());
            if records > MAX_PHYSICAL_RECORDS {
                return Err(CaptureError::Limit(
                    "capture spool contains too many physical records".into(),
                ));
            }
        }
    }
    let namespace = root.join("namespace-v1.json");
    if namespace.exists() {
        let metadata = fs::symlink_metadata(namespace)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(CaptureError::Invalid(
                "capture spool namespace is not a regular file".into(),
            ));
        }
        records = records.saturating_add(1);
        bytes = bytes.saturating_add(metadata.len());
    }
    let compaction = root.join("compaction-v1.json");
    if compaction.exists() {
        let metadata = fs::symlink_metadata(compaction)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(CaptureError::Invalid(
                "capture spool compaction index is not a regular file".into(),
            ));
        }
        records = records.saturating_add(1);
        bytes = bytes.saturating_add(metadata.len());
    }
    let order = root.join("order-v1.json");
    if order.exists() {
        let metadata = fs::symlink_metadata(order)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(CaptureError::Invalid(
                "capture spool observed-order state is not a regular file".into(),
            ));
        }
        records = records.saturating_add(1);
        bytes = bytes.saturating_add(metadata.len());
    }
    let cursor = root.join("delivery-cursor-v1.json");
    if cursor.exists() {
        let metadata = fs::symlink_metadata(cursor)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(CaptureError::Invalid(
                "capture spool delivery cursor is not a regular file".into(),
            ));
        }
        records = records.saturating_add(1);
        bytes = bytes.saturating_add(metadata.len());
    }
    let observation_scope = root.join("observation-scope-v1.json");
    if observation_scope.exists() {
        let metadata = fs::symlink_metadata(observation_scope)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(CaptureError::Invalid(
                "capture spool observation scope is not a regular file".into(),
            ));
        }
        records = records.saturating_add(1);
        bytes = bytes.saturating_add(metadata.len());
    }
    for name in [
        "spool.lock",
        "managed-capture.lock",
        "capture-spool-v4.json",
    ] {
        let path = root.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(CaptureError::Invalid(format!(
                "capture spool fixed metadata {name} is not a regular file"
            )));
        }
        records = records.saturating_add(1);
        bytes = bytes.saturating_add(metadata.len());
    }
    if records > MAX_PHYSICAL_RECORDS {
        return Err(CaptureError::Limit(
            "capture spool contains too many physical records".into(),
        ));
    }
    Ok((records, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change_record::GapReasonV1;

    fn export_anchors(parent: &Path) -> Vec<PathBuf> {
        let mut anchors = fs::read_dir(parent)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".agentscm-export-anchor-")
                    .then(|| entry.path())
            })
            .collect::<Vec<_>>();
        anchors.sort();
        anchors
    }

    fn completed_occurrence_marker(tool_use_id: &str) -> ManagedInvocationV1 {
        let observed_session_id = "observed_occurrence".to_owned();
        ManagedInvocationV1 {
            version: 4,
            marker_id: sha256(format!("{observed_session_id}\0{tool_use_id}").as_bytes()),
            operation_prefix: Some("capture:occurrence".into()),
            observed_session_id,
            canonical_session_id: "canonical_occurrence".into(),
            work_id: "wrk_occurrence".into(),
            generation: 1,
            tool_use_id: tool_use_id.into(),
            command_digest: "1".repeat(64),
            repository: "example/repository".into(),
            worktree_id: "worktree_occurrence".into(),
            base_ref: Some("main".into()),
            root_path: Some("/synthetic/repository".into()),
            root_device: 1,
            root_inode: 1,
            source: Some(SourceBindingV1::Native {
                repository: "example/repository".into(),
                revision: "2".repeat(64),
                change_id: "change_occurrence".into(),
                context_digest: "3".repeat(64),
                source_generation: 1,
            }),
            setup_digest: Some("4".repeat(64)),
            recipe_digest: Some("5".repeat(64)),
            parent_session_id: None,
            capture_mode: Some(CaptureModeV1::MetadataOnly),
            observed_at: 1,
            pre_fingerprint: Some("6".repeat(64)),
            post_fingerprint: Some("7".repeat(64)),
            route: Some(NativeOccurrenceRouteV1::Managed),
        }
    }

    fn ordered_start(operation_id: &str) -> ActionStartV1 {
        ActionStartV1 {
            schema_version: crate::change_record::SCHEMA_VERSION_V1,
            operation_id: operation_id.into(),
            work_id: "wrk_quota".into(),
            generation: 1,
            session_id: "session_quota".into(),
            action_id: format!("act:{operation_id}"),
            source: SourceBindingV1::Native {
                repository: "example/repository".into(),
                revision: "1".repeat(64),
                change_id: "change_quota".into(),
                context_digest: "2".repeat(64),
                source_generation: 1,
            },
            kind: ActionKindV1::Execution,
            recipe_provenance: crate::change_record::RecipeProvenanceV1::ObservedNativeInput,
            recipe_digest: "3".repeat(64),
            setup_digest: "4".repeat(64),
            started_at: 1,
            observed_order: 0,
            observation_scope_id: None,
        }
    }

    fn recipe_reservation(
        work_id: &str,
        recipe_id: &str,
        root_inode: u64,
    ) -> RecipeAttemptReservationV1 {
        let root_path = "/synthetic/repository".to_owned();
        let repository = "example/repository".to_owned();
        let worktree_id = "worktree_fixture".to_owned();
        RecipeAttemptReservationV1 {
            fence_id: sha256(
                format!("{work_id}\0{root_path}\0{repository}\0{worktree_id}\0{recipe_id}")
                    .as_bytes(),
            ),
            work_id: work_id.into(),
            generation: 1,
            observed_session_id: "observed_fixture".into(),
            canonical_session_id: "canonical_fixture".into(),
            repository,
            worktree_id,
            root_path,
            root_device: 1,
            root_inode,
            base_ref: "main".into(),
            recipe_id: recipe_id.into(),
            recipe_digest: "5".repeat(64),
            setup_digest: "6".repeat(64),
            coverage: CoverageV1::Complete,
            command_digest: "7".repeat(64),
            host_id: None,
            boot_id: None,
        }
    }

    #[test]
    fn replacement_admission_counts_old_and_new_physical_bytes() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        spool.observed_order_for("operation:first").unwrap();
        let target = spool.root.join("order-v1.json");
        let replacement_bytes = fs::metadata(&target).unwrap().len().saturating_add(128);
        let (records, bytes) = usage(&spool.root, spool.limits.max_records).unwrap();
        let constrained = LocalSpool {
            root: spool.root.clone(),
            limits: SpoolLimits {
                max_records: records.saturating_add(4),
                max_total_bytes: bytes.saturating_add(replacement_bytes).saturating_sub(1),
                max_artifact_bytes: spool.limits.max_artifact_bytes,
            },
            identity: spool.identity,
        };

        assert!(matches!(
            constrained.reserve_replace(&target, replacement_bytes),
            Err(CaptureError::Limit(_))
        ));
    }

    #[test]
    fn released_prestart_order_is_removed_but_never_reused() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool_path = temp.path().join("spool");
        let spool = LocalSpool::open(&spool_path).unwrap();
        let released = spool.observed_order_for("operation:prestart").unwrap();
        drop(spool);
        let recovered = LocalSpool::open(&spool_path).unwrap();
        recovered
            .release_observed_order("operation:prestart", released)
            .unwrap();
        assert_eq!(
            recovered
                .retained_observed_order("operation:prestart")
                .unwrap(),
            None
        );
        assert!(recovered.observed_order_for("operation:next").unwrap() > released);
    }

    #[test]
    fn rejected_ordered_starts_never_grow_the_order_ledger() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let order_path = spool.root.join("order-v1.json");
        let original = fs::read(&order_path).unwrap();
        let (records, bytes) = usage(&spool.root, spool.limits.max_records).unwrap();
        let constrained = LocalSpool {
            root: spool.root.clone(),
            limits: SpoolLimits {
                max_records: records.saturating_add(4),
                max_total_bytes: bytes.saturating_add(128),
                max_artifact_bytes: 1,
            },
            identity: spool.identity,
        };

        for index in 0..8 {
            assert!(matches!(
                constrained
                    .append_action_start_ordered(ordered_start(&format!("quota:start:{index}"))),
                Err(CaptureError::Limit(_))
            ));
            assert_eq!(fs::read(&order_path).unwrap(), original);
            assert!(
                constrained
                    .action_start(&format!("quota:start:{index}"))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn visible_start_after_directory_sync_error_keeps_unique_replayable_order() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool_path = temp.path().join("spool");
        let spool = LocalSpool::open(&spool_path).unwrap();
        let first = ordered_start("sync-uncertain:first");
        let first_path = spool.root.join("outbox").join(format!(
            "{}.json",
            envelope_key_parts("start", &first.operation_id)
        ));
        FAIL_DIRECTORY_SYNC_FOR.with(|target| *target.borrow_mut() = Some(first_path));

        assert!(matches!(
            spool.append_action_start_ordered(first.clone()),
            Err(CaptureError::Io(_))
        ));
        let retained_first = spool
            .action_start(&first.operation_id)
            .unwrap()
            .expect("renamed start must remain visible");
        assert_eq!(
            spool.retained_observed_order(&first.operation_id).unwrap(),
            Some(retained_first.observed_order)
        );

        let (_, second) = spool
            .append_action_start_ordered(ordered_start("sync-uncertain:second"))
            .unwrap();
        assert!(second.observed_order > retained_first.observed_order);

        let reopened = LocalSpool::open(&spool_path).unwrap();
        let (outcome, replayed) = reopened.append_action_start_ordered(first).unwrap();
        assert_eq!(outcome, AppendOutcome::Duplicate);
        assert_eq!(replayed.observed_order, retained_first.observed_order);
        assert_eq!(
            reopened
                .retained_observed_order("sync-uncertain:second")
                .unwrap(),
            Some(second.observed_order)
        );
    }

    #[test]
    fn permanent_recipe_lock_is_bounded_per_checkout_not_work_or_inode() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let first = recipe_reservation("wrk_first", "recipe_first", 10);
        let second = recipe_reservation("wrk_second", "recipe_second", 20);
        let guard = spool.try_recipe_fence(&first).unwrap().unwrap();
        assert!(spool.try_recipe_fence(&second).unwrap().is_none());
        drop(guard);
        drop(spool.try_recipe_fence(&second).unwrap().unwrap());
        assert_eq!(
            fs::read_dir(spool.root.join("recipe-fences"))
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn compacted_occurrence_sync_crash_recovers_only_an_exact_claim_remnant() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool_path = temp.path().join("spool");
        let spool = LocalSpool::open(&spool_path).unwrap();
        let marker = completed_occurrence_marker("sync-crash-exact");
        let marker_path = spool
            .root
            .join("managed-invocations")
            .join(format!("{}.json", marker.marker_id));
        spool.retain_managed_invocation(&marker).unwrap();
        FAIL_OCCURRENCE_UNLINK_AFTER_RECEIPT.with(|fail| fail.set(true));
        assert!(matches!(
            spool.remove_managed_invocation(&marker.marker_id),
            Err(CaptureError::Io(_))
        ));
        FAIL_OCCURRENCE_UNLINK_AFTER_RECEIPT.with(|fail| fail.set(false));
        assert!(marker_path.is_file());
        drop(spool);

        let reopened = LocalSpool::open(&spool_path).unwrap();
        assert!(!marker_path.exists());
        assert_eq!(
            reopened
                .occurrence_receipt(&marker.observed_session_id, &marker.tool_use_id)
                .unwrap(),
            Some(occurrence_receipt_from_marker(&marker).unwrap())
        );
    }

    #[test]
    fn compacted_occurrence_sync_crash_refuses_a_mismatched_claim_remnant() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool_path = temp.path().join("spool");
        let spool = LocalSpool::open(&spool_path).unwrap();
        let marker = completed_occurrence_marker("sync-crash-mismatch");
        let marker_path = spool
            .root
            .join("managed-invocations")
            .join(format!("{}.json", marker.marker_id));
        spool.retain_managed_invocation(&marker).unwrap();
        FAIL_OCCURRENCE_UNLINK_AFTER_RECEIPT.with(|fail| fail.set(true));
        assert!(spool.remove_managed_invocation(&marker.marker_id).is_err());
        FAIL_OCCURRENCE_UNLINK_AFTER_RECEIPT.with(|fail| fail.set(false));
        let mut changed = marker.clone();
        changed.work_id = "wrk_changed".into();
        atomic_replace(&marker_path, &serde_json::to_vec(&changed).unwrap()).unwrap();
        drop(spool);

        assert!(matches!(
            LocalSpool::open(&spool_path),
            Err(CaptureError::Conflict(message))
                if message.contains("differs from its live claim remnant")
        ));
        assert!(marker_path.is_file());
    }

    #[test]
    fn export_refuses_a_destination_created_after_preflight_without_touching_it() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"owned export source",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let destination = temp.path().join("archive");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *EXPORT_BEFORE_INSTALL_BARRIER.lock().unwrap() =
            Some((destination.clone(), barrier.clone()));
        let source = spool.root.clone();
        let export_destination = destination.clone();
        let exporter = std::thread::spawn(move || export_spool_archive(source, export_destination));
        barrier.wait();
        fs::create_dir(&destination).unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(destination.join("foreign.txt"), b"foreign bytes").unwrap();
        barrier.wait();
        let result = exporter.join().unwrap();
        *EXPORT_BEFORE_INSTALL_BARRIER.lock().unwrap() = None;

        assert!(result.is_err());
        assert_eq!(
            fs::read(destination.join("foreign.txt")).unwrap(),
            b"foreign bytes"
        );
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o755);
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
        assert_eq!(
            spool.read_artifact(&artifact).unwrap(),
            b"owned export source"
        );
    }

    #[test]
    fn export_uses_its_retained_destination_after_parent_replacement() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"descriptor-bound export source",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let destination = temp.path().join("archive");
        let moved = temp.path().join("owned-archive-moved");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *EXPORT_AFTER_INSTALL_BARRIER.lock().unwrap() =
            Some((destination.clone(), barrier.clone()));
        let source = spool.root.clone();
        let export_destination = destination.clone();
        let exporter = std::thread::spawn(move || export_spool_archive(source, export_destination));
        barrier.wait();
        fs::rename(&destination, &moved).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(destination.join("foreign.txt"), b"replacement bytes").unwrap();
        barrier.wait();
        let result = exporter.join().unwrap();
        *EXPORT_AFTER_INSTALL_BARRIER.lock().unwrap() = None;

        assert!(matches!(
            result,
            Err(CaptureError::Conflict(message))
                if message.contains("destination identity changed")
        ));
        assert_eq!(
            fs::read(destination.join("foreign.txt")).unwrap(),
            b"replacement bytes"
        );
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o755);
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
        assert!(moved.join("capture-export-manifest-v1.json").is_file());
        assert_eq!(
            spool.read_artifact(&artifact).unwrap(),
            b"descriptor-bound export source"
        );
    }

    #[test]
    fn export_rejects_a_replaced_anchor_before_open_without_adopting_it() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"anchor acquisition source",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let destination = temp.path().join("archive");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *EXPORT_AFTER_ANCHOR_CREATE_BARRIER.lock().unwrap() =
            Some((destination.clone(), barrier.clone()));
        let source = spool.root.clone();
        let export_destination = destination.clone();
        let exporter = std::thread::spawn(move || export_spool_archive(source, export_destination));

        barrier.wait();
        let anchors = export_anchors(temp.path());
        assert_eq!(anchors.len(), 1);
        let anchor = anchors[0].clone();
        let moved_anchor = temp.path().join("original-anchor-moved");
        fs::rename(&anchor, &moved_anchor).unwrap();
        fs::create_dir(&anchor).unwrap();
        fs::set_permissions(&anchor, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(anchor.join("foreign.txt"), b"foreign anchor bytes").unwrap();
        barrier.wait();
        let result = exporter.join().unwrap();
        *EXPORT_AFTER_ANCHOR_CREATE_BARRIER.lock().unwrap() = None;

        assert!(matches!(
            result,
            Err(CaptureError::Conflict(message))
                if message.contains("private-mode validation failed")
                    && message.contains("privacy and contents are unverified")
        ));
        assert!(!destination.exists());
        assert_eq!(
            fs::read(anchor.join("foreign.txt")).unwrap(),
            b"foreign anchor bytes"
        );
        assert_eq!(fs::metadata(&anchor).unwrap().mode() & 0o777, 0o755);
        assert_eq!(fs::read_dir(&anchor).unwrap().count(), 1);
        assert_eq!(fs::read_dir(&moved_anchor).unwrap().count(), 0);
        assert_eq!(
            spool.read_artifact(&artifact).unwrap(),
            b"anchor acquisition source"
        );
    }

    #[test]
    fn export_rejects_unexpected_anchor_content_after_acl_clear() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"unexpected anchor content source",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let destination = temp.path().join("archive");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *EXPORT_AFTER_ANCHOR_CREATE_BARRIER.lock().unwrap() =
            Some((destination.clone(), barrier.clone()));
        let source = spool.root.clone();
        let export_destination = destination.clone();
        let exporter = std::thread::spawn(move || export_spool_archive(source, export_destination));

        barrier.wait();
        let anchors = export_anchors(temp.path());
        assert_eq!(anchors.len(), 1);
        let unexpected = anchors[0].join("foreign-entry");
        fs::write(&unexpected, b"untrusted retained bytes").unwrap();
        barrier.wait();
        let result = exporter.join().unwrap();
        *EXPORT_AFTER_ANCHOR_CREATE_BARRIER.lock().unwrap() = None;

        assert!(matches!(
            result,
            Err(CaptureError::Conflict(message))
                if message.contains("unexpected entry retained untouched")
                    && message.contains("contents are unverified")
        ));
        assert!(!destination.exists());
        assert_eq!(fs::read(unexpected).unwrap(), b"untrusted retained bytes");
        assert_eq!(fs::read_dir(&anchors[0]).unwrap().count(), 1);
        assert_eq!(
            spool.read_artifact(&artifact).unwrap(),
            b"unexpected anchor content source"
        );
    }

    #[test]
    fn export_readdir_errors_never_admit_an_unscanned_anchor() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        for fail_after_entries in [0, 1] {
            let temp = tempfile::TempDir::new().unwrap();
            let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
            let artifact = spool
                .retain_artifact(
                    b"directory scan failure source",
                    ArtifactMediaV1::Binary,
                    RetentionClassV1::WorkLocal,
                )
                .unwrap();
            let destination = temp.path().join("archive");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            *EXPORT_AFTER_ANCHOR_CREATE_BARRIER.lock().unwrap() =
                Some((destination.clone(), barrier.clone()));
            let source = spool.root.clone();
            let export_destination = destination.clone();
            let exporter =
                std::thread::spawn(move || export_spool_archive(source, export_destination));

            barrier.wait();
            let anchors = export_anchors(temp.path());
            assert_eq!(anchors.len(), 1);
            let first = anchors[0].join("foreign-first");
            let second = anchors[0].join("foreign-second");
            fs::write(&first, b"first foreign bytes").unwrap();
            fs::write(&second, b"second foreign bytes").unwrap();
            let opened_anchor = open_private_directory(&anchors[0]).unwrap();
            *FAIL_EXPORT_READDIR_FOR.lock().unwrap() =
                Some((file_identity(&opened_anchor).unwrap(), fail_after_entries));
            barrier.wait();
            let result = exporter.join().unwrap();
            *FAIL_EXPORT_READDIR_FOR.lock().unwrap() = None;
            *EXPORT_AFTER_ANCHOR_CREATE_BARRIER.lock().unwrap() = None;

            assert!(matches!(
                result,
                Err(CaptureError::Conflict(message))
                    if message.contains("anchor emptiness scan failed")
                        && message.contains("contents are unverified")
            ));
            assert!(!destination.exists());
            assert!(!anchors[0].join("owned-output").exists());
            assert_eq!(fs::read(&first).unwrap(), b"first foreign bytes");
            assert_eq!(fs::read(&second).unwrap(), b"second foreign bytes");
            assert_eq!(fs::read_dir(&anchors[0]).unwrap().count(), 2);
            assert_eq!(
                spool.read_artifact(&artifact).unwrap(),
                b"directory scan failure source"
            );
        }
    }

    #[test]
    fn export_install_source_stays_bound_when_the_anchor_name_is_replaced() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"retained anchor descriptor source",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let destination = temp.path().join("archive");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *EXPORT_AFTER_CHILD_OPEN_BARRIER.lock().unwrap() =
            Some((destination.clone(), barrier.clone()));
        let source = spool.root.clone();
        let export_destination = destination.clone();
        let exporter = std::thread::spawn(move || export_spool_archive(source, export_destination));

        barrier.wait();
        let anchors = export_anchors(temp.path());
        assert_eq!(anchors.len(), 1);
        let anchor = anchors[0].clone();
        let moved_anchor = temp.path().join("opened-anchor-moved");
        fs::rename(&anchor, &moved_anchor).unwrap();
        fs::create_dir(&anchor).unwrap();
        fs::set_permissions(&anchor, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(anchor.join("owned-output")).unwrap();
        fs::write(
            anchor.join("owned-output").join("foreign.txt"),
            b"foreign source-name bytes",
        )
        .unwrap();
        barrier.wait();
        let result = exporter.join().unwrap();
        *EXPORT_AFTER_CHILD_OPEN_BARRIER.lock().unwrap() = None;

        assert!(result.is_ok());
        assert!(
            destination
                .join("capture-export-manifest-v1.json")
                .is_file()
        );
        assert_eq!(
            fs::read(anchor.join("owned-output").join("foreign.txt")).unwrap(),
            b"foreign source-name bytes"
        );
        assert_eq!(fs::metadata(&anchor).unwrap().mode() & 0o777, 0o755);
        assert_eq!(fs::read_dir(&moved_anchor).unwrap().count(), 0);
        assert_eq!(
            spool.read_artifact(&artifact).unwrap(),
            b"retained anchor descriptor source"
        );
    }

    #[test]
    fn export_rename_failure_reports_the_retained_owned_child_tree() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"rename failure source",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let destination = temp.path().join("archive");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *EXPORT_AFTER_CHILD_OPEN_BARRIER.lock().unwrap() =
            Some((destination.clone(), barrier.clone()));
        let source = spool.root.clone();
        let export_destination = destination.clone();
        let exporter = std::thread::spawn(move || export_spool_archive(source, export_destination));

        barrier.wait();
        fs::create_dir(&destination).unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(destination.join("foreign.txt"), b"foreign destination").unwrap();
        barrier.wait();
        let result = exporter.join().unwrap();
        *EXPORT_AFTER_CHILD_OPEN_BARRIER.lock().unwrap() = None;

        assert!(matches!(
            result,
            Err(CaptureError::Conflict(message))
                if message.contains("destination installation failed")
                    && message.contains("owned child tree owned-output")
                    && message.contains("export anchor")
        ));
        assert_eq!(
            fs::read(destination.join("foreign.txt")).unwrap(),
            b"foreign destination"
        );
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o755);
        let anchors = export_anchors(temp.path());
        assert_eq!(anchors.len(), 1);
        assert!(anchors[0].join("owned-output").is_dir());
        assert_eq!(
            fs::read_dir(anchors[0].join("owned-output"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            spool.read_artifact(&artifact).unwrap(),
            b"rename failure source"
        );
    }

    #[test]
    fn export_copy_failure_reports_destination_and_anchor_provenance() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"copy failure original",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let retained = spool.root.join("artifacts").join(&artifact.digest);
        let destination = temp.path().join("archive");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *EXPORT_AFTER_INSTALL_BARRIER.lock().unwrap() =
            Some((destination.clone(), barrier.clone()));
        let source = spool.root.clone();
        let export_destination = destination.clone();
        let exporter = std::thread::spawn(move || export_spool_archive(source, export_destination));

        barrier.wait();
        fs::write(&retained, b"copy failure changed!").unwrap();
        barrier.wait();
        let result = exporter.join().unwrap();
        *EXPORT_AFTER_INSTALL_BARRIER.lock().unwrap() = None;

        assert!(matches!(
            result,
            Err(CaptureError::Conflict(message))
                if message.contains("copy or manifest creation failed")
                    && message.contains("owned export destination")
                    && message.contains("export anchor")
        ));
        assert!(destination.is_dir());
        assert!(!destination.join("capture-export-manifest-v1.json").exists());
        let anchors = export_anchors(temp.path());
        assert_eq!(anchors.len(), 1);
        assert_eq!(fs::read_dir(&anchors[0]).unwrap().count(), 0);
        assert_eq!(fs::read(retained).unwrap(), b"copy failure changed!");
    }

    #[test]
    fn export_copy_failure_reports_removed_anchor_as_location_unknown() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"removed anchor original",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let retained = spool.root.join("artifacts").join(&artifact.digest);
        let destination = temp.path().join("archive");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *EXPORT_AFTER_INSTALL_BARRIER.lock().unwrap() =
            Some((destination.clone(), barrier.clone()));
        let source = spool.root.clone();
        let export_destination = destination.clone();
        let exporter = std::thread::spawn(move || export_spool_archive(source, export_destination));

        barrier.wait();
        let anchors = export_anchors(temp.path());
        assert_eq!(anchors.len(), 1);
        fs::remove_dir(&anchors[0]).unwrap();
        fs::write(&retained, b"removed anchor changed!").unwrap();
        barrier.wait();
        let result = exporter.join().unwrap();
        *EXPORT_AFTER_INSTALL_BARRIER.lock().unwrap() = None;

        assert!(matches!(
            result,
            Err(CaptureError::Conflict(message))
                if message.contains("copy or manifest creation failed")
                    && message.contains("export anchor")
                    && message.contains("not currently observed at expected name")
                    && message.contains("current location and existence are unknown")
                    && message.contains("may have been moved or removed")
        ));
        assert!(!anchors[0].exists());
        assert!(destination.is_dir());
        assert!(!destination.join("capture-export-manifest-v1.json").exists());
        assert_eq!(fs::read(retained).unwrap(), b"removed anchor changed!");
    }

    #[test]
    fn export_destination_removed_before_reopen_is_reported_as_location_unknown() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"destination reopen source",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let destination = temp.path().join("archive");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *EXPORT_BEFORE_DESTINATION_REOPEN_BARRIER.lock().unwrap() =
            Some((destination.clone(), barrier.clone()));
        let source = spool.root.clone();
        let export_destination = destination.clone();
        let exporter = std::thread::spawn(move || export_spool_archive(source, export_destination));

        barrier.wait();
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
        fs::remove_dir(&destination).unwrap();
        barrier.wait();
        let result = exporter.join().unwrap();
        *EXPORT_BEFORE_DESTINATION_REOPEN_BARRIER.lock().unwrap() = None;

        assert!(matches!(
            result,
            Err(CaptureError::Conflict(message))
                if message.contains("destination reopen failed")
                    && message.contains("owned export destination")
                    && message.contains("not currently observed at expected name")
                    && message.contains("current location and existence are unknown")
                    && message.contains("may have been moved or removed")
        ));
        assert!(!destination.exists());
        let anchors = export_anchors(temp.path());
        assert_eq!(anchors.len(), 1);
        assert_eq!(fs::read_dir(&anchors[0]).unwrap().count(), 0);
        assert_eq!(
            spool.read_artifact(&artifact).unwrap(),
            b"destination reopen source"
        );
    }

    #[test]
    fn export_sync_failures_report_the_exact_retained_phase() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        for stage in [
            "destination parent after anchor creation",
            "export anchor after output creation",
            "export anchor after destination installation",
            "destination parent after destination installation",
        ] {
            let temp = tempfile::TempDir::new().unwrap();
            let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
            let artifact = spool
                .retain_artifact(
                    b"sync failure source",
                    ArtifactMediaV1::Binary,
                    RetentionClassV1::WorkLocal,
                )
                .unwrap();
            let destination = temp.path().join("archive");
            *FAIL_EXPORT_SYNC_FOR.lock().unwrap() = Some((destination.clone(), stage));
            let result = export_spool_archive(&spool.root, &destination);
            *FAIL_EXPORT_SYNC_FOR.lock().unwrap() = None;

            let message = match result {
                Err(CaptureError::Conflict(message)) => message,
                other => panic!("expected retained-output conflict, got {other:?}"),
            };
            let anchors = export_anchors(temp.path());
            assert_eq!(anchors.len(), 1);
            match stage {
                "destination parent after anchor creation" => {
                    assert!(
                        message.contains("identity/location, privacy, and contents are uncertain")
                    );
                    assert!(!destination.exists());
                    assert_eq!(fs::read_dir(&anchors[0]).unwrap().count(), 0);
                }
                "export anchor after output creation" => {
                    assert!(message.contains("owned child tree owned-output"));
                    assert!(!destination.exists());
                    let child = anchors[0].join("owned-output");
                    assert!(child.is_dir());
                    assert_eq!(fs::read_dir(child).unwrap().count(), 0);
                }
                "export anchor after destination installation"
                | "destination parent after destination installation" => {
                    assert!(message.contains("owned export destination"));
                    assert!(message.contains("export anchor"));
                    assert!(destination.is_dir());
                    assert!(!destination.join("capture-export-manifest-v1.json").exists());
                    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
                    assert_eq!(fs::read_dir(&anchors[0]).unwrap().count(), 0);
                }
                _ => unreachable!(),
            }
            assert_eq!(
                spool.read_artifact(&artifact).unwrap(),
                b"sync failure source"
            );
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn export_clears_an_inherited_macos_acl_before_creating_archive_content() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"acl inheritance source",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let destination_parent = temp.path().join("export-parent");
        fs::create_dir(&destination_parent).unwrap();
        fs::set_permissions(&destination_parent, fs::Permissions::from_mode(0o700)).unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg(
                "everyone allow read,write,execute,add_file,add_subdirectory,delete_child,file_inherit,directory_inherit",
            )
            .arg(&destination_parent)
            .status()
            .unwrap();
        assert!(status.success());
        let parent = open_private_directory(&destination_parent).unwrap();
        assert!(verify_export_acl_empty(&parent).is_err());

        let destination = destination_parent.join("archive");
        export_spool_archive(&spool.root, &destination).unwrap();
        let anchors = export_anchors(&destination_parent);
        assert_eq!(anchors.len(), 1);
        let artifacts = destination.join("artifacts");
        let exported_artifact = fs::read_dir(&artifacts)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        for directory in [&anchors[0], &destination, &artifacts] {
            assert_eq!(fs::metadata(directory).unwrap().mode() & 0o777, 0o700);
            let opened = open_private_directory(directory).unwrap();
            verify_export_acl_empty(&opened).unwrap();
        }
        assert_eq!(
            fs::metadata(&exported_artifact).unwrap().mode() & 0o777,
            0o600
        );
        let opened_artifact = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&exported_artifact)
            .unwrap();
        verify_export_acl_empty(&opened_artifact).unwrap();
        assert_eq!(
            spool.read_artifact(&artifact).unwrap(),
            b"acl inheritance source"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn export_first_parent_sync_failure_reports_raced_acl_anchor_as_uncertain() {
        let _serial = EXPORT_TEST_SERIAL.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"first sync ACL source",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let destination_parent = temp.path().join("export-parent");
        fs::create_dir(&destination_parent).unwrap();
        fs::set_permissions(&destination_parent, fs::Permissions::from_mode(0o700)).unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg(
                "everyone allow read,write,execute,add_file,add_subdirectory,delete_child,file_inherit,directory_inherit",
            )
            .arg(&destination_parent)
            .status()
            .unwrap();
        assert!(status.success());
        let destination = destination_parent.join("archive");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *EXPORT_AFTER_ANCHOR_CREATE_BARRIER.lock().unwrap() =
            Some((destination.clone(), barrier.clone()));
        *FAIL_EXPORT_SYNC_FOR.lock().unwrap() = Some((
            destination.clone(),
            "destination parent after anchor creation",
        ));
        let source = spool.root.clone();
        let export_destination = destination.clone();
        let exporter = std::thread::spawn(move || export_spool_archive(source, export_destination));

        barrier.wait();
        let anchors = export_anchors(&destination_parent);
        assert_eq!(anchors.len(), 1);
        let anchor = anchors[0].clone();
        let moved_anchor = destination_parent.join("original-anchor-moved");
        fs::rename(&anchor, &moved_anchor).unwrap();
        fs::create_dir(&anchor).unwrap();
        fs::set_permissions(&anchor, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(anchor.join("foreign.txt"), b"foreign ACL race bytes").unwrap();
        barrier.wait();
        let result = exporter.join().unwrap();
        *EXPORT_AFTER_ANCHOR_CREATE_BARRIER.lock().unwrap() = None;
        *FAIL_EXPORT_SYNC_FOR.lock().unwrap() = None;

        assert!(matches!(
            result,
            Err(CaptureError::Conflict(message))
                if message.contains("anchor parent sync failed")
                    && message.contains("current identity/location, privacy, and contents are uncertain")
        ));
        assert!(!destination.exists());
        assert_eq!(
            fs::read(anchor.join("foreign.txt")).unwrap(),
            b"foreign ACL race bytes"
        );
        assert_eq!(fs::metadata(&anchor).unwrap().mode() & 0o777, 0o755);
        assert_eq!(fs::read_dir(&moved_anchor).unwrap().count(), 0);
        assert_eq!(
            spool.read_artifact(&artifact).unwrap(),
            b"first sync ACL source"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn export_acl_failure_stops_before_child_and_reports_unverified_anchor() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let artifact = spool
            .retain_artifact(
                b"acl failure source",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .unwrap();
        let destination = temp.path().join("archive");
        FAIL_EXPORT_ACL_CLEAR.with(|failure| failure.set(true));
        let result = export_spool_archive(&spool.root, &destination);
        FAIL_EXPORT_ACL_CLEAR.with(|failure| failure.set(false));

        assert!(matches!(
            result,
            Err(CaptureError::Conflict(message))
                if message.contains("ACL clear failure")
                    && message.contains("opened export anchor")
                    && message.contains("privacy and contents are unverified")
        ));
        assert!(!destination.exists());
        let anchors = export_anchors(temp.path());
        assert_eq!(anchors.len(), 1);
        assert_eq!(fs::read_dir(&anchors[0]).unwrap().count(), 0);
        assert_eq!(
            spool.read_artifact(&artifact).unwrap(),
            b"acl failure source"
        );
    }

    #[test]
    fn legacy_evidence_arriving_before_first_mutation_prevents_v4_commit() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("spool");
        private_directory(&root).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        *INIT_BEFORE_MUTATION_BARRIER.lock().unwrap() = Some((root.clone(), barrier.clone()));
        // The fault seam belongs only to this root. Other concurrently executing spool tests and
        // real openers must never become accidental barrier participants.
        LocalSpool::open(temp.path().join("unrelated-spool")).unwrap();
        let open_root = root.clone();
        let opener = std::thread::spawn(move || LocalSpool::open(open_root));
        barrier.wait();
        let legacy = root.join("legacy-observation.json");
        fs::write(&legacy, b"retained legacy bytes").unwrap();
        barrier.wait();
        let result = opener.join().unwrap();
        *INIT_BEFORE_MUTATION_BARRIER.lock().unwrap() = None;
        assert!(matches!(result, Err(CaptureError::Conflict(_))));
        assert_eq!(fs::read(legacy).unwrap(), b"retained legacy bytes");
        assert!(!root.join("capture-spool-v4.json").exists());
        assert!(!root.join("spool.lock").exists());
        assert!(!root.join("managed-capture.lock").exists());
    }

    fn gap_envelope(
        operation_id: &str,
        observed_at: i64,
        reason: GapReasonV1,
    ) -> CaptureEnvelopeV1 {
        use crate::change_record::{
            AgentHarnessV1, CaptureBatchV1, CapturedEventKindV1, CapturedEventV1, GapFactV1,
            SessionIdentityV1,
        };
        CaptureEnvelopeV1::EventBatch(CaptureBatchV1 {
            schema_version: crate::change_record::SCHEMA_VERSION_V1,
            operation_id: operation_id.into(),
            work_id: "wrk_receipt".into(),
            generation: 1,
            session: SessionIdentityV1 {
                harness: AgentHarnessV1::ClaudeCode,
                adapter_version: "test".into(),
                session_id: "session_receipt".into(),
                parent_session_id: None,
                capture_mode: CaptureModeV1::MetadataOnly,
            },
            events: vec![CapturedEventV1 {
                event_id: "gap:receipt".into(),
                sequence: 1,
                observed_at,
                kind: CapturedEventKindV1::Gap,
                tool: None,
                artifact: None,
                setup: None,
                gap: Some(GapFactV1 {
                    first_missing: 1,
                    last_missing: 1,
                    reason,
                }),
                boundary: None,
                subagent: None,
            }],
        })
    }

    #[test]
    fn receipt_timed_priority_redelivery_in_outbox_ignores_only_observation_time() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let operation = format!("cap:{}", "a".repeat(64));
        let original = gap_envelope(&operation, 100, GapReasonV1::AdapterStartedLate);
        assert_eq!(
            spool.append_priority_at_receipt(&original).unwrap(),
            AppendOutcome::Stored
        );
        let later = gap_envelope(&operation, 200, GapReasonV1::AdapterStartedLate);
        assert_eq!(
            spool.append_priority_at_receipt(&later).unwrap(),
            AppendOutcome::Duplicate
        );
        let different = gap_envelope(&operation, 200, GapReasonV1::CaptureInterrupted);
        assert!(matches!(
            spool.append_priority_at_receipt(&different),
            Err(CaptureError::Conflict(_))
        ));
    }

    #[test]
    fn receipt_timed_redelivery_after_compaction_compares_retained_envelopes() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let digest_only = format!("cap:{}", "b".repeat(64));
        let replayable = format!("provider:history:{}:gap", "c".repeat(64));
        for operation in [&digest_only, &replayable] {
            let original = gap_envelope(operation, 100, GapReasonV1::AdapterStartedLate);
            spool.append_priority_at_receipt(&original).unwrap();
            spool.acknowledge(&original).unwrap();
        }
        spool.compact_acknowledged(10).unwrap();

        // Digest-only history keeps no bytes, so the operation id alone decides.
        let later = gap_envelope(&digest_only, 200, GapReasonV1::CaptureInterrupted);
        assert_eq!(
            spool.append_priority_at_receipt(&later).unwrap(),
            AppendOutcome::Duplicate
        );
        // Replayable history keeps the envelope, so only the observation time may differ.
        let later = gap_envelope(&replayable, 200, GapReasonV1::AdapterStartedLate);
        assert_eq!(
            spool.append_priority_at_receipt(&later).unwrap(),
            AppendOutcome::Duplicate
        );
        let different = gap_envelope(&replayable, 200, GapReasonV1::CaptureInterrupted);
        assert!(matches!(
            spool.append_priority_at_receipt(&different),
            Err(CaptureError::Conflict(_))
        ));
        // An untimed append still requires exact bytes.
        let retimed = gap_envelope(&replayable, 200, GapReasonV1::AdapterStartedLate);
        assert!(matches!(
            spool.append_priority(&retimed),
            Err(CaptureError::Conflict(_))
        ));
    }

    #[test]
    fn replaced_fixed_lock_inodes_never_split_live_callers() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let spool_guard = spool.lock().unwrap();
        fs::rename(
            spool.root.join("spool.lock"),
            spool.root.join("displaced-spool.lock"),
        )
        .unwrap();
        create_private_lock_file(&spool.root.join("spool.lock")).unwrap();
        assert!(matches!(spool.lock(), Err(CaptureError::Conflict(_))));
        drop(spool_guard);

        let managed_root = temp.path().join("managed-spool");
        let managed = LocalSpool::open(&managed_root).unwrap();
        let managed_guard = managed.lock_managed_capture().unwrap();
        fs::rename(
            managed.root.join("managed-capture.lock"),
            managed.root.join("displaced-managed.lock"),
        )
        .unwrap();
        create_private_lock_file(&managed.root.join("managed-capture.lock")).unwrap();
        assert!(matches!(
            managed.lock_managed_capture(),
            Err(CaptureError::Conflict(_))
        ));
        drop(managed_guard);
    }
}
