//! Repeated, explicit observation of one Git/native pair. Git remains authoritative.
//! Captured candidates and local mirror receipts never approve or move native targets.
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::authority::Authority;
use crate::git_export::{ExportRequest, ExportState, Mirror};
use crate::git_snapshot::{self, Preflight, SnapshotRequest};
use crate::local_recovery::AccessGuard;
use crate::model::TargetState;
use crate::store::{Engine, SourceReader};
use crate::{Error, Result};

pub const PROFILE: &str = "agentscm-git-reconciliation/v1";
const DATABASE: &str = "git-reconcile.sqlite";
const LOCK: &str = ".agentscm-git-reconcile.lock";
const MIRROR_INSTANCE: &str = "agentscm-reconcile-instance";
const APPLICATION: i64 = 0x4153_4752;
const MAX_RECORD: usize = 64 * 1024;
const MAX_REQUEST: usize = 16 * 1024;
const MAX_OPERATIONS: i64 = 10_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InitializeRequest {
    pub git: PathBuf,
    pub repository: PathBuf,
    pub target: String,
    pub git_commit: String,
    pub native_target: TargetState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObserveRequest {
    pub operation_id: String,
    pub expected_generation: u64,
    pub git_commit: String,
    pub native_target: TargetState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExitRequest {
    pub operation_id: String,
    pub expected_generation: u64,
    pub git_commit: String,
    pub native_target: TargetState,
    pub destination: PathBuf,
    pub export: ExportRequest,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Synchronized,
    GitChanged,
    NativeChanged,
    Conflict,
    Unknown,
    ExitPrepared,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub git_commit: String,
    pub native_target: TargetState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub git_commit: String,
    pub git_tree: String,
    pub git_snapshot: String,
    pub native_target: TargetState,
    pub native_tree: String,
    /// A file-set change parented on the verified native baseline, not Git history.
    pub candidate_revision: Option<String>,
    /// Actual native ancestry contains this captured candidate; no message matching.
    pub candidate_incorporated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExitReceipt {
    pub event_sequence: u64,
    pub native_target: TargetState,
    pub commit: String,
    pub tree: String,
    pub destination: PathBuf,
}

/// Retained historical observations. Reading this is never a fresh synchronization check.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub profile: String,
    pub repository: String,
    pub target: String,
    pub source_authority: String,
    pub observation_freshness: String,
    pub generation: u64,
    pub state: Phase,
    pub baseline: Baseline,
    pub observation: Option<Observation>,
    pub active_operation: Option<String>,
    pub exit: Option<ExitReceipt>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationStatus {
    Missing,
    Pending {
        operation_id: String,
        generation: u64,
    },
    Complete {
        operation_id: String,
        result: Box<Status>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Configuration {
    data: PathBuf,
    git: PathBuf,
    source: PathBuf,
    git_dir: PathBuf,
    repository: String,
    target: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    configuration: Configuration,
    status: Status,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Request {
    Observe(ObserveRequest),
    PrepareExit(ExitRequest),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct MirrorIdentity {
    device: u64,
    inode: u64,
    instance: String,
}

impl Request {
    fn id(&self) -> &str {
        match self {
            Self::Observe(r) => &r.operation_id,
            Self::PrepareExit(r) => &r.operation_id,
        }
    }
    fn generation(&self) -> u64 {
        match self {
            Self::Observe(r) => r.expected_generation,
            Self::PrepareExit(r) => r.expected_generation,
        }
    }
}

/// The connection closes before its shared data lease is released.
pub struct Reconciler {
    database: Connection,
    configuration: Configuration,
    _access: AccessGuard,
}

/// Pins a known journal generation against cooperating reconciliation changes.
/// It is not a native target lock, authorization proof or editor/host fence.
pub struct CurrentObservation<'a> {
    status: Status,
    _lock: crate::lease::Lease,
    _owner: &'a Reconciler,
}

impl CurrentObservation<'_> {
    pub fn status(&self) -> &Status {
        &self.status
    }
    pub fn data_dir(&self) -> &Path {
        &self._owner.configuration.data
    }
}

impl Reconciler {
    pub fn initialize(data: &Path, request: &InitializeRequest) -> Result<Self> {
        let data = data.canonicalize()?;
        let access = AccessGuard::shared(&data)?;
        let authority = Authority::open_read_only(&data)?;
        check_target(&authority, &request.target, &request.native_target)?;
        let snapshot_request = SnapshotRequest {
            git: request.git.clone(),
            repository: request.repository.clone(),
            revision: request.git_commit.clone(),
            exclude: Vec::new(),
        };
        let report = git_snapshot::preflight(&snapshot_request)?;
        let source = Path::new(&report.repository).canonicalize()?;
        let git_dir = Path::new(&report.git_dir).canonicalize()?;
        for root in [&source, &git_dir] {
            separate(&data, root)?;
        }
        if !report.eligible {
            return Err(Error::Conflict(
                "Git tree is not eligible for complete reconciliation".into(),
            ));
        }
        require_regular_report(&report)?;
        let configuration = Configuration {
            data: data.clone(),
            git: request.git.clone(),
            source,
            git_dir,
            repository: authority.repository_id()?,
            target: request.target.clone(),
        };
        let engine = open_engine(&data)?;
        let snapshot = git_snapshot::capture(
            &engine,
            &configuration.repository,
            &snapshot_request,
            report,
        )?;
        checked_report(&configuration, &request.git_commit)?;
        check_target(&authority, &configuration.target, &request.native_target)?;
        if engine.load_revision(&snapshot.revision)?.tree
            != engine.load_revision(&request.native_target.revision)?.tree
        {
            return Err(Error::Conflict(
                "initial Git tree and native target differ".into(),
            ));
        }
        // Initialization never adopts an existing or interrupted journal.
        let lock = File::create_new(data.join(LOCK))?;
        lock.sync_all()?;
        File::create_new(data.join(DATABASE))?.sync_all()?;
        let database =
            Connection::open_with_flags(data.join(DATABASE), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        database.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA application_id=1095976786; PRAGMA user_version=1; BEGIN IMMEDIATE; CREATE TABLE state(singleton INTEGER PRIMARY KEY CHECK(singleton=1),record BLOB NOT NULL) STRICT; CREATE TABLE operations(id TEXT PRIMARY KEY,request BLOB NOT NULL,before BLOB NOT NULL,result BLOB,binding BLOB) STRICT; COMMIT;")?;
        let status = Status {
            profile: PROFILE.into(),
            repository: configuration.repository.clone(),
            target: configuration.target.clone(),
            source_authority: "git".into(),
            observation_freshness: "historical".into(),
            generation: 0,
            state: Phase::Synchronized,
            baseline: Baseline {
                git_commit: request.git_commit.clone(),
                native_target: request.native_target.clone(),
            },
            observation: None,
            active_operation: None,
            exit: None,
            error: None,
        };
        database.execute(
            "INSERT INTO state(singleton,record) VALUES(1,?1)",
            [encode(
                &Record {
                    configuration: configuration.clone(),
                    status,
                },
                MAX_RECORD,
            )?],
        )?;
        File::open(&data)?.sync_all()?;
        Ok(Self {
            database,
            configuration,
            _access: access,
        })
    }

    pub fn open(data: &Path) -> Result<Self> {
        let data = data.canonicalize()?;
        let access = AccessGuard::shared(&data)?;
        let database = open_database(&data, false)?;
        let record = read_record(&database)?;
        if record.configuration.data != data {
            return Err(Error::Conflict("restored reconciliation journal requires explicit external-state reconciliation; original data binding differs".into()));
        }
        let authority = Authority::open_read_only(&data)?;
        if authority.repository_id()? != record.configuration.repository {
            return Err(Error::Conflict(
                "reconciliation authority identity differs".into(),
            ));
        }
        Ok(Self {
            database,
            configuration: record.configuration,
            _access: access,
        })
    }

    pub fn status(&self) -> Result<Status> {
        Ok(read_record(&self.database)?.status)
    }

    pub fn current(&self, expected_generation: u64) -> Result<CurrentObservation<'_>> {
        let lock = self.lock()?;
        let status = self.status()?;
        if status.generation != expected_generation
            || status.active_operation.is_some()
            || status.state == Phase::Unknown
        {
            return Err(Error::Conflict(
                "reconciliation observation is stale or uncertain".into(),
            ));
        }
        let authority = Authority::open_read_only(&self.configuration.data)?;
        if authority.repository_id()? != self.configuration.repository {
            return Err(Error::Conflict(
                "native authority differs from reconciliation".into(),
            ));
        }
        let target = status
            .observation
            .as_ref()
            .map(|observation| &observation.native_target)
            .unwrap_or(&status.baseline.native_target);
        check_target(&authority, &self.configuration.target, target)?;
        Ok(CurrentObservation {
            status,
            _lock: lock,
            _owner: self,
        })
    }

    pub fn observe(&mut self, request: &ObserveRequest) -> Result<Status> {
        let _lock = self.lock()?;
        let fixed = Request::Observe(request.clone());
        let (before, completed) = self.begin(&fixed, None)?;
        if let Some(result) = completed {
            return Ok(result);
        }
        match self.collect(
            &request.git_commit,
            &request.native_target,
            &before.baseline,
        ) {
            Ok(observation) => {
                let mut status = pending(&before, request.operation_id.clone())?;
                let git_changed = observation.git_commit != before.baseline.git_commit;
                let native_changed = observation.native_target != before.baseline.native_target;
                let engine = open_engine(&self.configuration.data)?;
                let equal = engine.load_revision(&observation.git_snapshot)?.tree
                    == observation.native_tree;
                status.state = if equal {
                    Phase::Synchronized
                } else if observation.candidate_incorporated {
                    Phase::NativeChanged
                } else if git_changed && native_changed {
                    Phase::Conflict
                } else if git_changed {
                    Phase::GitChanged
                } else if native_changed {
                    Phase::NativeChanged
                } else {
                    Phase::Conflict
                };
                if equal {
                    status.baseline = Baseline {
                        git_commit: observation.git_commit.clone(),
                        native_target: observation.native_target.clone(),
                    };
                }
                status.observation = Some(observation);
                status.active_operation = None;
                self.complete(&fixed, &status)?;
                Ok(status)
            }
            Err(error) => {
                // Capture only publishes immutable objects. A failed observation
                // is retained as unknown; a new explicit observation may follow.
                let mut status = pending(&before, request.operation_id.clone())?;
                status.active_operation = None;
                status.error = Some(excerpt(&error));
                self.complete(&fixed, &status)?;
                Err(error)
            }
        }
    }

    /// Prepare a verified local Git mirror only. A separately explicit workflow
    /// decision owns disconnect, outstanding changes and source-authority exit.
    pub fn prepare_exit(&mut self, request: &ExitRequest) -> Result<Status> {
        let _lock = self.lock()?;
        if !request.destination.is_absolute() {
            return Err(Error::Invalid("exit destination must be absolute".into()));
        }
        let fixed = Request::PrepareExit(request.clone());
        validate_operation(&fixed)?;
        // Resolve completed retry receipts before requiring a current clean pair.
        let retained = read_operation(&self.database, fixed.id())?;
        if let Some((stored, _, Some(result), _)) = &retained {
            if stored != &fixed {
                return Err(Error::Conflict(
                    "operation ID has a different fixed request".into(),
                ));
            }
            return Ok(result.clone());
        }
        let destination = request.destination.canonicalize()?;
        if destination != request.destination {
            return Err(Error::Conflict(
                "exit destination must retain its exact canonical path".into(),
            ));
        }
        for root in [
            &self.configuration.data,
            &self.configuration.source,
            &self.configuration.git_dir,
        ] {
            separate(root, &destination)?;
        }
        // Reject invalid NEW handoffs before reserving the journal. For pending
        // retries use the original before-state, since the current row is unknown.
        let before = if let Some((stored, before, _, _)) = &retained {
            if stored != &fixed {
                return Err(Error::Conflict(
                    "operation ID has a different fixed request".into(),
                ));
            }
            before.clone()
        } else {
            self.status()?
        };
        if before.state != Phase::Synchronized
            || before.baseline.git_commit != request.git_commit
            || before.baseline.native_target != request.native_target
        {
            return Err(Error::Conflict(
                "prepare-exit requires the exact last synchronized pair".into(),
            ));
        }
        let mut mirror = Mirror::open(
            &self.configuration.git,
            &destination,
            &self.configuration.repository,
        )?;
        let authority = Authority::open_read_only(&self.configuration.data)?;
        check_target(
            &authority,
            &self.configuration.target,
            &request.native_target,
        )?;
        let sequence = request.export.event_sequence;
        if sequence == 0 {
            return Err(Error::Invalid(
                "exit needs a positive integration event".into(),
            ));
        }
        let event = authority
            .events(sequence - 1, 1)?
            .into_iter()
            .next()
            .filter(|event| {
                event.sequence == sequence
                    && event.target == self.configuration.target
                    && event.after == request.native_target
            })
            .ok_or_else(|| {
                Error::Conflict("exit event does not match the actual native target".into())
            })?;
        if let Some(existing) = mirror.status(sequence)?
            && (existing.record.request != request.export || existing.record.event != event)
        {
            return Err(Error::Conflict(
                "mirror event already has another fixed request".into(),
            ));
        }
        // Mirror::open validates ownership and retains the shared recovery lease.
        // Capture identity only under that lease. Pending retries never create or
        // adopt missing marker state, even if this path now owns a valid mirror.
        let identity = mirror_identity(&destination, retained.is_none())?;
        if let Some((_, _, _, binding)) = &retained
            && binding.as_ref() != Some(&identity)
        {
            return Err(Error::Conflict(
                "pending exit mirror instance changed; retain both copies and reconcile explicitly"
                    .into(),
            ));
        }
        let (before, completed) = self.begin(&fixed, Some(&identity))?;
        if let Some(result) = completed {
            return Ok(result);
        }
        let result = (|| {
            let observation = self.collect(
                &request.git_commit,
                &request.native_target,
                &before.baseline,
            )?;
            let engine = open_engine(&self.configuration.data)?;
            if engine.load_revision(&observation.git_snapshot)?.tree != observation.native_tree {
                return Err(Error::Conflict(
                    "Git/native trees differ; explicit reconciliation is required".into(),
                ));
            }
            let exported = mirror.export(&engine, &authority, &request.export)?;
            if exported.record.state != ExportState::Mirrored
                || !exported.ref_matches
                || exported.object_error.is_some()
            {
                return Err(Error::Conflict("mirror export is not verified".into()));
            }
            // The external source and native target can move independently of
            // the mirror CAS. A changed pair cannot become a completed handoff.
            checked_report(&self.configuration, &request.git_commit)?;
            check_target(
                &authority,
                &self.configuration.target,
                &request.native_target,
            )?;
            let mut status = pending(&before, request.operation_id.clone())?;
            status.state = Phase::ExitPrepared;
            status.active_operation = None;
            status.observation = Some(observation);
            status.exit = Some(ExitReceipt {
                event_sequence: sequence,
                native_target: request.native_target.clone(),
                commit: exported
                    .record
                    .commit
                    .ok_or_else(|| Error::Invalid("export has no commit".into()))?,
                tree: exported
                    .record
                    .tree
                    .ok_or_else(|| Error::Invalid("export has no tree".into()))?,
                destination,
            });
            self.complete(&fixed, &status)?;
            Ok(status)
        })();
        if let Err(error) = &result {
            let mut status = pending(&before, request.operation_id.clone())?;
            status.error = Some(excerpt(error));
            // Without a retained export row, the exporter could not have moved
            // its ref. Otherwise keep the exact pending operation for recovery.
            if matches!(mirror.status(sequence), Ok(None)) {
                status.active_operation = None;
                self.complete(&fixed, &status)?;
            } else {
                self.save(&status)?;
            }
        }
        result
    }

    fn collect(
        &self,
        commit: &str,
        target: &TargetState,
        baseline: &Baseline,
    ) -> Result<Observation> {
        let authority = Authority::open_read_only(&self.configuration.data)?;
        if authority.repository_id()? != self.configuration.repository {
            return Err(Error::Conflict("native repository changed".into()));
        }
        check_target(&authority, &self.configuration.target, target)?;
        let report = checked_report(&self.configuration, commit)?;
        let git_tree = report.tree.clone();
        let request = SnapshotRequest {
            git: self.configuration.git.clone(),
            repository: self.configuration.source.clone(),
            revision: commit.into(),
            exclude: Vec::new(),
        };
        let engine = open_engine(&self.configuration.data)?;
        let snapshot =
            git_snapshot::capture(&engine, &self.configuration.repository, &request, report)?;
        let tree = engine.load_revision(&snapshot.revision)?.tree;
        let baseline_tree = engine.load_revision(&baseline.native_target.revision)?.tree;
        let candidate_revision = if tree != baseline_tree {
            Some(candidate_from_snapshot(
                &engine,
                &baseline.native_target.revision,
                &snapshot.revision,
            )?)
        } else {
            None
        };
        let native_tree = engine.load_revision(&target.revision)?.tree;
        let candidate_incorporated = candidate_revision
            .as_ref()
            .map(|candidate| contains_revision(&engine, &target.revision, candidate))
            .transpose()?
            .unwrap_or(false);
        checked_report(&self.configuration, commit)?;
        check_target(&authority, &self.configuration.target, target)?;
        Ok(Observation {
            git_commit: commit.into(),
            git_tree,
            git_snapshot: snapshot.revision,
            native_target: target.clone(),
            native_tree,
            candidate_revision,
            candidate_incorporated,
        })
    }

    fn lock(&self) -> Result<crate::lease::Lease> {
        let path = self.configuration.data.join(LOCK);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(Error::Invalid("reconciliation lock must be regular".into()));
        }
        crate::lease::Lease::try_exclusive(
            fs::OpenOptions::new().read(true).write(true).open(path)?,
        )
        .map_err(|_| Error::Conflict("reconciliation is already active".into()))
    }

    fn begin(
        &mut self,
        request: &Request,
        binding: Option<&MirrorIdentity>,
    ) -> Result<(Status, Option<Status>)> {
        validate_operation(request)?;
        let tx = self
            .database
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_record(&tx)?;
        if current.configuration != self.configuration {
            return Err(Error::Conflict("reconciliation owner changed".into()));
        }
        if let Some((stored, before, result, stored_binding)) = read_operation(&tx, request.id())? {
            if stored != *request {
                return Err(Error::Conflict(
                    "operation ID has a different fixed request".into(),
                ));
            }
            if result.is_none() && stored_binding.as_ref() != binding {
                return Err(Error::Conflict("operation mirror binding differs".into()));
            }
            if result.is_none() && current.status.active_operation.as_deref() != Some(request.id())
            {
                return Err(Error::Conflict(
                    "pending operation no longer owns the journal".into(),
                ));
            }
            return Ok((before, result));
        }
        if current.status.generation != request.generation()
            || current.status.active_operation.is_some()
            || current.status.state == Phase::ExitPrepared
        {
            return Err(Error::Conflict(
                "stale generation, pending operation or prepared exit; inspect status".into(),
            ));
        }
        let count: i64 = tx.query_row("SELECT count(*) FROM operations", [], |row| row.get(0))?;
        if count >= MAX_OPERATIONS {
            return Err(Error::Limit(
                "reconciliation operation journal is full".into(),
            ));
        }
        let next = pending(&current.status, request.id().into())?;
        tx.execute(
            "INSERT INTO operations(id,request,before,result,binding) VALUES(?1,?2,?3,NULL,?4)",
            params![
                request.id(),
                encode(request, MAX_REQUEST)?,
                encode(&current.status, MAX_RECORD)?,
                binding.map(|identity| encode(identity, 1024)).transpose()?
            ],
        )?;
        write_record(
            &tx,
            &Record {
                configuration: self.configuration.clone(),
                status: next,
            },
        )?;
        tx.commit()?;
        Ok((current.status, None))
    }

    fn save(&self, status: &Status) -> Result<()> {
        write_record(
            &self.database,
            &Record {
                configuration: self.configuration.clone(),
                status: status.clone(),
            },
        )
    }

    fn complete(&mut self, request: &Request, status: &Status) -> Result<()> {
        let tx = self
            .database
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_record(&tx)?.status.active_operation.as_deref() != Some(request.id()) {
            return Err(Error::Conflict("operation lost journal ownership".into()));
        }
        tx.execute(
            "UPDATE operations SET result=?1 WHERE id=?2 AND result IS NULL",
            params![encode(status, MAX_RECORD)?, request.id()],
        )?;
        write_record(
            &tx,
            &Record {
                configuration: self.configuration.clone(),
                status: status.clone(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// No SQLite creation, schema migration, source capture or Git subprocess.
/// The shared lease may use the existing local coordination lock file.
pub fn read_status(data: &Path) -> Result<Status> {
    let _access = AccessGuard::shared_existing(data)?;
    Ok(read_record(&open_database(data, true)?)?.status)
}

pub fn lookup(data: &Path, operation_id: &str) -> Result<OperationStatus> {
    identifier(operation_id)?;
    let _access = AccessGuard::shared_existing(data)?;
    let database = open_database(data, true)?;
    match read_operation(&database, operation_id)? {
        None => Ok(OperationStatus::Missing),
        Some((_, before, None, _)) => Ok(OperationStatus::Pending {
            operation_id: operation_id.into(),
            generation: next(before.generation)?,
        }),
        Some((_, _, Some(result), _)) => Ok(OperationStatus::Complete {
            operation_id: operation_id.into(),
            result: Box::new(result),
        }),
    }
}

fn pending(before: &Status, operation: String) -> Result<Status> {
    let mut status = before.clone();
    status.generation = next(before.generation)?;
    status.state = Phase::Unknown;
    status.observation = None;
    status.active_operation = Some(operation);
    status.exit = None;
    status.error = None;
    Ok(status)
}

fn next(generation: u64) -> Result<u64> {
    generation
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or_else(|| Error::Limit("reconciliation generation exhausted".into()))
}

fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
    {
        return Err(Error::Invalid("invalid reconciliation identifier".into()));
    }
    Ok(())
}

fn oid(value: &str) -> Result<()> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return Err(Error::Invalid(
            "expected complete lowercase Git object ID".into(),
        ));
    }
    Ok(())
}

fn validate_target(target: &TargetState) -> Result<()> {
    crate::store::validate_id(&target.revision)?;
    if target.version == 0 || target.version > i64::MAX as u64 {
        return Err(Error::Invalid("invalid native target generation".into()));
    }
    Ok(())
}

fn validate_operation(request: &Request) -> Result<()> {
    identifier(request.id())?;
    next(request.generation())?;
    let (commit, target) = match request {
        Request::Observe(r) => (&r.git_commit, &r.native_target),
        Request::PrepareExit(r) => (&r.git_commit, &r.native_target),
    };
    oid(commit)?;
    validate_target(target)?;
    encode(request, MAX_REQUEST)?;
    Ok(())
}

fn check_target(authority: &Authority, name: &str, expected: &TargetState) -> Result<()> {
    identifier(name)?;
    validate_target(expected)?;
    if authority.target(name)?.as_ref() != Some(expected) {
        return Err(Error::Conflict(
            "actual native target differs from fixed request".into(),
        ));
    }
    Ok(())
}

fn separate(a: &Path, b: &Path) -> Result<()> {
    if a.starts_with(b) || b.starts_with(a) {
        return Err(Error::Invalid(
            "Git source, mirror and native data must be separate non-nested roots".into(),
        ));
    }
    Ok(())
}

fn checked_report(configuration: &Configuration, commit: &str) -> Result<Preflight> {
    let report = git_snapshot::preflight(&SnapshotRequest {
        git: configuration.git.clone(),
        repository: configuration.source.clone(),
        revision: commit.into(),
        exclude: Vec::new(),
    })?;
    if !report.eligible
        || !report.excluded.is_empty()
        || Path::new(&report.repository).canonicalize()? != configuration.source
        || Path::new(&report.git_dir).canonicalize()? != configuration.git_dir
    {
        return Err(Error::Conflict(
            "Git source is dirty, unsupported or changed repository identity".into(),
        ));
    }
    require_regular_report(&report)?;
    Ok(report)
}

/// Reconciliation exports regular files only, so a tree capturing an
/// executable or symlink entry is refused.
fn require_regular_report(report: &Preflight) -> Result<()> {
    if report.profile != git_snapshot::PROFILE {
        return Err(Error::Invalid(
            "unsupported entry kind: Git reconciliation preserves regular files only".into(),
        ));
    }
    Ok(())
}

fn open_engine(data: &Path) -> Result<Engine> {
    let metadata = fs::symlink_metadata(data.join("objects"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid(
            "existing native object store required".into(),
        ));
    }
    Engine::open(data)
}

fn open_database(data: &Path, read_only: bool) -> Result<Connection> {
    let path = data.join(DATABASE);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid(
            "reconciliation journal must be an existing regular file".into(),
        ));
    }
    let database = Connection::open_with_flags(
        path,
        if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        },
    )?;
    if database.pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))?
        != APPLICATION
        || database.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))? != 1
    {
        return Err(Error::Invalid("unsupported reconciliation journal".into()));
    }
    if !read_only {
        database.pragma_update(None, "synchronous", "FULL")?;
    }
    Ok(database)
}

fn encode<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > limit {
        return Err(Error::Limit("reconciliation record exceeds bound".into()));
    }
    Ok(bytes)
}

fn decode<T: serde::de::DeserializeOwned>(bytes: Vec<u8>, limit: usize) -> Result<T> {
    if bytes.len() > limit {
        return Err(Error::Limit("reconciliation record exceeds bound".into()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_record(database: &Connection) -> Result<Record> {
    let size: i64 = database.query_row(
        "SELECT length(record) FROM state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    if size < 0 || size > MAX_RECORD as i64 {
        return Err(Error::Limit("reconciliation record exceeds bound".into()));
    }
    let record: Record = decode(
        database.query_row("SELECT record FROM state WHERE singleton=1", [], |row| {
            row.get(0)
        })?,
        MAX_RECORD,
    )?;
    validate_status(&record.status, &record.configuration.repository)?;
    if record.configuration.target != record.status.target
        || !record.configuration.data.is_absolute()
        || !record.configuration.git.is_absolute()
        || !record.configuration.source.is_absolute()
        || !record.configuration.git_dir.is_absolute()
    {
        return Err(Error::Invalid(
            "invalid reconciliation owner binding".into(),
        ));
    }
    Ok(record)
}

fn write_record(database: &Connection, record: &Record) -> Result<()> {
    database.execute(
        "UPDATE state SET record=?1 WHERE singleton=1",
        [encode(record, MAX_RECORD)?],
    )?;
    Ok(())
}

type Operation = (Request, Status, Option<Status>, Option<MirrorIdentity>);
type EncodedOperation = (Vec<u8>, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);
fn read_operation(database: &Connection, id: &str) -> Result<Option<Operation>> {
    let sizes: Option<(i64,i64,Option<i64>,Option<i64>)> = database.query_row("SELECT length(request),length(before),length(result),length(binding) FROM operations WHERE id=?1",[id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    let Some((request, before, result, binding)) = sizes else {
        return Ok(None);
    };
    if request > MAX_REQUEST as i64
        || before > MAX_RECORD as i64
        || result.is_some_and(|size| size > MAX_RECORD as i64)
        || binding.is_some_and(|size| size > 1024)
    {
        return Err(Error::Limit(
            "reconciliation operation exceeds bound".into(),
        ));
    }
    let (request, before, result, binding): EncodedOperation = database.query_row(
        "SELECT request,before,result,binding FROM operations WHERE id=?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let request: Request = decode(request, MAX_REQUEST)?;
    validate_operation(&request)?;
    if request.id() != id {
        return Err(Error::Invalid(
            "operation key differs from retained request".into(),
        ));
    }
    let binding: Option<MirrorIdentity> = binding.map(|bytes| decode(bytes, 1024)).transpose()?;
    if let Some(binding) = &binding {
        validate_instance(&binding.instance)?;
    }
    if matches!(&request, Request::Observe(_)) && binding.is_some()
        || matches!(&request, Request::PrepareExit(_)) && result.is_none() && binding.is_none()
    {
        return Err(Error::Invalid(
            "missing or invalid reconciliation mirror binding".into(),
        ));
    }
    Ok(Some((
        request,
        decode(before, MAX_RECORD)?,
        result.map(|bytes| decode(bytes, MAX_RECORD)).transpose()?,
        binding,
    )))
}

fn validate_status(status: &Status, repository: &str) -> Result<()> {
    if status.profile != PROFILE
        || status.repository != repository
        || repository.len() != 32
        || !repository
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
        || status.source_authority != "git"
        || status.observation_freshness != "historical"
        || status.generation > i64::MAX as u64
    {
        return Err(Error::Invalid(
            "invalid reconciliation status binding".into(),
        ));
    }
    identifier(&status.target)?;
    oid(&status.baseline.git_commit)?;
    validate_target(&status.baseline.native_target)?;
    if let Some(id) = &status.active_operation {
        identifier(id)?;
        if status.state != Phase::Unknown {
            return Err(Error::Invalid(
                "pending reconciliation is not unknown".into(),
            ));
        }
    }
    if let Some(observation) = &status.observation {
        oid(&observation.git_commit)?;
        oid(&observation.git_tree)?;
        crate::store::validate_id(&observation.git_snapshot)?;
        crate::store::validate_id(&observation.native_tree)?;
        validate_target(&observation.native_target)?;
        if let Some(candidate) = &observation.candidate_revision {
            crate::store::validate_id(candidate)?;
        }
    }
    if let Some(exit) = &status.exit {
        validate_target(&exit.native_target)?;
        oid(&exit.commit)?;
        oid(&exit.tree)?;
        if status.state != Phase::ExitPrepared
            || !exit.destination.is_absolute()
            || exit.event_sequence == 0
        {
            return Err(Error::Invalid("invalid prepared Git exit".into()));
        }
    } else if status.state == Phase::ExitPrepared {
        return Err(Error::Invalid("prepared exit has no receipt".into()));
    }
    if status
        .error
        .as_ref()
        .is_some_and(|error| error.len() > 4096)
    {
        return Err(Error::Limit("reconciliation error too large".into()));
    }
    Ok(())
}

fn excerpt(error: &Error) -> String {
    error.to_string().chars().take(1024).collect()
}

// Caller must retain the validated Mirror and its recovery lease throughout.
fn mirror_identity(destination: &Path, create: bool) -> Result<MirrorIdentity> {
    let owner = fs::symlink_metadata(destination.join("agentscm-mirror.json"))?;
    if !owner.is_file() || owner.file_type().is_symlink() {
        return Err(Error::Invalid(
            "mirror ownership file must be regular".into(),
        ));
    }
    let path = destination.join(MIRROR_INSTANCE);
    if create {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                let mut random = [0u8; 16];
                getrandom::fill(&mut random)
                    .map_err(|_| Error::Invalid("mirror instance randomness unavailable".into()))?;
                let instance: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
                file.write_all(instance.as_bytes())?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::Invalid(
            "mirror instance marker must be regular".into(),
        ));
    }
    let mut bytes = Vec::new();
    (&file).take(33).read_to_end(&mut bytes)?;
    let instance = String::from_utf8(bytes)
        .map_err(|_| Error::Invalid("invalid mirror instance marker".into()))?;
    validate_instance(&instance)?;
    if create {
        // An earlier interrupted creation may have written a complete marker but
        // not synced its directory. Establish durability before reserving work.
        file.sync_all()?;
        File::open(destination)?.sync_all()?;
    }
    Ok(MirrorIdentity {
        device: owner.dev(),
        inode: owner.ino(),
        instance,
    })
}

fn validate_instance(instance: &str) -> Result<()> {
    if instance.len() != 32
        || !instance
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(Error::Invalid("invalid mirror instance marker".into()));
    }
    Ok(())
}

fn contains_revision(engine: &Engine, head: &str, wanted: &str) -> Result<bool> {
    let mut pending = vec![head.to_owned()];
    let mut seen = BTreeSet::new();
    while let Some(revision) = pending.pop() {
        if !seen.insert(revision.clone()) {
            continue;
        }
        if seen.len() > crate::history::MAX_HISTORY_REVISIONS {
            return Err(Error::Limit(
                "reconciliation native ancestry exceeds bound".into(),
            ));
        }
        if revision == wanted {
            return Ok(true);
        }
        pending.extend(engine.load_revision(&revision)?.parents);
    }
    Ok(false)
}

fn candidate_from_snapshot(engine: &Engine, baseline: &str, snapshot: &str) -> Result<String> {
    let mut cursor = None;
    let mut changes = Vec::new();
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    loop {
        let page = engine.diff_page(
            baseline,
            snapshot,
            cursor.as_deref(),
            crate::store::MAX_DIFF_PAGE,
        )?;
        if let Some(missing) = page.needed_objects.first() {
            return Err(Error::MissingObject(missing.clone()));
        }
        for entry in page.entries {
            if changes.len() >= 200_000 {
                return Err(Error::Limit(
                    "reconciliation delta exceeds complete-tree bounds".into(),
                ));
            }
            if entry.after.is_some() {
                // A written path keeps its existing kind, so a non-regular
                // baseline or snapshot entry cannot be reconciled as 100644.
                for kind in [entry.before_kind, entry.after_kind].into_iter().flatten() {
                    crate::store::require_regular_entry("Git reconciliation", &entry.path, kind)?;
                }
            }
            let content = entry
                .after
                .as_ref()
                .map(|_| engine.read_file(snapshot, &entry.path))
                .transpose()?;
            let size = content.as_ref().map_or(0, Vec::len);
            bytes = bytes
                .checked_add(size)
                .ok_or_else(|| Error::Limit("reconciliation content overflow".into()))?;
            if bytes > crate::store::MAX_CHECKPOINT_BYTES {
                return Err(Error::Limit(
                    "reconciliation content exceeds snapshot bound".into(),
                ));
            }
            entries.push(git_snapshot::ManifestEntry {
                path: entry.path.clone(),
                mode: "100644".into(),
                oid: entry.after.unwrap_or_default(),
                size: size as u64,
            });
            changes.push(crate::model::FileChange {
                path: entry.path,
                content,
            });
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let plan = git_snapshot::plan_batches(
        &entries,
        crate::store::MAX_CHANGES,
        crate::store::MAX_CHECKPOINT_PREFIXES,
        crate::store::MAX_CHECKPOINT_BYTES as u64,
    );
    // R1 checkpoints support exactly one parent. R2 merge publication requires
    // at least two real parents and is deliberately not used for this file-set delta.
    let candidate = engine.checkpoint_captured_batches(
        baseline,
        snapshot,
        plan.iter().map(|range| &changes[range.clone()]),
    )?;
    if engine.load_revision(&candidate)?.tree != engine.load_revision(snapshot)?.tree {
        return Err(Error::Conflict(
            "captured candidate differs from complete Git tree".into(),
        ));
    }
    Ok(candidate)
}

/// Typed backup validation; no Git calls, external writes or authority adoption.
pub(crate) fn validate_recovery(
    database: &Connection,
    repository: &str,
    source: &SourceReader<'_>,
) -> Result<()> {
    let record = read_record(database)?;
    validate_status(&record.status, repository)?;
    let mut roots = BTreeSet::new();
    let mut pending_operation = None;
    collect_roots(&record.status, &mut roots);
    let mut rows = database.prepare("SELECT id FROM operations ORDER BY id LIMIT 10001")?;
    for (index, id) in rows
        .query_map([], |row| row.get::<_, String>(0))?
        .enumerate()
    {
        if index >= MAX_OPERATIONS as usize {
            return Err(Error::Limit(
                "reconciliation journal exceeds operation bound".into(),
            ));
        }
        let id = id?;
        identifier(&id)?;
        let (request, before, result, _) = read_operation(database, &id)?
            .ok_or_else(|| Error::NotFound("reconciliation operation".into()))?;
        validate_status(&before, repository)?;
        if before.generation != request.generation() || before.active_operation.is_some() {
            return Err(Error::Invalid(
                "invalid operation baseline generation".into(),
            ));
        }
        collect_roots(&before, &mut roots);
        // Fixed requests are untrusted intentions. In particular, a failed
        // observation may name an absent revision; only confirmed state is a
        // retained source root, never a fabricated successful capture.
        if let Some(result) = result {
            validate_status(&result, repository)?;
            if result.generation != next(before.generation)? || result.active_operation.is_some() {
                return Err(Error::Invalid(
                    "invalid reconciliation operation result".into(),
                ));
            }
            collect_roots(&result, &mut roots);
        } else {
            let mut expected = pending(&before, id.clone())?;
            // A failed uncertain export may retain a bounded diagnostic, but
            // cannot change the generation, baseline or operation identity.
            expected.error = record.status.error.clone();
            if record.status != expected {
                return Err(Error::Invalid(
                    "pending reconciliation status differs from fixed operation".into(),
                ));
            }
            if record.status.active_operation.as_deref() != Some(id.as_str())
                || pending_operation.replace(id).is_some()
            {
                return Err(Error::Invalid(
                    "orphan pending reconciliation operation".into(),
                ));
            }
        }
    }
    if record.status.active_operation != pending_operation {
        return Err(Error::Invalid(
            "active reconciliation operation has no pending row".into(),
        ));
    }
    crate::sync::validate_retained_reader(source, &roots.into_iter().collect::<Vec<_>>())?;
    Ok(())
}

fn collect_roots(status: &Status, roots: &mut BTreeSet<String>) {
    roots.insert(status.baseline.native_target.revision.clone());
    if let Some(observation) = &status.observation {
        roots.insert(observation.git_snapshot.clone());
        roots.insert(observation.native_target.revision.clone());
        roots.extend(observation.candidate_revision.iter().cloned());
    }
    if let Some(exit) = &status.exit {
        roots.insert(exit.native_target.revision.clone());
    }
}
