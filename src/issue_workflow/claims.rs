//! Trusted-local, bounded journal for issue intake and canonical issue ownership.
//! No method here calls a forge or creates native work.
#[path = "recovery.rs"]
pub mod recovery;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::authority::Authority;
use crate::local_recovery::AccessGuard;
use crate::store::create_directory_durable;
use crate::{Error, Result};

use super::{
    IntakeDecision, IntakeObservation, IntakeOutcome, IntakeRequest, IssueCandidate, IssueIdentity,
    IssueState, MatchBasis, OperatorChoice, evaluate,
};

const APPLICATION_ID: i64 = 0x4147_4957;
const SCHEMA: i64 = 1;
const MAX_JSON: usize = 16 * 1024;
// Validated 100-candidate arguments fit below 100 * 2 KiB + 8 KiB.
// This hash budget is separate from durable JSON and preserves digest bytes.
const MAX_OPERATION_INPUT: usize = 256 * 1024;
const MAX_ROWS: i64 = 4096;
const MAX_ID: usize = 128;

// Canonical DDL is also the reopen contract. SQLite removes only the trailing
// semicolon; comparison deliberately rejects alternate schemas and added objects.
const TABLES: &[(&str, &str)] = &[
    (
        "claims",
        "CREATE TABLE claims(issue_key TEXT PRIMARY KEY,generation INTEGER NOT NULL CHECK(generation>0 AND generation<9223372036854775807),worker_id TEXT,progress_digest TEXT,record_json BLOB NOT NULL CHECK(length(record_json)<=16384)) STRICT",
    ),
    (
        "intakes",
        "CREATE TABLE intakes(key TEXT PRIMARY KEY,request_digest TEXT NOT NULL,generation INTEGER NOT NULL CHECK(generation>0 AND generation<9223372036854775807),selected_issue_key TEXT,create_state TEXT NOT NULL CHECK(create_state IN ('unreserved','reserved_uncertain','observed')),create_operation TEXT,claimed_issue_key TEXT,assessment_conflicted INTEGER NOT NULL CHECK(assessment_conflicted IN (0,1)),record_json BLOB NOT NULL CHECK(length(record_json)<=16384),CHECK((create_state='unreserved' AND selected_issue_key IS NULL AND create_operation IS NULL) OR (create_state='reserved_uncertain' AND selected_issue_key IS NULL AND create_operation IS NOT NULL AND claimed_issue_key IS NULL) OR (create_state='observed' AND selected_issue_key IS NOT NULL AND create_operation IS NOT NULL))) STRICT",
    ),
    (
        "metadata",
        "CREATE TABLE metadata(singleton INTEGER PRIMARY KEY CHECK(singleton=1),native_repository TEXT NOT NULL,installation_id TEXT NOT NULL) STRICT",
    ),
    (
        "operations",
        "CREATE TABLE operations(id TEXT PRIMARY KEY,kind TEXT NOT NULL CHECK(kind IN ('register','register_conflict','reassess','reserve','acquire','progress','release','transfer')),intake_key TEXT REFERENCES intakes(key),issue_key TEXT REFERENCES claims(issue_key),expected_generation INTEGER,request_digest TEXT NOT NULL,expected_intake_generation INTEGER,selected_change_repository TEXT,selected_change_id TEXT,outcome_digest TEXT NOT NULL,outcome_json BLOB NOT NULL CHECK(length(outcome_json)<=16384),CHECK((kind='acquire' AND intake_key IS NOT NULL AND issue_key IS NOT NULL AND expected_intake_generation IS NOT NULL) OR (kind IN ('register','register_conflict','reassess','reserve') AND intake_key IS NOT NULL AND issue_key IS NULL AND expected_intake_generation IS NULL) OR (kind IN ('progress','release','transfer') AND intake_key IS NULL AND issue_key IS NOT NULL AND expected_intake_generation IS NULL)),CHECK((selected_change_repository IS NULL AND selected_change_id IS NULL) OR (kind='acquire' AND selected_change_repository IS NOT NULL AND selected_change_id IS NOT NULL))) STRICT",
    ),
];

fn normalize_ddl(value: &str) -> &str {
    value.trim().trim_end_matches(';').trim_end()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntakeRecord {
    pub key: String,
    pub request: IntakeRequest,
    pub choice: Option<OperatorChoice>,
    pub outcome: IntakeOutcome,
    pub generation: i64,
    pub create_state: CreateState,
    pub create_operation: Option<String>,
    pub selected_issue: Option<IssueIdentity>,
    pub reconciled_observation: Option<IssueCandidate>,
    /// Sticky canonical issue association, retained after release and transfer.
    pub claimed_issue: Option<IssueIdentity>,
    /// A conflicting registration invalidates old actionable evidence until reassessment.
    pub assessment_conflicted: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CreateState {
    Unreserved,
    ReservedUncertain,
    Observed,
}
impl CreateState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unreserved => "unreserved",
            Self::ReservedUncertain => "reserved_uncertain",
            Self::Observed => "observed",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClaimRecord {
    pub issue: IssueIdentity,
    pub generation: i64,
    pub worker_id: Option<String>,
    pub progress_digest: Option<String>,
    pub change: Option<crate::context::ChangeId>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JournalReceipt<T> {
    pub outcome: T,
    pub replay: bool,
    /// Recomputed in the same SQLite snapshot as the historical receipt read.
    pub current_fence: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcquireIntent {
    pub intake_generation: i64,
    pub issue: IssueIdentity,
    pub selected_change: Option<crate::context::ChangeId>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    pub id: String,
    pub kind: String,
    pub intake_key: Option<String>,
    pub issue_key: Option<String>,
    pub expected_generation: Option<i64>,
    pub request_digest: String,
    pub expected_intake_generation: Option<i64>,
    pub selected_change: Option<crate::context::ChangeId>,
    /// Consistency checksum; this does not authenticate authorship.
    pub outcome_digest: String,
    pub outcome: Option<serde_json::Value>,
}

pub struct IssueJournal {
    connection: Connection,
    _lease: AccessGuard,
    native_repository: String,
    installation_id: String,
    path: PathBuf,
}

impl IssueJournal {
    pub fn open(root: &Path, expected_native: &str, installation_id: &str) -> Result<Self> {
        super::validate_native_repository(expected_native)?;
        validate_id(installation_id)?;
        let reader = Authority::open_read_only(root)?;
        if reader.repository_id()? != expected_native {
            return Err(Error::Conflict(
                "issue journal native repository mismatch".into(),
            ));
        }
        let lease = AccessGuard::shared_existing(root)?;
        drop(reader);
        let directory = root.join("issue-workflow");
        // Only the atomic namespace creator may initialize its create-new file.
        // An interrupted initialization intentionally leaves a fail-closed namespace.
        let initialize = match fs::create_dir(&directory) {
            Ok(()) => {
                create_directory_durable(&directory)?;
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error.into()),
        };
        let meta = fs::symlink_metadata(&directory)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(Error::Invalid("invalid issue journal namespace".into()));
        }
        let path = directory.join("state.sqlite");
        if initialize {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.sync_all()?;
            create_directory_durable(&directory)?;
        } else {
            let meta = fs::symlink_metadata(&path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    Error::Conflict("issue journal namespace is orphaned or initializing; retry only after initialization finishes".into())
                } else { error.into() }
            })?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err(Error::Invalid("invalid issue journal database".into()));
            }
        }
        // Never use SQLite CREATE when reopening: file loss must not rearm barriers.
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let mut journal = Self {
            connection,
            _lease: lease,
            native_repository: expected_native.into(),
            installation_id: installation_id.into(),
            path,
        };
        journal.initialize_or_verify(initialize)?;
        journal.validate_existing_rows()?;
        Ok(journal)
    }

    fn validate_existing_rows(&self) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        let count: i64 = tx.query_row("SELECT (SELECT count(*) FROM intakes)+(SELECT count(*) FROM claims)+(SELECT count(*) FROM operations)", [], |r| r.get(0))?;
        if count > MAX_ROWS {
            return Err(Error::Invalid("issue journal row cap exceeded".into()));
        }
        let mut statement = tx.prepare("SELECT key FROM intakes")?;
        let keys = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for key in keys {
            read_intake(&tx, &key, &self.native_repository, &self.installation_id)?;
        }
        let mut statement = tx.prepare("SELECT issue_key FROM claims")?;
        let keys = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for key in keys {
            read_claim(&tx, &key, &self.native_repository)?;
        }
        let mut statement = tx.prepare("SELECT id FROM operations")?;
        let ids = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in ids {
            read_operation(&tx, &id, &self.native_repository, &self.installation_id)?;
        }
        Ok(())
    }

    pub fn database_path(&self) -> &Path {
        &self.path
    }

    fn initialize_or_verify(&mut self, initialize: bool) -> Result<()> {
        let mode: String = self
            .connection
            .pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(Error::Invalid("issue journal requires DELETE mode".into()));
        }
        self.connection.pragma_update(None, "synchronous", "FULL")?;
        self.connection.pragma_update(None, "foreign_keys", "ON")?;
        let synchronous: i64 = self
            .connection
            .pragma_query_value(None, "synchronous", |r| r.get(0))?;
        let foreign_keys: i64 = self
            .connection
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
        if synchronous != 2 || foreign_keys != 1 {
            return Err(Error::Invalid(
                "issue journal safety pragma mismatch".into(),
            ));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let application: i64 = tx.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let schema: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let objects: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT GLOB 'sqlite_*'",
            [],
            |r| r.get(0),
        )?;
        if application == APPLICATION_ID && schema == SCHEMA {
            drop(tx);
            return self.verify();
        }
        if !initialize && application == 0 && schema == 0 && objects == 0 {
            return Err(Error::Conflict("issue journal is empty, truncated or initializing; initialization is forbidden; retry only after a known initializer finishes".into()));
        }
        if !initialize || application != 0 || schema != 0 || objects != 0 {
            return Err(Error::Invalid(
                "existing unknown issue journal schema".into(),
            ));
        }
        tx.execute_batch("PRAGMA application_id=1095190871; PRAGMA user_version=1;")?;
        for (_, ddl) in TABLES {
            tx.execute_batch(ddl)?;
        }
        tx.execute(
            "INSERT INTO metadata VALUES(1,?1,?2)",
            params![self.native_repository, self.installation_id],
        )?;
        tx.commit()?;
        self.verify()
    }

    fn verify(&self) -> Result<()> {
        let application: i64 = self
            .connection
            .pragma_query_value(None, "application_id", |r| r.get(0))?;
        let schema: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if application != APPLICATION_ID || schema != SCHEMA {
            return Err(Error::Invalid("unknown issue journal schema".into()));
        }
        let mode: String = self
            .connection
            .pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(Error::Invalid("issue journal requires DELETE mode".into()));
        }
        self.connection.pragma_update(None, "synchronous", "FULL")?;
        self.connection.pragma_update(None, "foreign_keys", "ON")?;
        let synchronous: i64 = self
            .connection
            .pragma_query_value(None, "synchronous", |r| r.get(0))?;
        let foreign_keys: i64 = self
            .connection
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
        if synchronous != 2 || foreign_keys != 1 {
            return Err(Error::Invalid(
                "issue journal safety pragma mismatch".into(),
            ));
        }
        let mut statement = self.connection.prepare(
            "SELECT type,name,sql FROM sqlite_master WHERE name NOT GLOB 'sqlite_*' ORDER BY name",
        )?;
        let objects = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if objects.len() != TABLES.len()
            || objects
                .iter()
                .zip(TABLES)
                .any(|((kind, name, ddl), (expected, canonical))| {
                    kind != "table"
                        || name.as_str() != *expected
                        || normalize_ddl(ddl) != normalize_ddl(canonical)
                })
        {
            return Err(Error::Invalid(
                "issue journal schema contract mismatch".into(),
            ));
        }
        let mut foreign_keys = self.connection.prepare("PRAGMA foreign_key_check")?;
        if foreign_keys.query([])?.next()?.is_some() {
            return Err(Error::Invalid("issue journal foreign key violation".into()));
        }
        let count: i64 = self
            .connection
            .query_row("SELECT count(*) FROM metadata", [], |r| r.get(0))?;
        let row: (String, String) = self.connection.query_row(
            "SELECT native_repository,installation_id FROM metadata WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if count != 1 || row != (self.native_repository.clone(), self.installation_id.clone()) {
            return Err(Error::Conflict(
                "issue journal installation mismatch".into(),
            ));
        }
        Ok(())
    }

    pub fn intake(&self, key: &str) -> Result<Option<IntakeRecord>> {
        self.verify()?;
        validate_key(key)?;
        let tx = self.connection.unchecked_transaction()?;
        read_intake(&tx, key, &self.native_repository, &self.installation_id)
    }
    pub fn operation(&self, id: &str) -> Result<Option<OperationRecord>> {
        self.verify()?;
        validate_id(id)?;
        let tx = self.connection.unchecked_transaction()?;
        read_operation(&tx, id, &self.native_repository, &self.installation_id)
    }

    pub fn claim(&self, issue: &IssueIdentity) -> Result<Option<ClaimRecord>> {
        self.verify()?;
        issue.validate()?;
        let tx = self.connection.unchecked_transaction()?;
        read_claim(&tx, &issue.stable_key(), &self.native_repository)
    }

    pub fn register(
        &mut self,
        operation: &str,
        request: IntakeRequest,
        observation: IntakeObservation,
        choice: Option<OperatorChoice>,
    ) -> Result<JournalReceipt<IntakeRecord>> {
        self.verify()?;
        validate_id(operation)?;
        request.validate()?;
        observation.validate()?;
        if request.native_repository != self.native_repository {
            return Err(Error::Conflict("native repository mismatch".into()));
        }
        let outcome = evaluate(&request, observation.clone(), choice.as_ref())?;
        let key = intake_key(&self.installation_id, &request);
        let digest = operation_digest(
            &self.native_repository,
            &self.installation_id,
            &request.repository.stable_key(),
            "register",
            &key,
            None,
            &(&request, &observation, &choice),
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(saved) = read_operation(
            &tx,
            operation,
            &self.native_repository,
            &self.installation_id,
        )? && saved.kind == "register_conflict"
        {
            if saved.intake_key.as_deref() != Some(key.as_str()) || saved.request_digest != digest {
                return Err(Error::Conflict(
                    "operation ID reused with different input".into(),
                ));
            }
            return Err(Error::Conflict(
                "registration previously contradicted retained assessment; reload and reassess"
                    .into(),
            ));
        }
        if let Some(saved) = replay::<IntakeRecord>(
            &tx,
            operation,
            "register",
            &key,
            None,
            &digest,
            &self.native_repository,
            &self.installation_id,
        )? {
            validate_intake_receipt(&saved, &self.native_repository, &self.installation_id)?;
            tx.commit()?;
            return Ok(JournalReceipt {
                outcome: saved,
                replay: true,
                current_fence: false,
            });
        }
        let existing = read_intake(&tx, &key, &self.native_repository, &self.installation_id)?;
        let record = if let Some(mut existing) = existing {
            if existing.request != request
                || existing.choice != choice
                || !equivalent_outcome(&existing.outcome, &outcome)?
            {
                let mut invalidated_generation = None;
                if !existing.assessment_conflicted {
                    let prior = existing.generation;
                    existing.assessment_conflicted = true;
                    existing.generation = advance(prior)?;
                    update_intake(&tx, &existing, prior)?;
                    invalidated_generation = Some(prior);
                }
                // Invalidation updates an existing row even at capacity. If there
                // is no receipt slot, the rejected ID is not consumed, but the
                // sticky conflict marker still prevents stale or reloaded use.
                match capacity(&tx, 1) {
                    Ok(()) => match insert_operation(
                        &tx,
                        operation,
                        "register_conflict",
                        Some(key.as_str()),
                        None,
                        invalidated_generation,
                        &digest,
                        None,
                        None,
                        Some(&existing),
                    ) {
                        Ok(()) => {}
                        Err(error @ Error::Limit(_)) => {
                            tx.commit()?;
                            return Err(error);
                        }
                        Err(error) => return Err(error),
                    },
                    Err(Error::Limit(_)) => {}
                    Err(error) => return Err(error),
                }
                tx.commit()?;
                return Err(Error::Conflict("registration contradicts retained assessment; generation-checked reassessment required".into()));
            }
            if existing.assessment_conflicted {
                return Err(Error::Conflict("intake requires reassessment".into()));
            }
            existing
        } else {
            let record = IntakeRecord {
                key: key.clone(),
                request,
                choice,
                outcome,
                generation: 1,
                create_state: CreateState::Unreserved,
                create_operation: None,
                selected_issue: None,
                reconciled_observation: None,
                claimed_issue: None,
                assessment_conflicted: false,
            };
            capacity(&tx, 2)?;
            insert_intake(&tx, &record)?;
            record
        };

        capacity(&tx, 1)?;
        insert_operation(
            &tx,
            operation,
            "register",
            Some(key.as_str()),
            None,
            None,
            &digest,
            None,
            None,
            Some(&record),
        )?;
        tx.commit()?;
        Ok(JournalReceipt {
            outcome: record,
            replay: false,
            current_fence: false,
        })
    }

    pub fn reassess(
        &mut self,
        operation: &str,
        key: &str,
        expected: i64,
        request: IntakeRequest,
        observation: IntakeObservation,
        choice: Option<OperatorChoice>,
    ) -> Result<JournalReceipt<IntakeRecord>> {
        self.verify()?;
        validate_id(operation)?;
        validate_key(key)?;
        request.validate()?;
        observation.validate()?;
        let outcome = evaluate(&request, observation.clone(), choice.as_ref())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = read_intake(&tx, key, &self.native_repository, &self.installation_id)?
            .ok_or_else(|| Error::NotFound("issue intake".into()))?;
        if !same_request_scope(&record.request, &request)
            || intake_key(&self.installation_id, &request) != key
        {
            return Err(Error::Conflict(
                "reassessment request scope mismatch".into(),
            ));
        }
        let digest = operation_digest(
            &self.native_repository,
            &self.installation_id,
            &request.repository.stable_key(),
            "reassess",
            key,
            Some(expected),
            &(&request, &observation, &choice),
        )?;
        if let Some(saved) = replay::<IntakeRecord>(
            &tx,
            operation,
            "reassess",
            key,
            Some(expected),
            &digest,
            &self.native_repository,
            &self.installation_id,
        )? {
            validate_intake_receipt(&saved, &self.native_repository, &self.installation_id)?;
            tx.commit()?;
            return Ok(JournalReceipt {
                outcome: saved,
                replay: true,
                current_fence: false,
            });
        }
        if record.generation != expected || record.create_state == CreateState::ReservedUncertain {
            return Err(Error::Conflict(
                "intake is stale or create is reserved".into(),
            ));
        }
        let fixed = record
            .selected_issue
            .as_ref()
            .or(record.claimed_issue.as_ref());
        let mut refreshed = record.clone();
        refreshed.request = request;
        refreshed.choice = choice;
        refreshed.outcome = outcome;
        refreshed.assessment_conflicted = false;
        let proof = if let Some(bound) = fixed {
            match evidence_issue(&refreshed) {
                Ok(issue) if issue.stable_key() == bound.stable_key() => {
                    let existing = read_claim(&tx, &issue.stable_key(), &self.native_repository)?;
                    let change = match &refreshed.outcome.decision {
                        IntakeDecision::ExistingWork { change } => Some(change),
                        _ => None,
                    };
                    !existing
                        .as_ref()
                        .and_then(|c| c.change.as_ref())
                        .is_some_and(|old| change.is_some_and(|new| old != new))
                }
                _ => false,
            }
        } else {
            true
        };
        refreshed.generation = advance(expected)?;
        if proof && refreshed.claimed_issue.is_some() {
            refreshed.claimed_issue = Some(evidence_issue(&refreshed)?);
        }
        // Preflight the complete replacement, including display and generation.
        let fits = match encode(&refreshed) {
            Ok(_) => true,
            Err(Error::Limit(_)) => false,
            Err(error) => return Err(error),
        };
        if !proof || !fits {
            // A valid negative assessment is a durable blocked receipt retaining
            // prior evidence. A failed novel ID at capacity is never consumed.
            match capacity(&tx, 1) {
                Ok(()) => {}
                Err(error @ Error::Limit(_)) => {
                    if !record.assessment_conflicted {
                        mark_conflicted(&tx, &mut record)?;
                    }
                    tx.commit()?;
                    return Err(error);
                }
                Err(error) => return Err(error),
            }
            let mut blocked = record.clone();
            blocked.assessment_conflicted = true;
            blocked.generation = advance(expected)?;
            if let Err(error) = encode(&blocked) {
                if !record.assessment_conflicted {
                    mark_conflicted(&tx, &mut record)?;
                }
                tx.commit()?;
                return Err(error);
            }
            record = blocked;
        } else {
            capacity(&tx, 1)?;
            record = refreshed;
        }
        update_intake(&tx, &record, expected)?;
        insert_operation(
            &tx,
            operation,
            "reassess",
            Some(key),
            None,
            Some(expected),
            &digest,
            None,
            None,
            Some(&record),
        )?;
        tx.commit()?;
        Ok(JournalReceipt {
            outcome: record,
            replay: false,
            current_fence: false,
        })
    }

    pub fn reserve_create(
        &mut self,
        operation: &str,
        key: &str,
        expected: i64,
    ) -> Result<JournalReceipt<IntakeRecord>> {
        self.verify()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = read_intake(&tx, key, &self.native_repository, &self.installation_id)?
            .ok_or_else(|| Error::NotFound("issue intake".into()))?;
        let digest = operation_digest(
            &self.native_repository,
            &self.installation_id,
            &record.request.repository.stable_key(),
            "reserve",
            key,
            Some(expected),
            &(),
        )?;
        if let Some(saved) = replay::<IntakeRecord>(
            &tx,
            operation,
            "reserve",
            key,
            Some(expected),
            &digest,
            &self.native_repository,
            &self.installation_id,
        )? {
            validate_intake_receipt(&saved, &self.native_repository, &self.installation_id)?;
            tx.commit()?;
            return Ok(JournalReceipt {
                outcome: saved,
                replay: true,
                current_fence: false,
            });
        }
        if record.generation != expected
            || record.create_state != CreateState::Unreserved
            || record.claimed_issue.is_some()
            || record.assessment_conflicted
            || !matches!(&record.outcome.decision, IntakeDecision::CreateEligible {})
        {
            return Err(Error::Conflict("create reservation is unavailable".into()));
        }
        capacity(&tx, 1)?;
        record.generation = advance(expected)?;
        record.create_state = CreateState::ReservedUncertain;
        record.create_operation = Some(operation.into());
        update_intake(&tx, &record, expected)?;
        insert_operation(
            &tx,
            operation,
            "reserve",
            Some(key),
            None,
            Some(expected),
            &digest,
            None,
            None,
            Some(&record),
        )?;
        tx.commit()?;
        Ok(JournalReceipt {
            outcome: record,
            replay: false,
            current_fence: false,
        })
    }

    /// Reconciles the reserved intake using existing rows, even at the aggregate row cap.
    pub fn reconcile_create(
        &mut self,
        reservation: &str,
        key: &str,
        issue: IssueIdentity,
        observed: IssueCandidate,
    ) -> Result<IntakeRecord> {
        self.verify()?;
        validate_id(reservation)?;
        validate_key(key)?;
        issue.validate()?;
        observed.identity.validate()?;
        super::validate_digest(&observed.request_digest)?;
        if let Some(proposal) = &observed.related_proposal {
            proposal.validate()?;
            if !proposal.repository.same_repository(&issue.repository) {
                return Err(Error::Conflict(
                    "reconciled proposal repository mismatch".into(),
                ));
            }
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = read_intake(&tx, key, &self.native_repository, &self.installation_id)?
            .ok_or_else(|| Error::NotFound("issue intake".into()))?;
        if observed.identity.stable_key() != issue.stable_key()
            || observed.basis != MatchBasis::ExactRequest
            || observed.request_digest != record.request.accepted_request_sha256
            || record.create_operation.as_deref() != Some(reservation)
            || !record.request.repository.same_repository(&issue.repository)
        {
            return Err(Error::Conflict("create reservation mismatch".into()));
        }
        if record.create_state == CreateState::Observed {
            if record
                .selected_issue
                .as_ref()
                .map(IssueIdentity::stable_key)
                != Some(issue.stable_key())
            {
                return Err(Error::Conflict(
                    "conflicting reconciled issue selector".into(),
                ));
            }
            let old = record
                .reconciled_observation
                .as_ref()
                .ok_or_else(|| Error::Invalid("missing reconciliation".into()))?;
            if old.state == observed.state
                && old.basis == observed.basis
                && old.request_digest == observed.request_digest
                && old
                    .related_proposal
                    .as_ref()
                    .map(super::ProposalIdentity::stable_key)
                    == observed
                        .related_proposal
                        .as_ref()
                        .map(super::ProposalIdentity::stable_key)
            {
                tx.commit()?;
                return Ok(record);
            }
            mark_conflicted(&tx, &mut record)?;
            tx.commit()?;
            return Err(Error::Conflict(
                "reconciliation contradicts retained assessment".into(),
            ));
        }
        if record.create_state != CreateState::ReservedUncertain {
            return Err(Error::Conflict("create is not reserved".into()));
        }
        if observed.state != IssueState::Open {
            mark_conflicted(&tx, &mut record)?;
            tx.commit()?;
            return Err(Error::Conflict(
                "reserved issue is closed; create remains uncertain".into(),
            ));
        }
        let prior = record.generation;
        let mut reconciled = record.clone();
        reconciled.generation = advance(prior)?;
        reconciled.create_state = CreateState::Observed;
        reconciled.selected_issue = Some(issue);
        reconciled.reconciled_observation = Some(observed);
        // Keep prior uncertainty blocked when valid evidence cannot be retained.
        // This updates an existing row without adding a reconciliation receipt.
        match encode(&reconciled) {
            Ok(_) => {}
            Err(error @ Error::Limit(_)) => {
                mark_conflicted(&tx, &mut record)?;
                tx.commit()?;
                return Err(error);
            }
            Err(error) => return Err(error),
        }
        update_intake(&tx, &reconciled, prior)?;
        tx.commit()?;
        Ok(reconciled)
    }

    pub fn acquire(
        &mut self,
        operation: &str,
        intake_key: &str,
        worker: &str,
        expected: Option<i64>,
        intent: &AcquireIntent,
    ) -> Result<JournalReceipt<ClaimRecord>> {
        self.verify()?;
        validate_id(worker)?;
        validate_key(intake_key)?;
        intent.issue.validate()?;
        if intent.intake_generation <= 0 || intent.intake_generation == i64::MAX {
            return Err(Error::Invalid("invalid intake generation".into()));
        }
        if let Some(change) = &intent.selected_change {
            validate_change(change, &self.native_repository)?;
        }
        let key = intent.issue.stable_key();
        let digest = operation_digest(
            &self.native_repository,
            &self.installation_id,
            &intent.issue.repository.stable_key(),
            "acquire",
            &key,
            expected,
            &(intake_key, worker, intent),
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Resolve immutable operation scope before consulting mutable intake evidence.
        if let Some(saved_operation) = read_operation(
            &tx,
            operation,
            &self.native_repository,
            &self.installation_id,
        )? {
            if saved_operation.kind != "acquire"
                || saved_operation.intake_key.as_deref() != Some(intake_key)
                || saved_operation.issue_key.as_deref() != Some(key.as_str())
                || saved_operation.expected_generation != expected
                || saved_operation.expected_intake_generation != Some(intent.intake_generation)
                || saved_operation.selected_change != intent.selected_change
                || saved_operation.request_digest != digest
            {
                return Err(Error::Conflict(
                    "operation ID reused with different input".into(),
                ));
            }
            let saved: ClaimRecord = serde_json::from_value(
                saved_operation
                    .outcome
                    .ok_or_else(|| Error::Invalid("missing operation outcome".into()))?,
            )?;
            let current = read_claim(&tx, &key, &self.native_repository)?;
            let fence = current.as_ref().is_some_and(|r| same_fence(r, &saved));
            tx.commit()?;
            return Ok(JournalReceipt {
                outcome: saved,
                replay: true,
                current_fence: fence,
            });
        }
        let mut intake = read_intake(
            &tx,
            intake_key,
            &self.native_repository,
            &self.installation_id,
        )?
        .ok_or_else(|| Error::NotFound("issue intake".into()))?;
        // Every new owner must remain invalidatable, including sticky reacquisition.
        advance(intake.generation)?;
        let issue = claimable_issue(&intake)?;
        let selected_change =
            if let IntakeDecision::ExistingWork { change } = &intake.outcome.decision {
                Some(change.clone())
            } else {
                None
            };
        if intake.generation != intent.intake_generation
            || issue.stable_key() != key
            || selected_change != intent.selected_change
            || intake
                .claimed_issue
                .as_ref()
                .is_some_and(|old| old.stable_key() != key)
        {
            return Err(Error::Conflict("stale intake selection".into()));
        }
        let current = read_claim(&tx, &key, &self.native_repository)?;
        if current.as_ref().map(|r| r.generation) != expected
            || current.as_ref().is_some_and(|r| r.worker_id.is_some())
        {
            return Err(Error::Conflict(
                "canonical issue claim is stale or owned".into(),
            ));
        }
        capacity(&tx, 1 + if current.is_none() { 1 } else { 0 })?;
        let mut claim = current.unwrap_or(ClaimRecord {
            issue: issue.clone(),
            generation: 0,
            worker_id: None,
            progress_digest: None,
            change: None,
        });
        if let Some(change) = selected_change {
            if claim.change.as_ref().is_some_and(|old| old != &change) {
                return Err(Error::Conflict(
                    "native Change association is immutable".into(),
                ));
            }
            claim.change = Some(change);
        }
        if intake.claimed_issue.is_none() {
            let prior = intake.generation;
            intake.claimed_issue = Some(issue.clone());
            intake.generation = advance(prior)?;
            update_intake(&tx, &intake, prior)?;
        }
        let prior = (claim.generation, claim.worker_id.clone());
        claim.issue = issue.clone();
        claim.generation = advance(claim.generation)?;
        claim.worker_id = Some(worker.into());
        write_claim(&tx, &claim, prior)?;
        insert_operation(
            &tx,
            operation,
            "acquire",
            Some(intake_key),
            Some(key.as_str()),
            expected,
            &digest,
            Some(intent.intake_generation),
            intent.selected_change.as_ref(),
            Some(&claim),
        )?;
        tx.commit()?;
        Ok(JournalReceipt {
            current_fence: true,
            outcome: claim,
            replay: false,
        })
    }

    pub fn mutate_claim(
        &mut self,
        operation: &str,
        issue: &IssueIdentity,
        worker: &str,
        expected: i64,
        action: ClaimAction,
    ) -> Result<JournalReceipt<ClaimRecord>> {
        self.verify()?;
        issue.validate()?;
        validate_id(worker)?;
        action.validate(&self.native_repository)?;
        let key = issue.stable_key();
        let kind = action.kind();
        let digest = operation_digest(
            &self.native_repository,
            &self.installation_id,
            &issue.repository.stable_key(),
            kind,
            &key,
            Some(expected),
            &(worker, &action),
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(saved) = replay::<ClaimRecord>(
            &tx,
            operation,
            kind,
            &key,
            Some(expected),
            &digest,
            &self.native_repository,
            &self.installation_id,
        )? {
            validate_claim_receipt(&saved, &self.native_repository)?;
            let current = read_claim(&tx, &key, &self.native_repository)?;
            let fence = current.as_ref().is_some_and(|r| same_fence(r, &saved));
            tx.commit()?;
            return Ok(JournalReceipt {
                outcome: saved,
                replay: true,
                current_fence: fence,
            });
        }
        let mut claim = read_claim(&tx, &key, &self.native_repository)?
            .ok_or_else(|| Error::NotFound("issue claim".into()))?;
        if claim.issue.stable_key() != key
            || claim.generation != expected
            || claim.worker_id.as_deref() != Some(worker)
        {
            return Err(Error::Conflict("stale issue claim fence".into()));
        }
        capacity(&tx, 1)?;
        let prior = (claim.generation, claim.worker_id.clone());
        claim.generation = advance(expected)?;
        match action {
            ClaimAction::Progress { digest, change } => {
                claim.progress_digest = Some(digest);
                if let Some(change) = change {
                    if claim.change.as_ref().is_some_and(|old| old != &change) {
                        return Err(Error::Conflict(
                            "native Change association is immutable".into(),
                        ));
                    }
                    claim.change = Some(change);
                }
            }
            ClaimAction::Release => claim.worker_id = None,
            ClaimAction::Transfer { worker_id } => claim.worker_id = Some(worker_id),
        }
        write_claim(&tx, &claim, prior)?;
        insert_operation(
            &tx,
            operation,
            kind,
            None,
            Some(key.as_str()),
            Some(expected),
            &digest,
            None,
            None,
            Some(&claim),
        )?;
        tx.commit()?;
        Ok(JournalReceipt {
            current_fence: claim.worker_id.is_some(),
            outcome: claim,
            replay: false,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClaimAction {
    Progress {
        digest: String,
        change: Option<crate::context::ChangeId>,
    },
    Release,
    Transfer {
        worker_id: String,
    },
}
impl ClaimAction {
    fn kind(&self) -> &'static str {
        match self {
            Self::Progress { .. } => "progress",
            Self::Release => "release",
            Self::Transfer { .. } => "transfer",
        }
    }
    fn validate(&self, native: &str) -> Result<()> {
        match self {
            Self::Progress { digest, change } => {
                super::validate_digest(digest)?;
                if let Some(change) = change {
                    change.validate()?;
                    if change.repository != native {
                        return Err(Error::Conflict("native Change repository mismatch".into()));
                    }
                }
            }
            Self::Transfer { worker_id } => validate_id(worker_id)?,
            Self::Release => {}
        }
        Ok(())
    }
}

fn same_fence(current: &ClaimRecord, saved: &ClaimRecord) -> bool {
    current.issue.stable_key() == saved.issue.stable_key()
        && current.worker_id == saved.worker_id
        && current.generation == saved.generation
        && current.worker_id.is_some()
}
fn advance(value: i64) -> Result<i64> {
    value
        .checked_add(1)
        .filter(|v| *v < i64::MAX)
        .ok_or_else(|| Error::Limit("journal generation exhausted".into()))
}
fn validate_key(value: &str) -> Result<()> {
    super::validate_digest(value)
}
fn validate_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_ID
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(Error::Invalid("invalid journal identifier".into()));
    }
    Ok(())
}
fn hash(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for part in parts {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part);
    }
    format!("{:x}", h.finalize())
}
fn intake_key(installation: &str, request: &IntakeRequest) -> String {
    hash(&[
        b"agentscm.issue-intake.v1",
        installation.as_bytes(),
        request.native_repository.as_bytes(),
        request.repository.stable_key().as_bytes(),
        request.accepted_request_sha256.as_bytes(),
    ])
}
fn operation_digest<T: Serialize>(
    native: &str,
    installation: &str,
    repo: &str,
    kind: &str,
    target: &str,
    expected: Option<i64>,
    args: &T,
) -> Result<String> {
    let bytes = serde_json::to_vec(args)?;
    if bytes.len() > MAX_OPERATION_INPUT {
        return Err(Error::Limit("issue operation input exceeds 256 KiB".into()));
    }
    Ok(hash(&[
        b"agentscm.issue-operation.v1",
        native.as_bytes(),
        installation.as_bytes(),
        repo.as_bytes(),
        kind.as_bytes(),
        target.as_bytes(),
        expected.unwrap_or(-1).to_string().as_bytes(),
        &bytes,
    ]))
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_JSON {
        return Err(Error::Limit("issue journal JSON exceeds 16 KiB".into()));
    }
    Ok(bytes)
}
fn decode<T: DeserializeOwned>(bytes: Vec<u8>) -> Result<T> {
    if bytes.len() > MAX_JSON {
        return Err(Error::Invalid("oversized stored issue JSON".into()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn capacity(tx: &Transaction<'_>, additional: i64) -> Result<()> {
    let total:i64=tx.query_row("SELECT (SELECT count(*) FROM intakes)+(SELECT count(*) FROM claims)+(SELECT count(*) FROM operations)",[],|r|r.get(0))?;
    if total.checked_add(additional).is_none_or(|n| n > MAX_ROWS) {
        return Err(Error::Limit(
            "issue journal aggregate row cap reached".into(),
        ));
    }
    Ok(())
}
fn read_blob<T: DeserializeOwned>(
    conn: &Connection,
    table: &str,
    key_column: &str,
    key: &str,
    column: &str,
) -> Result<Option<T>> {
    let query = format!("SELECT length({column}),{column} FROM {table} WHERE {key_column}=?1");
    let row: Option<(i64, Vec<u8>)> = conn
        .query_row(&query, [key], |r| {
            let length: i64 = r.get(0)?;
            if length < 0 || length as usize > MAX_JSON {
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok((length, r.get(1)?))
        })
        .optional()?;
    row.map(|(length, bytes)| {
        if length as usize != bytes.len() {
            return Err(Error::Invalid("stored JSON length mismatch".into()));
        }
        decode(bytes)
    })
    .transpose()
}
fn read_intake(
    conn: &Connection,
    key: &str,
    native: &str,
    installation: &str,
) -> Result<Option<IntakeRecord>> {
    let record: Option<IntakeRecord> = read_blob(conn, "intakes", "key", key, "record_json")?;
    if let Some(ref record) = record {
        record.request.validate()?;
        if record.request.native_repository != native
            || record.key != key
            || record.key != intake_key(installation, &record.request)
        {
            return Err(Error::Invalid("stored intake binding mismatch".into()));
        }
        validate_intake_receipt(record, native, installation)?;
        let row:(String,i64,Option<String>,String,Option<String>,Option<String>,bool)=conn.query_row("SELECT request_digest,generation,selected_issue_key,create_state,create_operation,claimed_issue_key,assessment_conflicted FROM intakes WHERE key=?1",[key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
        if row.0 != record.request.accepted_request_sha256
            || row.1 != record.generation
            || row.2
                != record
                    .selected_issue
                    .as_ref()
                    .map(IssueIdentity::stable_key)
            || row.3 != record.create_state.as_str()
            || row.4 != record.create_operation
            || row.5 != record.claimed_issue.as_ref().map(IssueIdentity::stable_key)
            || row.6 != record.assessment_conflicted
        {
            return Err(Error::Invalid("stored intake scalar/JSON mismatch".into()));
        }
        if let Some(operation) = &record.create_operation {
            validate_id(operation)?;
            let owner: Option<(String, String)> = conn
                .query_row(
                    "SELECT kind,intake_key FROM operations WHERE id=?1",
                    [operation],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if owner.as_ref() != Some(&(String::from("reserve"), key.to_owned())) {
                return Err(Error::Invalid("stored create operation mismatch".into()));
            }
        }
        if let Some(issue) = &record.claimed_issue
            && read_claim(conn, &issue.stable_key(), native)?.is_none()
        {
            return Err(Error::Invalid(
                "claimed intake has no canonical claim".into(),
            ));
        }
    }
    Ok(record)
}
fn insert_intake(tx: &Transaction<'_>, r: &IntakeRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO intakes VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            r.key,
            r.request.accepted_request_sha256,
            r.generation,
            r.selected_issue.as_ref().map(IssueIdentity::stable_key),
            r.create_state.as_str(),
            r.create_operation,
            r.claimed_issue.as_ref().map(IssueIdentity::stable_key),
            r.assessment_conflicted,
            encode(r)?
        ],
    )?;
    Ok(())
}
fn update_intake(tx: &Transaction<'_>, r: &IntakeRecord, expected: i64) -> Result<()> {
    let changed=tx.execute("UPDATE intakes SET generation=?3,selected_issue_key=?4,create_state=?5,create_operation=?6,claimed_issue_key=?7,assessment_conflicted=?8,record_json=?9 WHERE key=?1 AND generation=?2",params![r.key,expected,r.generation,r.selected_issue.as_ref().map(IssueIdentity::stable_key),r.create_state.as_str(),r.create_operation,r.claimed_issue.as_ref().map(IssueIdentity::stable_key),r.assessment_conflicted,encode(r)?])?;
    if changed != 1 {
        return Err(Error::Conflict("stale intake generation".into()));
    }
    Ok(())
}
fn read_claim(conn: &Connection, key: &str, native: &str) -> Result<Option<ClaimRecord>> {
    let record: Option<ClaimRecord> = read_blob(conn, "claims", "issue_key", key, "record_json")?;
    if let Some(ref record) = record {
        validate_claim_receipt(record, native)?;
        if let Some(worker) = &record.worker_id {
            validate_id(worker)?;
        }
        if let Some(digest) = &record.progress_digest {
            super::validate_digest(digest)?;
        }
        if let Some(change) = &record.change {
            change.validate()?;
            if change.repository != native {
                return Err(Error::Invalid("stored native Change scope mismatch".into()));
            }
        }
        let row: (i64, Option<String>, Option<String>) = conn.query_row(
            "SELECT generation,worker_id,progress_digest FROM claims WHERE issue_key=?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if record.issue.stable_key() != key
            || row
                != (
                    record.generation,
                    record.worker_id.clone(),
                    record.progress_digest.clone(),
                )
        {
            return Err(Error::Invalid("stored claim scalar/JSON mismatch".into()));
        }
    }
    Ok(record)
}
fn write_claim(tx: &Transaction<'_>, r: &ClaimRecord, prior: (i64, Option<String>)) -> Result<()> {
    let bytes = encode(r)?;
    let changed = if prior.0 == 0 {
        tx.execute(
            "INSERT INTO claims VALUES(?1,?2,?3,?4,?5)",
            params![
                r.issue.stable_key(),
                r.generation,
                r.worker_id,
                r.progress_digest,
                bytes
            ],
        )?
    } else {
        tx.execute("UPDATE claims SET generation=?4,worker_id=?5,progress_digest=?6,record_json=?7 WHERE issue_key=?1 AND generation=?2 AND worker_id IS ?3",params![r.issue.stable_key(),prior.0,prior.1,r.generation,r.worker_id,r.progress_digest,bytes])?
    };
    if changed != 1 {
        return Err(Error::Conflict("stale issue claim fence".into()));
    }
    Ok(())
}

fn operation_checksum(record: &OperationRecord, bytes: &[u8]) -> Result<String> {
    let scope = encode(&(
        &record.id,
        &record.kind,
        &record.intake_key,
        &record.issue_key,
        record.expected_generation,
        &record.request_digest,
        record.expected_intake_generation,
        &record.selected_change,
    ))?;
    Ok(hash(&[b"agentscm.issue-outcome.v1", &scope, bytes]))
}

// The fixed SQL operation row scope is intentionally passed explicitly.
#[allow(clippy::too_many_arguments)]
fn insert_operation<T: Serialize>(
    tx: &Transaction<'_>,
    id: &str,
    kind: &str,
    intake: Option<&str>,
    issue: Option<&str>,
    expected: Option<i64>,
    digest: &str,
    intake_generation: Option<i64>,
    change: Option<&crate::context::ChangeId>,
    outcome: Option<&T>,
) -> Result<()> {
    validate_id(id)?;
    let bytes = encode(
        outcome.ok_or_else(|| Error::Invalid("operation requires terminal outcome".into()))?,
    )?;
    let record = OperationRecord {
        id: id.into(),
        kind: kind.into(),
        intake_key: intake.map(str::to_owned),
        issue_key: issue.map(str::to_owned),
        expected_generation: expected,
        request_digest: digest.into(),
        expected_intake_generation: intake_generation,
        selected_change: change.cloned(),
        outcome_digest: String::new(),
        outcome: None,
    };
    let checksum = operation_checksum(&record, &bytes)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            id,
            kind,
            intake,
            issue,
            expected,
            digest,
            intake_generation,
            change.map(|c| c.repository.as_str()),
            change.map(|c| c.id.as_str()),
            checksum,
            bytes
        ],
    )?;
    Ok(())
}

fn read_operation(
    conn: &Connection,
    id: &str,
    native: &str,
    installation: &str,
) -> Result<Option<OperationRecord>> {
    validate_id(id)?;
    let row=conn.query_row("SELECT kind,intake_key,issue_key,expected_generation,request_digest,expected_intake_generation,selected_change_repository,selected_change_id,outcome_digest,length(outcome_json) FROM operations WHERE id=?1",[id],|r| {
        Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<i64>>(5)?,r.get::<_,Option<String>>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,String>(8)?,r.get::<_,Option<i64>>(9)?))
    }).optional()?;
    let Some((
        kind,
        intake_key,
        issue_key,
        expected_generation,
        request_digest,
        expected_intake_generation,
        change_repository,
        change_id,
        outcome_digest,
        length,
    )) = row
    else {
        return Ok(None);
    };
    let length =
        length.ok_or_else(|| Error::Invalid("operation has no terminal outcome".into()))?;
    if length <= 0 || length as usize > MAX_JSON {
        return Err(Error::Invalid(
            "stored operation outcome too large or empty".into(),
        ));
    }
    let selected_change = match (change_repository, change_id) {
        (None, None) => None,
        (Some(repository), Some(id)) => Some(crate::context::ChangeId { repository, id }),
        _ => return Err(Error::Invalid("partial operation Change selector".into())),
    };
    let bytes: Vec<u8> = conn.query_row(
        "SELECT outcome_json FROM operations WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    if bytes.len() != length as usize {
        return Err(Error::Invalid("stored operation length mismatch".into()));
    }
    let mut record = OperationRecord {
        id: id.into(),
        kind,
        intake_key,
        issue_key,
        expected_generation,
        request_digest,
        expected_intake_generation,
        selected_change,
        outcome_digest,
        outcome: None,
    };
    super::validate_digest(&record.outcome_digest)?;
    if operation_checksum(&record, &bytes)? != record.outcome_digest {
        return Err(Error::Invalid(
            "operation outcome consistency mismatch".into(),
        ));
    }
    record.outcome = Some(decode(bytes)?);
    validate_operation(conn, &record, native, installation)?;
    Ok(Some(record))
}

fn validate_operation(
    conn: &Connection,
    operation: &OperationRecord,
    native: &str,
    installation: &str,
) -> Result<()> {
    super::validate_digest(&operation.request_digest)?;
    if operation
        .expected_generation
        .is_some_and(|n| n <= 0 || n == i64::MAX)
    {
        return Err(Error::Invalid("invalid operation generation".into()));
    }
    let value = operation
        .outcome
        .clone()
        .ok_or_else(|| Error::Invalid("missing terminal outcome".into()))?;
    match operation.kind.as_str() {
        "register" | "register_conflict" | "reassess" | "reserve" => {
            if operation.intake_key.is_none()
                || operation.issue_key.is_some()
                || operation.expected_intake_generation.is_some()
                || operation.selected_change.is_some()
            {
                return Err(Error::Invalid("invalid intake operation selectors".into()));
            }
            let record: IntakeRecord = serde_json::from_value(value)?;
            validate_intake_receipt(&record, native, installation)?;
            if operation.intake_key.as_deref() != Some(record.key.as_str()) {
                return Err(Error::Invalid("operation intake target mismatch".into()));
            }
            if let Some(reservation) = &record.create_operation {
                let owner: Option<(String, Option<String>, Option<i64>)> = conn
                    .query_row(
                        "SELECT kind,intake_key,expected_generation FROM operations WHERE id=?1",
                        [reservation],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                if !owner.is_some_and(|(kind, key, generation)| {
                    kind == "reserve"
                        && key.as_deref() == Some(record.key.as_str())
                        && generation.is_some_and(|n| {
                            n > 0 && advance(n).is_ok_and(|reserved| reserved <= record.generation)
                        })
                }) {
                    return Err(Error::Invalid(
                        "historical reservation scope mismatch".into(),
                    ));
                }
            }
            // Only immutable request scope is compared; historical evidence and
            // generation need not equal the latest assessment or create state.
            let current = read_intake(conn, &record.key, native, installation)?
                .ok_or_else(|| Error::Invalid("orphan intake operation".into()))?;
            if !same_request_scope(&current.request, &record.request)
                || current.generation < record.generation
            {
                return Err(Error::Invalid("operation intake history mismatch".into()));
            }
            match operation.kind.as_str() {
                "register"
                    if operation.expected_generation.is_none() && !record.assessment_conflicted => {
                }
                "register_conflict"
                    if operation.expected_generation.is_none() && record.assessment_conflicted => {}
                "register_conflict"
                    if record.assessment_conflicted
                        && operation
                            .expected_generation
                            .is_some_and(|n| advance(n).ok() == Some(record.generation)) => {}
                "reassess"
                    if record.create_state != CreateState::ReservedUncertain
                        && operation
                            .expected_generation
                            .is_some_and(|n| advance(n).ok() == Some(record.generation)) =>
                {
                    if record.create_state == CreateState::Observed && !record.assessment_conflicted
                    {
                        evidence_issue(&record)?;
                    }
                }
                "reserve"
                    if record.create_state == CreateState::ReservedUncertain
                        && record.create_operation.as_deref() == Some(operation.id.as_str())
                        && operation
                            .expected_generation
                            .is_some_and(|n| advance(n).ok() == Some(record.generation)) => {}
                _ => {
                    return Err(Error::Invalid(
                        "operation intake transition mismatch".into(),
                    ));
                }
            }
        }
        "acquire" | "progress" | "release" | "transfer" => {
            if operation.issue_key.is_none() {
                return Err(Error::Invalid("missing claim operation selector".into()));
            }
            let record: ClaimRecord = serde_json::from_value(value)?;
            validate_claim_receipt(&record, native)?;
            if operation.issue_key.as_deref() != Some(record.issue.stable_key().as_str()) {
                return Err(Error::Invalid("operation claim target mismatch".into()));
            }
            let expected = operation.expected_generation.unwrap_or(0);
            if advance(expected)? != record.generation {
                return Err(Error::Invalid("operation claim transition mismatch".into()));
            }
            let current = read_claim(conn, &record.issue.stable_key(), native)?
                .ok_or_else(|| Error::Invalid("orphan claim operation".into()))?;
            if current.generation < record.generation {
                return Err(Error::Invalid("operation claim history mismatch".into()));
            }
            if operation.kind == "acquire" {
                let key = operation
                    .intake_key
                    .as_deref()
                    .ok_or_else(|| Error::Invalid("missing acquire intake selector".into()))?;
                validate_key(key)?;
                let generation = operation
                    .expected_intake_generation
                    .ok_or_else(|| Error::Invalid("missing acquire intake generation".into()))?;
                if generation <= 0 || generation == i64::MAX || record.worker_id.is_none() {
                    return Err(Error::Invalid("invalid acquire outcome".into()));
                }
                let intake = read_intake(conn, key, native, installation)?
                    .ok_or_else(|| Error::Invalid("orphan acquire intake".into()))?;
                if intake.generation < generation
                    || !intake
                        .request
                        .repository
                        .same_repository(&record.issue.repository)
                    || intake.claimed_issue.as_ref().map(IssueIdentity::stable_key)
                        != Some(record.issue.stable_key())
                {
                    return Err(Error::Invalid("acquire intake scope mismatch".into()));
                }
                if let Some(change) = &operation.selected_change {
                    validate_change(change, native)?;
                    if record.change.as_ref() != Some(change) {
                        return Err(Error::Invalid("acquire Change selection mismatch".into()));
                    }
                }
            } else {
                if operation.intake_key.is_some()
                    || operation.expected_intake_generation.is_some()
                    || operation.selected_change.is_some()
                    || operation.expected_generation.is_none()
                {
                    return Err(Error::Invalid("invalid claim operation selectors".into()));
                }
                if (operation.kind == "release") != record.worker_id.is_none()
                    || (operation.kind == "progress" && record.progress_digest.is_none())
                {
                    return Err(Error::Invalid("invalid claim action outcome".into()));
                }
            }
        }
        _ => return Err(Error::Invalid("unknown issue operation kind".into())),
    }
    Ok(())
}

// Replay checks the same fixed SQL operation row scope as insertion.
#[allow(clippy::too_many_arguments)]
fn replay<T: DeserializeOwned>(
    tx: &Transaction<'_>,
    id: &str,
    kind: &str,
    target: &str,
    expected: Option<i64>,
    digest: &str,
    native: &str,
    installation: &str,
) -> Result<Option<T>> {
    let Some(saved) = read_operation(tx, id, native, installation)? else {
        return Ok(None);
    };
    if saved.kind != kind
        || saved.intake_key.as_deref().or(saved.issue_key.as_deref()) != Some(target)
        || saved.expected_generation != expected
        || saved.request_digest != digest
    {
        return Err(Error::Conflict(
            "operation ID reused with different input".into(),
        ));
    }
    Ok(Some(serde_json::from_value(saved.outcome.ok_or_else(
        || Error::Invalid("missing terminal operation outcome".into()),
    )?)?))
}

fn validate_change(change: &crate::context::ChangeId, native: &str) -> Result<()> {
    change.validate()?;
    if change.repository != native {
        return Err(Error::Invalid("native Change repository mismatch".into()));
    }
    Ok(())
}

fn equivalent_outcome(left: &IntakeOutcome, right: &IntakeOutcome) -> Result<bool> {
    fn canonicalize<T: Serialize>(
        coverage: &super::Coverage<T>,
    ) -> Result<(Option<super::IncompleteReason>, Vec<Vec<u8>>)> {
        let reason = match coverage {
            super::Coverage::Complete { .. } => None,
            super::Coverage::Incomplete { reason, .. } => Some(*reason),
        };
        let mut rows = coverage
            .candidates()
            .iter()
            .map(serde_json::to_vec)
            .collect::<serde_json::Result<Vec<_>>>()?;
        rows.sort();
        rows.dedup();
        Ok((reason, rows))
    }
    Ok(left.decision == right.decision
        && left.trace == right.trace
        && left.coverage.version == right.coverage.version
        && left.coverage.repository == right.coverage.repository
        && canonicalize(&left.coverage.open_issues)? == canonicalize(&right.coverage.open_issues)?
        && canonicalize(&left.coverage.closed_issues)?
            == canonicalize(&right.coverage.closed_issues)?
        && canonicalize(&left.coverage.proposals)? == canonicalize(&right.coverage.proposals)?
        && canonicalize(&left.coverage.native_work)? == canonicalize(&right.coverage.native_work)?)
}

fn same_request_scope(left: &IntakeRequest, right: &IntakeRequest) -> bool {
    left.version == right.version
        && left.native_repository == right.native_repository
        && left.accepted_request_sha256 == right.accepted_request_sha256
        && left.repository.same_repository(&right.repository)
}

fn mark_conflicted(tx: &Transaction<'_>, record: &mut IntakeRecord) -> Result<()> {
    if !record.assessment_conflicted {
        let prior = record.generation;
        record.assessment_conflicted = true;
        record.generation = advance(prior)?;
        update_intake(tx, record, prior)?;
    }
    Ok(())
}

// Current evidence alone must prove an open issue; selection is never an override.
fn evidence_issue(intake: &IntakeRecord) -> Result<IssueIdentity> {
    if !intake.outcome.coverage.complete() {
        return Err(Error::Conflict("issue search incomplete".into()));
    }
    let issue = match &intake.outcome.decision {
        IntakeDecision::ReuseIssue { identity } => identity.clone(),
        IntakeDecision::ExistingWork { change } => intake
            .outcome
            .coverage
            .native_work
            .candidates()
            .iter()
            .find(|c| &c.change == change)
            .map(|c| c.issue.clone())
            .ok_or_else(|| Error::Conflict("native work issue missing".into()))?,
        _ => return Err(Error::Conflict("intake has no claimable open issue".into())),
    };
    if !intake
        .outcome
        .coverage
        .open_issues
        .candidates()
        .iter()
        .any(|c| c.state == IssueState::Open && c.identity.stable_key() == issue.stable_key())
    {
        return Err(Error::Conflict("canonical issue not observed open".into()));
    }
    Ok(issue)
}

fn claimable_issue(intake: &IntakeRecord) -> Result<IssueIdentity> {
    if intake.assessment_conflicted {
        return Err(Error::Conflict("intake requires reassessment".into()));
    }
    let issue = if intake.create_state == CreateState::Observed
        && matches!(intake.outcome.decision, IntakeDecision::CreateEligible {})
    {
        let observed = intake
            .reconciled_observation
            .as_ref()
            .ok_or_else(|| Error::Invalid("missing reconciliation".into()))?;
        if observed.related_proposal.is_some() {
            return Err(Error::Conflict(
                "reconciled issue already has a proposal".into(),
            ));
        }
        observed.identity.clone()
    } else {
        evidence_issue(intake)?
    };
    if intake
        .selected_issue
        .as_ref()
        .or(intake.claimed_issue.as_ref())
        .is_some_and(|fixed| fixed.stable_key() != issue.stable_key())
    {
        return Err(Error::Conflict("canonical issue selection mismatch".into()));
    }
    Ok(issue)
}

fn validate_intake_receipt(record: &IntakeRecord, native: &str, installation: &str) -> Result<()> {
    record.request.validate()?;
    if record.request.native_repository != native
        || record.key != intake_key(installation, &record.request)
        || record.generation <= 0
        || record.generation == i64::MAX
    {
        return Err(Error::Invalid(
            "historical intake receipt scope mismatch".into(),
        ));
    }
    if evaluate(
        &record.request,
        record.outcome.coverage.clone(),
        record.choice.as_ref(),
    )? != record.outcome
    {
        return Err(Error::Invalid(
            "historical intake receipt decision mismatch".into(),
        ));
    }
    for issue in record
        .selected_issue
        .iter()
        .chain(record.claimed_issue.iter())
    {
        issue.validate()?;
        if !issue.repository.same_repository(&record.request.repository) {
            return Err(Error::Invalid(
                "historical intake issue scope mismatch".into(),
            ));
        }
    }
    if let Some(id) = &record.create_operation {
        validate_id(id)?;
    }
    match record.create_state {
        CreateState::Unreserved
            if record.selected_issue.is_none()
                && record.create_operation.is_none()
                && record.reconciled_observation.is_none() => {}
        CreateState::ReservedUncertain
            if record.selected_issue.is_none()
                && record.claimed_issue.is_none()
                && record.create_operation.is_some()
                && record.reconciled_observation.is_none()
                && matches!(record.outcome.decision, IntakeDecision::CreateEligible {}) => {}
        CreateState::Observed
            if record.selected_issue.is_some() && record.create_operation.is_some() =>
        {
            let observed = record
                .reconciled_observation
                .as_ref()
                .ok_or_else(|| Error::Invalid("missing reconciliation observation".into()))?;
            observed.identity.validate()?;
            if let Some(proposal) = &observed.related_proposal {
                proposal.validate()?;
                if !proposal
                    .repository
                    .same_repository(&record.request.repository)
                {
                    return Err(Error::Invalid(
                        "reconciliation proposal scope mismatch".into(),
                    ));
                }
            }
            if observed.state != IssueState::Open
                || observed.basis != MatchBasis::ExactRequest
                || observed.request_digest != record.request.accepted_request_sha256
                || record
                    .selected_issue
                    .as_ref()
                    .map(IssueIdentity::stable_key)
                    != Some(observed.identity.stable_key())
            {
                return Err(Error::Invalid("historical reconciliation mismatch".into()));
            }
            if !matches!(record.outcome.decision, IntakeDecision::CreateEligible {})
                && evidence_issue(record)?.stable_key() != observed.identity.stable_key()
            {
                return Err(Error::Invalid(
                    "historical observed evidence mismatch".into(),
                ));
            }
        }
        _ => return Err(Error::Invalid("historical create state mismatch".into())),
    }
    if let Some(bound) = &record.claimed_issue {
        // Historical identity is fixed even when readiness has been revoked.
        let identity = if record.create_state == CreateState::Observed {
            record
                .selected_issue
                .clone()
                .ok_or_else(|| Error::Invalid("missing selected issue".into()))?
        } else {
            evidence_issue(record)?
        };
        if identity.stable_key() != bound.stable_key() {
            return Err(Error::Invalid("historical claimed issue mismatch".into()));
        }
    }
    Ok(())
}
fn validate_claim_receipt(record: &ClaimRecord, native: &str) -> Result<()> {
    record.issue.validate()?;
    if record.generation <= 0 || record.generation == i64::MAX {
        return Err(Error::Invalid(
            "historical claim generation mismatch".into(),
        ));
    }
    if let Some(worker) = &record.worker_id {
        validate_id(worker)?;
    }
    if let Some(digest) = &record.progress_digest {
        super::validate_digest(digest)?;
    }
    if let Some(change) = &record.change {
        change.validate()?;
        if change.repository != native {
            return Err(Error::Invalid(
                "historical native Change scope mismatch".into(),
            ));
        }
    }
    Ok(())
}
