//! Trusted-local snapshot projection into a newly owned bare Git mirror.
//!
//! This does not move native authority, run candidate code, or push a remote.
mod external;
mod git;
pub mod publish;
pub mod remote;
mod seed;
mod tree;
#[cfg(unix)]
pub(crate) use tree::native_tree_oid;

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

pub use crate::candidate::CandidateSubject;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CandidateExportRequest {
    pub subject: CandidateSubject,
    pub expected_oid: String,
    pub metadata: CommitMetadata,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CandidateExportRecord {
    pub profile: String,
    pub repository: String,
    pub request: CandidateExportRequest,
    pub tree: Option<String>,
    pub commit: Option<String>,
    pub state: ExportState,
    pub error: Option<String>,
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
             CREATE TABLE exports(sequence INTEGER PRIMARY KEY, record BLOB NOT NULL) STRICT;
             CREATE TABLE candidates(id TEXT PRIMARY KEY, record BLOB NOT NULL) STRICT;",
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
        self.claim_stream("integrated")?;
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

    /// Build and retain one exact unlanded candidate. The caller must perform its own
    /// current Authority and attached-source checks immediately before this boundary.
    pub fn export_candidate(
        &mut self,
        engine: &Engine,
        request: &CandidateExportRequest,
    ) -> Result<CandidateExportRecord> {
        self.export_candidate_guarded(engine, request, || Ok(()))
    }

    pub fn export_candidate_guarded(
        &mut self,
        engine: &Engine,
        request: &CandidateExportRequest,
        mut qualify: impl FnMut() -> Result<()>,
    ) -> Result<CandidateExportRecord> {
        self.validate()?;
        validate_candidate_request(request, &self.owner)?;
        let _lease = crate::lease::Lease::try_exclusive(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.owner.destination.join(LOCK))?,
        )
        .map_err(|_| Error::Conflict("mirror export is already running".into()))?;
        qualify()?;
        self.claim_stream("candidate")?;
        self.check_prepare_reservations(request)?;
        ensure_candidate_table(&self.journal)?;
        let mut record = if let Some(record) = self.candidate_record(&request.subject.id)? {
            if record.request != *request {
                return Err(Error::Conflict(
                    "candidate id has a different fixed request".into(),
                ));
            }
            record
        } else {
            let retained: i64 =
                self.journal
                    .query_row("SELECT count(*) FROM candidates", [], |r| r.get(0))?;
            if retained >= 4096 {
                return Err(Error::Limit(
                    "candidate export journal is full at 4096 records".into(),
                ));
            }
            let mut remaining = MAX_EXPORT_SCAN_BYTES;
            let mut ids = self
                .journal
                .prepare("SELECT CASE WHEN typeof(id)='text' AND octet_length(id)<=64 THEN id END FROM candidates ORDER BY id LIMIT 4097")?;
            let retained_ids = ids
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(ids);
            if retained_ids.len() > 4096 {
                return Err(Error::Limit(
                    "candidate export journal exceeds 4096 records".into(),
                ));
            }
            for id in retained_ids {
                let prior = self
                    .read_candidate(&id, &mut remaining, false)?
                    .ok_or_else(|| Error::Invalid("candidate export row disappeared".into()))?;
                if !prior.request.subject.same_lineage(&request.subject) {
                    return Err(Error::Conflict(
                        "candidate mirror belongs to another attachment/Change/output".into(),
                    ));
                }
                if prior.state != ExportState::Mirrored {
                    return Err(Error::Conflict(
                        "another candidate export is unfinished; retry its fixed request".into(),
                    ));
                }
            }
            if self.git.current(REF)?.as_deref() != Some(request.expected_oid.as_str()) {
                return Err(Error::Conflict(
                    "candidate parent differs from the owned mirror ref".into(),
                ));
            }
            let record = CandidateExportRecord {
                profile: self.owner.profile.clone(),
                repository: self.owner.repository.clone(),
                request: request.clone(),
                tree: None,
                commit: None,
                state: ExportState::Pending,
                error: None,
            };
            let bytes = serde_json::to_vec(&record)?;
            if bytes.len() > remaining || bytes.len() > MAX_EXPORT_RECORD_BYTES {
                return Err(Error::Limit(
                    "candidate journal byte budget exceeded".into(),
                ));
            }
            self.journal.execute(
                "INSERT INTO candidates(id,record) VALUES(?1,?2)",
                params![request.subject.id, bytes],
            )?;
            record
        };
        if record.state == ExportState::Mirrored {
            // A receipt remains valid history after a later candidate moves the ref.
            return Ok(record);
        }
        let tree = tree::build_and_verify(
            &self.git,
            engine,
            &request.subject.revision,
            &self.owner.profile,
        )?;
        self.git.verify_closure(
            &request.subject.baseline_commit,
            None,
            git::MAX_CLOSURE_OBJECTS,
        )?;
        let body = candidate_body(&record, &tree);
        let commit = self.git.write_object("commit", body.as_bytes())?;
        if record.commit.as_ref().is_some_and(|old| old != &commit)
            || record.tree.as_ref().is_some_and(|old| old != &tree)
        {
            return Err(Error::Conflict(
                "rebuilt candidate differs from retained mapping".into(),
            ));
        }
        self.git
            .verify_closure(&commit, Some(&tree), git::MAX_CLOSURE_OBJECTS)?;
        if record.state != ExportState::Mirrored {
            record.commit = Some(commit.clone());
            record.tree = Some(tree);
            record.state = ExportState::Verified;
            record.error = None;
            save_candidate(&self.journal, &record)?;
            qualify()?;
            let current = self.git.current(REF)?;
            if current != Some(commit.clone()) {
                if current.as_deref() != Some(request.expected_oid.as_str()) {
                    return Err(Error::Conflict(
                        "owned Git ref moved; candidate remains unfinished".into(),
                    ));
                }
                self.git.run(
                    &[
                        "update-ref",
                        "--no-deref",
                        REF,
                        &commit,
                        &request.expected_oid,
                    ],
                    None,
                    4096,
                )?;
            }
            if self.git.current(REF)? != Some(commit) {
                return Err(Error::Conflict(
                    "Git ref changed after candidate update".into(),
                ));
            }
            record.state = ExportState::Mirrored;
            save_candidate(&self.journal, &record)?;
        }
        Ok(record)
    }

    pub fn candidate_record(&self, id: &str) -> Result<Option<CandidateExportRecord>> {
        self.validate()?;
        let mut remaining = MAX_EXPORT_RECORD_BYTES;
        self.read_candidate(id, &mut remaining, true)
    }

    fn read_candidate(
        &self,
        id: &str,
        remaining: &mut usize,
        verify: bool,
    ) -> Result<Option<CandidateExportRecord>> {
        crate::store::validate_id(id)?;
        let present: bool = self.journal.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='candidates')",
            [],
            |r| r.get(0),
        )?;
        if !present {
            return Ok(None);
        }
        let row: Option<(i64, Option<Vec<u8>>)> = self.journal.query_row(
            "SELECT octet_length(record), CASE WHEN typeof(record)='blob' AND octet_length(record)<=?2 THEN record END FROM candidates WHERE id=?1",
            params![id, (*remaining).min(MAX_EXPORT_RECORD_BYTES) as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        let Some((length, bytes)) = row else {
            return Ok(None);
        };
        let bytes = record_bytes(length, bytes, MAX_EXPORT_RECORD_BYTES, remaining)?;
        let record: CandidateExportRecord = serde_json::from_slice(&bytes)?;
        if record.request.subject.id != id
            || record.repository != self.owner.repository
            || record.profile != self.owner.profile
        {
            return Err(Error::Invalid(
                "candidate export record identity mismatch".into(),
            ));
        }
        validate_candidate_request(&record.request, &self.owner)?;
        if record.error.is_some() || record.state == ExportState::Error {
            return Err(Error::Invalid(
                "invalid candidate state/error combination".into(),
            ));
        }
        match (&record.commit, &record.tree, record.state) {
            (None, None, ExportState::Pending | ExportState::Error) => {}
            (
                Some(commit),
                Some(tree),
                ExportState::Verified | ExportState::Mirrored | ExportState::Error,
            ) => {
                git::validate_oid(commit)?;
                git::validate_oid(tree)?;
                if verify {
                    self.git
                        .verify_closure(commit, Some(tree), git::MAX_CLOSURE_OBJECTS)?;
                    if self.git.run(
                        &["cat-file", "commit", commit],
                        None,
                        MAX_EXPORT_RECORD_BYTES as u64,
                    )? != candidate_body(&record, tree).as_bytes()
                    {
                        return Err(Error::Invalid(
                            "candidate commit metadata differs from receipt".into(),
                        ));
                    }
                    if self.git.text(&["rev-parse", &format!("{commit}^1")])?
                        != record.request.expected_oid
                    {
                        return Err(Error::Invalid(
                            "candidate commit parent differs from receipt".into(),
                        ));
                    }
                }
            }
            _ => {
                return Err(Error::Invalid(
                    "inconsistent candidate export record".into(),
                ));
            }
        }
        Ok(Some(record))
    }

    fn prepare_abandoned(&self, operation: &str) -> Result<bool> {
        let present: bool = self.journal.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='candidate_abandoned' AND type='table')", [], |r| r.get(0))?;
        if !present {
            return Ok(false);
        }
        Ok(self.journal.query_row(
            "SELECT EXISTS(SELECT 1 FROM candidate_abandoned WHERE id=?1)",
            [operation],
            |r| r.get(0),
        )?)
    }

    /// Abandon only a reservation that never began candidate construction and
    /// whose expected owned ref is still unchanged. The fixed request is retained.
    pub fn abandon_candidate_operation(&self, operation: &str) -> Result<()> {
        remote::validate_id(operation)?;
        self.validate()?;
        let _lease = crate::lease::Lease::try_exclusive(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.owner.destination.join(LOCK))?,
        )
        .map_err(|_| Error::Conflict("mirror already in use".into()))?;
        let bytes: Option<Vec<u8>> = self.journal.query_row(
            "SELECT CASE WHEN typeof(record)='blob' AND octet_length(record)<=65536 THEN record END FROM candidate_operations WHERE id=?1",
            [operation], |r| r.get(0),
        )?;
        let bytes = bytes.ok_or_else(|| Error::Limit("prepare record exceeds budget".into()))?;
        let (key, _, request): (String, serde_json::Value, CandidateExportRequest) =
            serde_json::from_slice(&bytes)?;
        validate_candidate_request(&request, &self.owner)?;
        if key != operation {
            return Err(Error::Invalid("prepare row key mismatch".into()));
        }
        if self.prepare_abandoned(operation)? {
            return Ok(());
        }
        if self.candidate_record(&request.subject.id)?.is_some()
            || self.owned_oid()?.as_deref() != Some(&request.expected_oid)
        {
            return Err(Error::Conflict(
                "candidate construction or ref movement began; resume the exact prepare instead"
                    .into(),
            ));
        }
        self.journal.execute_batch(
            "CREATE TABLE IF NOT EXISTS candidate_abandoned(id TEXT PRIMARY KEY) STRICT;",
        )?;
        self.journal
            .execute("INSERT INTO candidate_abandoned VALUES(?1)", [operation])?;
        Ok(())
    }

    fn check_prepare_reservations(&self, request: &CandidateExportRequest) -> Result<()> {
        let present: bool = self.journal.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='candidate_operations' AND type='table')", [], |r| r.get(0))?;
        if !present {
            return Ok(());
        }
        let mut remaining = MAX_EXPORT_SCAN_BYTES;
        let mut query = self.journal.prepare("SELECT CASE WHEN typeof(id)='text' AND octet_length(id)<=64 THEN id END, octet_length(record), CASE WHEN typeof(record)='blob' AND octet_length(record)<=65536 THEN record END FROM candidate_operations LIMIT 4097")?;
        for (index, row) in query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                ))
            })?
            .enumerate()
        {
            if index == 4096 {
                return Err(Error::Limit(
                    "prepare journal exceeds 4096 operations".into(),
                ));
            }
            let (id, length, bytes) = row?;
            remote::validate_id(&id)?;
            let bytes = record_bytes(length, bytes, MAX_EXPORT_RECORD_BYTES, &mut remaining)?;
            let (key, _, retained): (String, serde_json::Value, CandidateExportRequest) =
                serde_json::from_slice(&bytes)?;
            if key != id {
                return Err(Error::Invalid("prepare row key mismatch".into()));
            }
            validate_candidate_request(&retained, &self.owner)?;
            if self.prepare_abandoned(&id)? {
                if retained == *request {
                    return Err(Error::Conflict("prepare was explicitly abandoned".into()));
                }
                continue;
            }
            if !retained.subject.same_lineage(&request.subject) {
                return Err(Error::Conflict(
                    "prepare belongs to another attachment/Change/output".into(),
                ));
            }
            if retained != *request
                && self
                    .read_candidate(&retained.subject.id, &mut remaining, false)?
                    .is_none_or(|r| r.state != ExportState::Mirrored)
            {
                return Err(Error::Conflict(
                    "another prepare is unfinished; recover its fixed request".into(),
                ));
            }
        }
        Ok(())
    }

    /// Fixed caller inputs and the chosen parent survive a partial prepare. This
    /// journal reserves identity before importing target objects or moving refs.
    pub fn candidate_operation(
        &self,
        operation: &str,
        input: &serde_json::Value,
    ) -> Result<Option<CandidateExportRequest>> {
        remote::validate_id(operation)?;
        self.validate()?;
        let present: bool = self.journal.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='candidate_operations' AND type='table')", [], |r| r.get(0))?;
        if !present {
            return Ok(None);
        }
        let row: Option<(i64, Option<Vec<u8>>)> = self.journal.query_row("SELECT octet_length(record), CASE WHEN typeof(record)='blob' AND octet_length(record)<=65536 THEN record END FROM candidate_operations WHERE id=?1", [operation], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((length, bytes)) = row else {
            return Ok(None);
        };
        let mut remaining = MAX_EXPORT_RECORD_BYTES;
        let bytes = record_bytes(length, bytes, MAX_EXPORT_RECORD_BYTES, &mut remaining)?;
        let (stored_operation, stored_input, request): (
            String,
            serde_json::Value,
            CandidateExportRequest,
        ) = serde_json::from_slice(&bytes)?;
        if stored_operation != operation || stored_input != *input {
            return Err(Error::Conflict(
                "prepare operation has different fixed inputs".into(),
            ));
        }
        if self.prepare_abandoned(operation)? {
            return Err(Error::Conflict(
                "prepare operation was explicitly abandoned; ID remains reserved".into(),
            ));
        }
        validate_candidate_request(&request, &self.owner)?;
        Ok(Some(request))
    }

    /// Read a retained fixed prepare request without accepting new caller-controlled inputs.
    pub fn retained_candidate_operation(
        &self,
        operation: &str,
    ) -> Result<Option<CandidateExportRequest>> {
        remote::validate_id(operation)?;
        self.validate()?;
        let present: bool = self.journal.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='candidate_operations' AND type='table')", [], |r| r.get(0))?;
        if !present {
            return Ok(None);
        }
        let row: Option<Vec<u8>> = self.journal.query_row(
            "SELECT CASE WHEN typeof(record)='blob' AND octet_length(record)<=65536 THEN record END FROM candidate_operations WHERE id=?1",
            [operation], |r| r.get(0),
        ).optional()?;
        let Some(bytes) = row else {
            return Ok(None);
        };
        let (stored, _, request): (String, serde_json::Value, CandidateExportRequest) =
            serde_json::from_slice(&bytes)?;
        if stored != operation {
            return Err(Error::Invalid("prepare row key mismatch".into()));
        }
        validate_candidate_request(&request, &self.owner)?;
        Ok(Some(request))
    }

    pub fn reserve_candidate_operation(
        &self,
        operation: &str,
        input: &serde_json::Value,
        request: &CandidateExportRequest,
    ) -> Result<()> {
        remote::validate_id(operation)?;
        self.validate()?;
        validate_candidate_request(request, &self.owner)?;
        let _lease = crate::lease::Lease::try_exclusive(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.owner.destination.join(LOCK))?,
        )
        .map_err(|_| Error::Conflict("mirror already in use".into()))?;
        self.claim_stream("candidate")?;
        self.journal.execute_batch("CREATE TABLE IF NOT EXISTS candidate_operations(id TEXT PRIMARY KEY, record BLOB NOT NULL) STRICT;")?;
        if let Some(prior) = self.candidate_operation(operation, input)? {
            if prior != *request {
                return Err(Error::Conflict("prepare operation subject changed".into()));
            }
            return Ok(());
        }
        // The parent read and comparison share the export lease with insertion.
        if self.owned_oid()?.as_deref() != Some(&request.expected_oid) {
            return Err(Error::Conflict(
                "candidate parent moved before reservation; requalify".into(),
            ));
        }
        self.check_prepare_reservations(request)?;
        let (count, total): (i64, i64) = self.journal.query_row(
            "SELECT count(*), coalesce(sum(octet_length(record)),0) FROM candidate_operations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let bytes = serde_json::to_vec(&(operation, input, request))?;
        if count >= 4096
            || bytes.len() > MAX_EXPORT_RECORD_BYTES
            || total < 0
            || total as usize + bytes.len() > MAX_EXPORT_SCAN_BYTES
        {
            return Err(Error::Limit(
                "prepare operation journal capacity exceeded".into(),
            ));
        }
        self.journal.execute(
            "INSERT INTO candidate_operations VALUES(?1,?2)",
            params![operation, bytes],
        )?;
        Ok(())
    }

    /// Read bounded fixed inputs from the trusted Mirror binding journal without recording an effect.
    pub(crate) fn candidate_binding_input(
        &self,
        operation: &str,
    ) -> Result<Option<serde_json::Value>> {
        remote::validate_id(operation)?;
        self.validate()?;
        let exists: bool = self.journal.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='candidate_bindings')", [], |r| r.get(0))?;
        if !exists {
            return Ok(None);
        }
        let bytes: Option<Vec<u8>> = self.journal.query_row("SELECT CASE WHEN typeof(record)='blob' AND octet_length(record)<=65536 THEN record END FROM candidate_bindings WHERE id=?1", [operation], |r| r.get(0)).optional()?;
        bytes
            .map(|bytes| {
                let (id, input, request): (String, serde_json::Value, CandidateExportRequest) =
                    serde_json::from_slice(&bytes)?;
                if id != operation {
                    return Err(Error::Invalid("candidate binding identity differs".into()));
                }
                validate_candidate_request(&request, &self.owner)?;
                Ok(input)
            })
            .transpose()
    }

    /// Bind fixed caller inputs to an already completed candidate without reusing the prepare
    /// reservation's parent guard. The candidate and owned ref must still be the retained
    /// mirrored result. This records no remote effect; the remote journal remains authoritative
    /// for dispatch and recovery.
    pub fn bind_mirrored_candidate_operation(
        &self,
        operation: &str,
        input: &serde_json::Value,
        request: &CandidateExportRequest,
    ) -> Result<()> {
        remote::validate_id(operation)?;
        self.validate()?;
        validate_candidate_request(request, &self.owner)?;
        let _lease = crate::lease::Lease::try_exclusive(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.owner.destination.join(LOCK))?,
        )
        .map_err(|_| Error::Conflict("mirror already in use".into()))?;
        self.journal.execute_batch("CREATE TABLE IF NOT EXISTS candidate_bindings(id TEXT PRIMARY KEY, record BLOB NOT NULL) STRICT;")?;
        let prior: Option<Vec<u8>> = self
            .journal
            .query_row(
                "SELECT CASE WHEN typeof(record)='blob' AND octet_length(record)<=65536 THEN record END FROM candidate_bindings WHERE id=?1",
                [operation],
                |row| row.get(0),
            )
            .optional()?;
        let bytes = serde_json::to_vec(&(operation, input, request))?;
        if let Some(prior) = prior {
            return if prior == bytes {
                Ok(())
            } else {
                Err(Error::Conflict(
                    "candidate binding has different fixed inputs".into(),
                ))
            };
        }
        let record = self
            .candidate_record(&request.subject.id)?
            .ok_or_else(|| Error::NotFound("candidate export record".into()))?;
        if record.request != *request
            || record.state != ExportState::Mirrored
            || self.owned_oid()? != record.commit
        {
            return Err(Error::Conflict(
                "candidate binding requires the intact mirrored candidate".into(),
            ));
        }
        let (count, total): (i64, i64) = self.journal.query_row(
            "SELECT count(*), coalesce(sum(octet_length(record)),0) FROM candidate_bindings",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count >= 4096
            || bytes.len() > MAX_EXPORT_RECORD_BYTES
            || total < 0
            || total as usize + bytes.len() > MAX_EXPORT_SCAN_BYTES
        {
            return Err(Error::Limit(
                "candidate binding journal capacity exceeded".into(),
            ));
        }
        self.journal.execute(
            "INSERT INTO candidate_bindings VALUES(?1,?2)",
            params![operation, bytes],
        )?;
        Ok(())
    }

    /// Import a qualified target closure into the owned mirror, without touching
    /// any attached checkout refs, index, or files. Ref advancement happens only
    /// through the journaled candidate export.
    pub fn import_candidate_baseline(&self, source: &Path, commit: &str) -> Result<()> {
        self.validate()?;
        git::validate_oid(commit)?;
        if self
            .git
            .verify_closure(commit, None, git::MAX_SEED_OBJECTS)
            .is_ok()
        {
            // Existing objects still obey the inherited seed size/profile limits.
            seed::QualifiedSeed::qualify(&self.git.executable(), &self.owner.destination, commit)?;
            return Ok(());
        }
        let seed = seed::QualifiedSeed::qualify(&self.git.executable(), source, commit)?;
        seed.copy_to(&self.git)
    }

    /// Find a retained subject before selecting the moving output parent.
    pub fn candidate_for_subject(
        &self,
        subject: &CandidateSubject,
        metadata: &CommitMetadata,
    ) -> Result<Option<CandidateExportRecord>> {
        self.validate()?;
        ensure_candidate_table(&self.journal)?;
        let mut remaining = MAX_EXPORT_SCAN_BYTES;
        let mut query = self.journal.prepare("SELECT CASE WHEN typeof(id)='text' AND octet_length(id)<=64 THEN id END FROM candidates ORDER BY id LIMIT 4097")?;
        let mut found = None;
        for (index, id) in query.query_map([], |r| r.get::<_, String>(0))?.enumerate() {
            if index == 4096 {
                return Err(Error::Limit("candidate lookup exceeds 4096 records".into()));
            }
            let record = self
                .read_candidate(&id?, &mut remaining, false)?
                .ok_or_else(|| Error::Invalid("candidate disappeared".into()))?;
            if !record.request.subject.same_lineage(subject) {
                return Err(Error::Conflict(
                    "candidate mirror belongs to another attachment/Change/output".into(),
                ));
            }
            let mut retained = record.request.subject.clone();
            retained.id = subject.id.clone();
            retained.parent_commit = subject.parent_commit.clone();
            if retained == *subject {
                if record.request.metadata != *metadata {
                    return Err(Error::Conflict(
                        "prepared subject has different fixed commit metadata".into(),
                    ));
                }
                if found.is_some() {
                    return Err(Error::Conflict(
                        "duplicate candidate subject lineage".into(),
                    ));
                }
                found = Some(record);
            }
        }
        found
            .map(|record| self.candidate_record(&record.request.subject.id))
            .transpose()
            .map(Option::flatten)
    }

    // Both streams take the same export lock before claiming the owned ref. Legacy
    // journals with exports are integrated-owned even before this marker exists.
    fn claim_stream(&self, mode: &str) -> Result<()> {
        ensure_candidate_table(&self.journal)?;
        self.journal.execute_batch("CREATE TABLE IF NOT EXISTS export_stream (singleton INTEGER PRIMARY KEY CHECK(singleton=1), mode TEXT NOT NULL CHECK(mode IN ('candidate','integrated'))) STRICT;")?;
        let exports: bool =
            self.journal
                .query_row("SELECT EXISTS(SELECT 1 FROM exports)", [], |r| r.get(0))?;
        let candidates: bool =
            self.journal
                .query_row("SELECT EXISTS(SELECT 1 FROM candidates)", [], |r| r.get(0))?;
        if (mode == "candidate" && exports) || (mode == "integrated" && candidates) {
            return Err(Error::Conflict(
                "owned ref belongs to a different export stream; use a separate mirror".into(),
            ));
        }
        self.journal
            .execute("INSERT OR IGNORE INTO export_stream VALUES(1,?1)", [mode])?;
        let retained: String = self.journal.query_row(
            "SELECT mode FROM export_stream WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if retained != mode {
            return Err(Error::Conflict("owned ref stream differs".into()));
        }
        Ok(())
    }

    /// Current exact commit of the owned mirror ref.
    pub fn owned_oid(&self) -> Result<Option<String>> {
        self.validate()?;
        self.git.current(REF)
    }
}

fn candidate_body(record: &CandidateExportRecord, tree: &str) -> String {
    let request = &record.request;
    let identity = format!(
        "{} <{}> {} +0000",
        request.metadata.name, request.metadata.email, request.metadata.timestamp
    );
    let baseline_parent = if request.subject.baseline_commit != request.expected_oid {
        format!("parent {}\n", request.subject.baseline_commit)
    } else {
        String::new()
    };
    format!(
        "tree {tree}\nparent {}\n{baseline_parent}author {identity}\ncommitter {identity}\n\n{}\n\nNative-Repository: {}\nNative-Revision: {}\nNative-Candidate: {}\nSnapshot-Profile: {}\n",
        request.expected_oid,
        request.metadata.message,
        record.repository,
        request.subject.revision,
        request.subject.id,
        record.profile
    )
}

fn ensure_candidate_table(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS candidates(id TEXT PRIMARY KEY, record BLOB NOT NULL) STRICT;",
    )?;
    Ok(())
}

fn save_candidate(db: &Connection, record: &CandidateExportRecord) -> Result<()> {
    db.execute(
        "UPDATE candidates SET record=?1 WHERE id=?2",
        params![serde_json::to_vec(record)?, record.request.subject.id],
    )?;
    Ok(())
}

fn validate_candidate_request(request: &CandidateExportRequest, owner: &Ownership) -> Result<()> {
    validate_request(&ExportRequest {
        event_sequence: 1,
        expected_oid: Some(request.expected_oid.clone()),
        metadata: request.metadata.clone(),
    })?;
    let s = &request.subject;
    for value in [
        &s.id,
        &s.attachment,
        &s.context_digest,
        &s.revision,
        &s.source_fingerprint,
        &s.comparison_fingerprint,
    ] {
        crate::store::validate_id(value)?;
    }
    s.change.validate()?;
    if s.endpoint.starts_with("https://") {
        remote::Endpoint::Https {
            url: s.endpoint.clone(),
            trust: remote::HttpsTrust::System,
        }
        .validate()?;
    } else if !Path::new(&s.endpoint).is_absolute() {
        return Err(Error::Invalid(
            "candidate requires canonical HTTPS or absolute local endpoint".into(),
        ));
    }
    if s.endpoint.is_empty()
        || s.endpoint.len() > 4096
        || s.endpoint.chars().any(char::is_control)
        || !crate::hub::github::valid_ref_name(&s.output)
        || s.output == s.target
    {
        return Err(Error::Invalid(
            "invalid candidate endpoint/output identity".into(),
        ));
    }
    git::validate_oid(&s.baseline_commit)?;
    git::validate_oid(&s.parent_commit)?;
    git::validate_oid(&s.source_commit)?;
    if request.expected_oid != s.parent_commit
        || s.change.repository != owner.repository
        || s.target != owner.target
        || s.revision.is_empty()
    {
        return Err(Error::Conflict(
            "candidate subject differs from mirror repository, target, revision or parent".into(),
        ));
    }
    let expected = s.canonical_id()?;
    if s.id != expected {
        return Err(Error::Invalid(
            "candidate subject id is not canonical".into(),
        ));
    }
    Ok(())
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
