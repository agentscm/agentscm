//! Trusted-local snapshot projection into a newly owned bare Git mirror.
//!
//! This does not move native authority, run candidate code, or push a remote.
mod external;
mod git;
pub mod publish;
mod seed;
mod tree;

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::authority::Authority;
use crate::local_recovery::AccessGuard;
use crate::model::IntegrationEvent;
use crate::store::Engine;
use crate::{Error, Result};
pub use external::{ExternalCheck, ExternalCheckOutcome, ExternalCheckRequest};
use git::Git;
pub use seed::SeedMetadata;

/// Written to mirrors and commit bodies; independent of the snapshot capture
/// profile so that profile can change without invalidating existing mirrors.
pub const PROFILE: &str = "agentscm-git-snapshot/v2";
const LEGACY_PROFILE: &str = "agentscm-git-snapshot/v1";

const REF: &str = "refs/heads/agentscm";
const OWNER: &str = "agentscm-mirror.json";
const JOURNAL: &str = "agentscm-export.sqlite";
const LOCK: &str = "agentscm-export.lock";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommitMetadata {
    pub name: String,
    pub email: String,
    /// Fixed Unix time in seconds, used for both author and committer, in UTC.
    pub timestamp: u64,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    pub event_sequence: u64,
    /// None means the owned ref must not exist. Otherwise the full SHA-1 OID.
    pub expected_oid: Option<String>,
    pub metadata: CommitMetadata,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportState {
    Pending,
    Verified,
    Mirrored,
    Error,
}

/// Retained sidecar: Git snapshots do not encode native history or approval semantics.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportRecord {
    pub profile: String,
    pub repository: String,
    pub event: IntegrationEvent,
    pub request: ExportRequest,
    pub tree: Option<String>,
    pub commit: Option<String>,
    pub state: ExportState,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportStatus {
    pub record: ExportRecord,
    /// Current observation, separate from the retained historical outcome.
    pub current_oid: Option<String>,
    pub ref_matches: bool,
    /// Current bounded connectivity failure, independent of historical journal state.
    pub object_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ownership {
    version: u32,
    profile: String,
    repository: String,
    target: String,
    destination: PathBuf,
    config: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seed: Option<SeedMetadata>,
}

pub struct Mirror {
    git: Git,
    owner: Ownership,
    journal: Connection,
    // Drop the journal before releasing the cooperative recovery lease.
    _access: AccessGuard,
}

impl Mirror {
    /// Creates a new directory. Existing directories, including empty ones, are rejected.
    /// An interrupted initialization is left for inspection; it is never adopted.
    pub fn initialize(
        executable: &Path,
        destination: &Path,
        repository: &str,
        target: &str,
    ) -> Result<Self> {
        Self::initialize_inner(executable, destination, repository, target, None)
    }

    /// Creates a new mirror whose first exported snapshot descends from one exact
    /// commit copied from a local SHA-1 repository. Source refs and worktree state
    /// are never consulted.
    pub fn initialize_with_seed(
        executable: &Path,
        destination: &Path,
        repository: &str,
        target: &str,
        seed_repository: &Path,
        seed_commit: &str,
    ) -> Result<Self> {
        let seed = seed::QualifiedSeed::qualify(executable, seed_repository, seed_commit)?;
        Self::initialize_inner(executable, destination, repository, target, Some(seed))
    }

    fn initialize_inner(
        executable: &Path,
        destination: &Path,
        repository: &str,
        target: &str,
        seed: Option<seed::QualifiedSeed>,
    ) -> Result<Self> {
        if repository.len() != 32
            || !repository
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Invalid("expected native repository identity".into()));
        }
        // Match the authority identifier contract before owning any destination.
        if target.is_empty()
            || target.len() > 128
            || !target
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
        {
            return Err(Error::Invalid("invalid native target".into()));
        }
        let name = destination
            .file_name()
            .ok_or_else(|| Error::Invalid("mirror needs a name".into()))?;
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = parent.canonicalize()?;
        ensure_separate(&parent)?;
        let destination = parent.join(name);
        // Validate the executable before creating anything.
        let git = Git::new(executable, destination.clone())?;
        crate::local_recovery::check_before_create(&destination, false)?;
        fs::create_dir(&destination)?;
        let access = AccessGuard::shared(&destination)?;
        git.run(
            &["init", "--bare", "--template=", "--object-format=sha1", "."],
            None,
            4096,
        )?;
        if let Some(seed) = &seed {
            seed.copy_to(&git)?;
            git.run(
                &[
                    "update-ref",
                    "--no-deref",
                    REF,
                    &seed.metadata.commit,
                    &"0".repeat(40),
                ],
                None,
                4096,
            )?;
            if git.current(REF)? != Some(seed.metadata.commit.clone()) {
                return Err(Error::Conflict(
                    "seed ref changed during mirror initialization".into(),
                ));
            }
            git.verify_closure(
                &seed.metadata.commit,
                Some(&seed.metadata.tree),
                git::MAX_SEED_OBJECTS,
            )?;
        }
        let owner = Ownership {
            version: 3,
            profile: PROFILE.into(),
            repository: repository.into(),
            target: target.into(),
            config: fs::read(destination.join("config"))?,
            destination,
            seed: seed.map(|seed| seed.metadata),
        };
        fs::File::create_new(owner.destination.join(LOCK))?.sync_all()?;
        let journal = Connection::open(owner.destination.join(JOURNAL))?;
        journal.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
             PRAGMA application_id=1095190616; PRAGMA user_version=1;
             CREATE TABLE exports(sequence INTEGER PRIMARY KEY, record BLOB NOT NULL) STRICT;",
        )?;
        let bytes = serde_json::to_vec(&owner)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(owner.destination.join(OWNER))?;
        use std::io::Write;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::File::open(&owner.destination)?.sync_all()?;
        let mirror = Self {
            git,
            owner,
            journal,
            _access: access,
        };
        mirror.validate()?;
        Ok(mirror)
    }

    pub fn seed(&self) -> Option<&SeedMetadata> {
        self.owner.seed.as_ref()
    }

    pub fn open(executable: &Path, destination: &Path, repository: &str) -> Result<Self> {
        let destination = destination.canonicalize()?;
        ensure_separate(
            destination
                .parent()
                .ok_or_else(|| Error::Invalid("mirror needs a parent".into()))?,
        )?;
        use std::io::Read;
        let mut bytes = Vec::new();
        fs::File::open(destination.join(OWNER))?
            .take(16385)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 16384 {
            return Err(Error::Limit("mirror ownership record too large".into()));
        }
        let owner: Ownership = serde_json::from_slice(&bytes)?;
        if owner.repository != repository || owner.destination != destination {
            return Err(Error::Conflict(
                "mirror ownership/path does not match".into(),
            ));
        }
        let access = AccessGuard::shared(&destination)?;
        let mirror = Self {
            git: Git::new(executable, destination.clone())?,
            owner,
            journal: Connection::open_with_flags(
                destination.join(JOURNAL),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )?,
            _access: access,
        };
        mirror.journal.pragma_update(None, "synchronous", "FULL")?;
        mirror.validate()?;
        Ok(mirror)
    }

    fn validate(&self) -> Result<()> {
        let application: i64 = self
            .journal
            .pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = self
            .journal
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if application != 1095190616 || !matches!(version, 1 | 2) {
            return Err(Error::Invalid("unsupported export journal".into()));
        }
        self.journal
            .prepare("SELECT sequence,record FROM exports LIMIT 0")?;
        if version == 2 {
            self.journal
                .prepare("SELECT result_id,event_sequence,record FROM external_checks LIMIT 0")?;
        }
        let supported_owner = matches!(
            (
                self.owner.version,
                self.owner.profile.as_str(),
                self.owner.seed.is_some()
            ),
            (1, LEGACY_PROFILE, false) | (2, LEGACY_PROFILE, true) | (3, PROFILE, _)
        );
        if !supported_owner || fs::read(self.owner.destination.join("config"))? != self.owner.config
        {
            return Err(Error::Conflict("mirror ownership/config changed".into()));
        }
        for path in ["config", OWNER, JOURNAL, LOCK, "objects", "refs"] {
            if fs::symlink_metadata(self.owner.destination.join(path))?
                .file_type()
                .is_symlink()
            {
                return Err(Error::Conflict(
                    "mirror metadata must not be symlinked".into(),
                ));
            }
        }
        // Repository discovery must not redirect this mirror's objects or refs
        // outside the destination protected by its ownership record and lease.
        for path in [
            ".git",
            "commondir",
            "gitdir",
            "objects/info/alternates",
            "objects/info/http-alternates",
            "worktrees",
            "shallow",
            "info/grafts",
        ] {
            if fs::symlink_metadata(self.owner.destination.join(path)).is_ok() {
                return Err(Error::Conflict(format!(
                    "unsupported mirror metadata: {path}"
                )));
            }
        }
        if self.git.text(&["rev-parse", "--is-bare-repository"])? != "true" {
            return Err(Error::Conflict(
                "destination is not an owned bare mirror".into(),
            ));
        }
        if self.git.object_format()? != "sha1" {
            return Err(Error::Conflict("mirror object format changed".into()));
        }
        if let Some(seed) = &self.owner.seed {
            if seed.object_format != "sha1" {
                return Err(Error::Conflict(
                    "unsupported retained seed object format".into(),
                ));
            }
            git::validate_oid(&seed.commit)?;
            git::validate_oid(&seed.tree)?;
        }
        Ok(())
    }

    fn record(&self, sequence: u64) -> Result<Option<ExportRecord>> {
        let mut remaining = MAX_EXPORT_RECORD_BYTES;
        read_record(&self.journal, &self.owner, sequence, &mut remaining)
    }

    fn save(&self, record: &ExportRecord) -> Result<()> {
        self.journal.execute(
            "UPDATE exports SET record=?1 WHERE sequence=?2",
            params![
                serde_json::to_vec(record)?,
                sql_sequence(record.event.sequence)?
            ],
        )?;
        Ok(())
    }

    pub fn status(&self, sequence: u64) -> Result<Option<ExportStatus>> {
        self.validate()?;
        self.record(sequence)?
            .map(|record| {
                let object_error =
                    if let (Some(commit), Some(tree)) = (&record.commit, &record.tree) {
                        self.git
                            .verify_closure(commit, Some(tree), git::MAX_CLOSURE_OBJECTS)
                            .err()
                            .map(|e| e.to_string())
                    } else {
                        None
                    };
                let current_oid = self.git.current(REF)?;
                let ref_matches = record.commit.is_some() && record.commit == current_oid;
                Ok(ExportStatus {
                    record,
                    current_oid,
                    ref_matches,
                    object_error,
                })
            })
            .transpose()
    }

    /// Bounded reverse lookup. The journal retains one mapping per exported integration event.
    pub fn lookup_revision(&self, revision: &str) -> Result<Vec<ExportRecord>> {
        crate::store::validate_id(revision)?;
        self.validate()?;
        let tx = self.journal.unchecked_transaction()?;
        let mut remaining = MAX_EXPORT_SCAN_BYTES;
        let mut result = Vec::new();
        {
            // Read only keys while streaming; each payload is lazily selected
            // by the common reader using the remaining cumulative budget.
            let mut query =
                tx.prepare("SELECT sequence FROM exports ORDER BY sequence LIMIT 100001")?;
            for (index, row) in query.query_map([], |row| row.get::<_, i64>(0))?.enumerate() {
                if index == 100000 {
                    return Err(Error::Limit("export lookup exceeds 100000 records".into()));
                }
                let sequence = retained_sequence(row?)?;
                let record = read_record(&tx, &self.owner, sequence, &mut remaining)?
                    .ok_or_else(|| Error::Invalid("export row disappeared from snapshot".into()))?;
                if record.event.after.revision == revision {
                    result.push(record);
                }
            }
        }
        tx.commit()?;
        Ok(result)
    }

    pub fn export(
        &mut self,
        engine: &Engine,
        authority: &Authority,
        request: &ExportRequest,
    ) -> Result<ExportStatus> {
        self.validate()?;
        // A kernel-owned lock retires on process exit. It serializes the entire
        // Git/SQLite sequence while allowing each journal boundary to be durable.
        let _lease = crate::lease::Lease::try_exclusive(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.owner.destination.join(LOCK))?,
        )
        .map_err(|_| Error::Conflict("mirror export is already running".into()))?;
        validate_request(request)?;
        if authority.repository_id()? != self.owner.repository {
            return Err(Error::Conflict(
                "native repository differs from mirror owner".into(),
            ));
        }
        let event = authority
            .events(request.event_sequence - 1, 1)?
            .into_iter()
            .next()
            .filter(|e| e.sequence == request.event_sequence)
            .ok_or_else(|| Error::NotFound("integration event".into()))?;
        external::validate_event(&event, &self.owner.repository)?;
        if event.target != self.owner.target {
            return Err(Error::Conflict(
                "event target differs from owned mirror target".into(),
            ));
        }
        // Commit the fixed request before touching Git objects. An unfinished event
        // reserves the ref; other events cannot overtake it after a crash.
        let tx = self
            .journal
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut remaining = MAX_EXPORT_SCAN_BYTES;
        let existing = read_record(&tx, &self.owner, event.sequence, &mut remaining)?;
        let mut record = if let Some(record) = existing {
            if record.request != *request || record.event != event {
                return Err(Error::Conflict(
                    "export event already has a different fixed request".into(),
                ));
            }
            record
        } else {
            let retained: i64 = tx.query_row("SELECT count(*) FROM exports", [], |r| r.get(0))?;
            if retained >= 100000 {
                return Err(Error::Limit(
                    "export journal is full at 100000 records".into(),
                ));
            }
            let mut last = self.owner.seed.as_ref().map(|seed| seed.commit.clone());
            let mut rows =
                tx.prepare("SELECT sequence FROM exports ORDER BY sequence LIMIT 100001")?;
            for (index, row) in rows.query_map([], |row| row.get::<_, i64>(0))?.enumerate() {
                if index == 100000 {
                    return Err(Error::Limit("export journal exceeds 100000 records".into()));
                }
                let previous =
                    read_record(&tx, &self.owner, retained_sequence(row?)?, &mut remaining)?
                        .ok_or_else(|| {
                            Error::Invalid("export row disappeared from snapshot".into())
                        })?;
                if previous.state != ExportState::Mirrored {
                    return Err(Error::Conflict(
                        "another export is unfinished; retry its fixed request".into(),
                    ));
                }
                if previous.request.expected_oid != last {
                    return Err(Error::Conflict(
                        "retained export lineage differs from fixed parent".into(),
                    ));
                }
                // The common reader requires exact valid commit/tree OIDs and
                // no error for every Mirrored row; None cannot restart lineage.
                last = previous.commit.clone();
                if previous.event.sequence >= event.sequence {
                    return Err(Error::Conflict(
                        "cannot export an older native event".into(),
                    ));
                }
            }
            drop(rows);
            if request.expected_oid != last || self.git.current(REF)? != last {
                return Err(Error::Conflict(
                    "expected ref differs from retained mirror lineage or current Git ref".into(),
                ));
            }
            let record = ExportRecord {
                profile: self.owner.profile.clone(),
                repository: self.owner.repository.clone(),
                event,
                request: request.clone(),
                tree: None,
                commit: None,
                state: ExportState::Pending,
                error: None,
            };
            tx.execute(
                "INSERT INTO exports(sequence,record) VALUES (?1,?2)",
                params![
                    sql_sequence(record.event.sequence)?,
                    serde_json::to_vec(&record)?
                ],
            )?;
            record
        };
        tx.commit()?;
        if record.state == ExportState::Mirrored {
            let latest: i64 =
                self.journal
                    .query_row("SELECT max(sequence) FROM exports", [], |r| r.get(0))?;
            if latest != sql_sequence(record.event.sequence)? {
                return Err(Error::Conflict(
                    "historical export cannot be retried after a newer request; inspect status"
                        .into(),
                ));
            }
            if self.git.current(REF)? != record.commit {
                return Err(Error::Conflict(
                    "previously mirrored ref moved; inspect retained status".into(),
                ));
            }
            // A retained ref string alone cannot prove that its objects survived
            // an interrupted restore. Rebuild and byte-verify the snapshot too.
        }
        let result = self.finish(engine, &mut record).and_then(|()| {
            let status = self
                .status(record.event.sequence)?
                .ok_or_else(|| Error::NotFound("export record".into()))?;
            if !status.ref_matches || status.object_error.is_some() {
                return Err(Error::Conflict(
                    "mirror changed after export; inspect retained status".into(),
                ));
            }
            Ok(status)
        });
        match result {
            Ok(status) => Ok(status),
            Err(error) => {
                // A fresh verification failure must not rewrite a durable
                // historical success or make it unfinished after a crash.
                if record.state != ExportState::Mirrored {
                    record.state = ExportState::Error;
                    record.error = Some(error.to_string().chars().take(1024).collect());
                    self.save(&record)?;
                }
                Err(error)
            }
        }
    }

    fn finish(&self, engine: &Engine, record: &mut ExportRecord) -> Result<()> {
        if let Some(parent) = &record.request.expected_oid {
            self.git
                .verify_closure(parent, None, git::MAX_CLOSURE_OBJECTS)?;
        }
        let tree = tree::build_and_verify(
            &self.git,
            engine,
            &record.event.after.revision,
            &self.owner.profile,
        )?;
        let metadata = &record.request.metadata;
        let parent = record
            .request
            .expected_oid
            .as_ref()
            .map(|oid| format!("parent {oid}\n"))
            .unwrap_or_default();
        let identity = format!(
            "{} <{}> {} +0000",
            metadata.name, metadata.email, metadata.timestamp
        );
        let body = format!(
            "tree {tree}\n{parent}author {identity}\ncommitter {identity}\n\n{}\n\nNative-Repository: {}\nNative-Revision: {}\nNative-Event: {}\nSnapshot-Profile: {}\n",
            metadata.message,
            record.repository,
            record.event.after.revision,
            record.event.sequence,
            record.profile
        );
        let commit = self.git.write_object("commit", body.as_bytes())?;
        if record.commit.as_ref().is_some_and(|old| old != &commit)
            || record.tree.as_ref().is_some_and(|old| old != &tree)
        {
            return Err(Error::Conflict(
                "rebuilt export differs from retained mapping".into(),
            ));
        }
        self.git
            .verify_closure(&commit, Some(&tree), git::MAX_CLOSURE_OBJECTS)?;
        if record.state == ExportState::Mirrored {
            if self.git.current(REF)? != Some(commit) {
                return Err(Error::Conflict(
                    "Git ref changed during historical re-verification".into(),
                ));
            }
            // The whole successful retry path is journal/ref read-only. Missing
            // immutable objects may have been restored under the same identities.
            return Ok(());
        }
        record.commit = Some(commit.clone());
        record.tree = Some(tree);
        record.state = ExportState::Verified;
        record.error = None;
        self.save(record)?;
        self.validate()?;
        let current = self.git.current(REF)?;
        if current != Some(commit.clone()) {
            if current != record.request.expected_oid {
                return Err(Error::Conflict(
                    "owned Git ref moved; export remains unfinished".into(),
                ));
            }
            let expected = record
                .request
                .expected_oid
                .clone()
                .unwrap_or_else(|| "0".repeat(40));
            // Hooks disabled even for reference-transaction; Git performs atomic CAS.
            self.git.run(
                &["update-ref", "--no-deref", REF, &commit, &expected],
                None,
                4096,
            )?;
        }
        if self.git.current(REF)? != Some(commit) {
            return Err(Error::Conflict(
                "Git ref changed after update; inspect status".into(),
            ));
        }
        record.state = ExportState::Mirrored;
        self.save(record)
    }
}

const MAX_EXPORT_RECORD_BYTES: usize = 64 * 1024;
const MAX_EXPORT_SCAN_BYTES: usize = 8 * 1024 * 1024;

// Select the BLOB only below both bounds, in the same SQLite snapshot as its
// byte length. octet_length does not materialize oversized overflow content.
fn read_record(
    database: &Connection,
    owner: &Ownership,
    sequence: u64,
    remaining: &mut usize,
) -> Result<Option<ExportRecord>> {
    let limit = (*remaining).min(MAX_EXPORT_RECORD_BYTES);
    let row: Option<(i64, i64, Option<Vec<u8>>)> = database
        .query_row(
            "SELECT sequence, octet_length(record),
             CASE WHEN typeof(record)='blob' AND octet_length(record)<=?2 THEN record END
             FROM exports WHERE sequence=?1",
            params![sql_sequence(sequence)?, limit as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(key, length, bytes)| {
        let bytes = record_bytes(length, bytes, MAX_EXPORT_RECORD_BYTES, remaining)?;
        let record: ExportRecord = serde_json::from_slice(&bytes)?;
        if retained_sequence(key)? != sequence
            || record.event.sequence != sequence
            || record.request.event_sequence != sequence
        {
            return Err(Error::Invalid(
                "export row key differs from retained event/request".into(),
            ));
        }
        validate_record(&record, owner)?;
        Ok(record)
    })
    .transpose()
}

fn retained_sequence(key: i64) -> Result<u64> {
    let sequence =
        u64::try_from(key).map_err(|_| Error::Invalid("invalid export event key".into()))?;
    sql_sequence(sequence)?;
    Ok(sequence)
}

fn validate_record(record: &ExportRecord, owner: &Ownership) -> Result<()> {
    if record.profile != owner.profile
        || record.repository != owner.repository
        || record.repository.len() != 32
        || !record
            .repository
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || record.event.target != owner.target
    {
        return Err(Error::Conflict(
            "retained export differs from owned repository/target/profile".into(),
        ));
    }
    validate_request(&record.request)?;
    external::validate_event(&record.event, &owner.repository)?;
    let mapped = match (&record.commit, &record.tree) {
        (Some(commit), Some(tree)) => {
            git::validate_oid(commit)?;
            git::validate_oid(tree)?;
            true
        }
        (None, None) => false,
        _ => {
            return Err(Error::Invalid(
                "incomplete retained commit/tree mapping".into(),
            ));
        }
    };
    let valid_error = record
        .error
        .as_ref()
        .is_some_and(|error| !error.is_empty() && error.chars().count() <= 1024);
    let valid_state = match record.state {
        ExportState::Pending => !mapped && record.error.is_none(),
        ExportState::Verified | ExportState::Mirrored => mapped && record.error.is_none(),
        // Failures before object construction have neither OID. A failed CAS
        // or later verification retains both exact OIDs for the real retry.
        ExportState::Error => valid_error,
    };
    if !valid_state {
        return Err(Error::Invalid(
            "invalid retained export state/mapping/error".into(),
        ));
    }
    Ok(())
}

fn record_bytes(
    length: i64,
    bytes: Option<Vec<u8>>,
    maximum: usize,
    remaining: &mut usize,
) -> Result<Vec<u8>> {
    let length = usize::try_from(length)
        .map_err(|_| Error::Invalid("invalid retained record length".into()))?;
    if length > maximum || length > *remaining {
        return Err(Error::Limit("retained record exceeds byte budget".into()));
    }
    let bytes = bytes.ok_or_else(|| Error::Invalid("retained record payload is absent".into()))?;
    if bytes.len() != length {
        return Err(Error::Invalid(
            "retained record length differs from payload".into(),
        ));
    }
    *remaining -= length;
    Ok(bytes)
}

fn sql_sequence(sequence: u64) -> Result<i64> {
    if sequence == 0 {
        return Err(Error::Invalid("event sequence must be positive".into()));
    }
    i64::try_from(sequence)
        .map_err(|_| Error::Invalid("event sequence exceeds SQLite range".into()))
}

fn validate_request(request: &ExportRequest) -> Result<()> {
    sql_sequence(request.event_sequence)?;
    if let Some(oid) = &request.expected_oid {
        git::validate_oid(oid)?;
    }
    let metadata = &request.metadata;
    for value in [&metadata.name, &metadata.email] {
        if value.is_empty()
            || value.len() > 256
            || value
                .chars()
                .any(|c| c.is_control() || matches!(c, '<' | '>'))
        {
            return Err(Error::Invalid("invalid commit identity".into()));
        }
    }
    if metadata.timestamp > 253402300799
        || metadata.message.is_empty()
        || metadata.message.len() > 4096
        || metadata.message.contains('\0')
    {
        return Err(Error::Invalid("invalid fixed commit metadata".into()));
    }
    Ok(())
}

// Refuse nesting in ordinary Git checkouts (including linked worktrees), bare
// repositories and native workspaces/data. No Git discovery or candidate code.
fn ensure_separate(parent: &Path) -> Result<()> {
    for ancestor in parent.ancestors() {
        let marker = [
            ".git",
            ".agentscm-workspace",
            "state.sqlite",
            "agentscm-mirror.json",
        ]
        .iter()
        .any(|name| fs::symlink_metadata(ancestor.join(name)).is_ok());
        let bare = ancestor.join("HEAD").is_file()
            && ancestor.join("objects").is_dir()
            && ancestor.join("config").is_file();
        if marker || bare {
            return Err(Error::Invalid(
                "mirror must be outside existing source/data repositories".into(),
            ));
        }
    }
    Ok(())
}
