use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::{
    BaseChoice, BasePage, ConflictPage, MergeConflict, MergeOutcome, MergePhase,
    MergeRejectionReason, MergeState, Resolution, ancestry, plan,
};
use crate::context::ChangeId;
use crate::store::{
    BlobChange, Engine, SourceReader, create_directory_durable, digest, validate_id,
};
use crate::{Error, Result, sync};

const APPLICATION_ID: i64 = 0x4147_4d47;
const SCHEMA_VERSION: i64 = 1;
const MAX_PLAN_BYTES: usize = 16 * 1024 * 1024;
const MAX_BASE_BYTES: usize = 8 * 1024 * 1024;
const MAX_OUTCOME_BYTES: usize = 4 * 1024;
const MAX_RESOLUTION_INTENT_BYTES: usize = 4 * 1024;
const MAX_PENDING_MANUAL_BYTES: i64 = 128 * 1024 * 1024;
const MAX_OPERATION_ROWS: i64 = 10_000;
const MAX_SOURCE_READ_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PAGE: usize = 1_000;

#[cfg(test)]
std::thread_local! {
    static AFTER_PENDING_METADATA: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_after_pending_metadata() {
    let callback = AFTER_PENDING_METADATA.with(|slot| slot.borrow_mut().take());
    if let Some(callback) = callback {
        callback();
    }
}

/// A local journal. Its recorded roots are not collector-visible retention leases.
pub struct MergeJournal {
    _access: crate::local_recovery::AccessGuard,
    database: PathBuf,
    repository: String,
}

/// An actual terminal journal record, bound to source and session generations.
/// This is not an authenticated network permission or a retention lease.
#[derive(Clone, Debug)]
pub struct MergeCompletion {
    pub(crate) repository: String,
    pub(crate) session: String,
    pub(crate) generation: u64,
    pub(crate) left: String,
    pub(crate) revision: String,
}

impl MergeCompletion {
    pub fn session(&self) -> &str {
        &self.session
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn revision(&self) -> &str {
        &self.revision
    }
}

struct Row {
    state: MergeState,
    base_selected: bool,
    bases: Vec<String>,
    plan: Option<plan::Plan>,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StoredResolutionChoice {
    Left,
    Right,
    Base,
    Delete,
    Bytes,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ResolutionIntent {
    conflict_id: String,
    choice: StoredResolutionChoice,
}

struct PendingResolution {
    request: String,
    expected: u64,
    intent: ResolutionIntent,
    manual_bytes: Option<Vec<u8>>,
}

enum ResolutionComputation {
    Applied(plan::Plan, MergePhase),
    Rejected(MergeRejectionReason),
}

impl PendingResolution {
    fn into_resolution(self) -> Result<Resolution> {
        match (self.intent.choice, self.manual_bytes) {
            (StoredResolutionChoice::Left, None) => Ok(Resolution::Left),
            (StoredResolutionChoice::Right, None) => Ok(Resolution::Right),
            (StoredResolutionChoice::Base, None) => Ok(Resolution::Base),
            (StoredResolutionChoice::Delete, None) => Ok(Resolution::Delete),
            (StoredResolutionChoice::Bytes, Some(bytes)) => Ok(Resolution::Bytes(bytes)),
            _ => Err(Error::Invalid(
                "pending merge resolution payload does not match its intent".into(),
            )),
        }
    }
}

impl MergeJournal {
    pub fn open(data_dir: impl AsRef<Path>, repository: &str) -> Result<Self> {
        ChangeId {
            repository: repository.to_owned(),
            id: "merge-journal".into(),
        }
        .validate()?;
        crate::local_recovery::check_before_create(data_dir.as_ref(), false)?;
        let root = create_directory_durable(data_dir.as_ref())?;
        let access = crate::local_recovery::AccessGuard::shared(&root)?;
        let directory = create_directory_durable(root.join("merges"))?;
        let journal = Self {
            _access: access,
            database: directory.join("state.sqlite"),
            repository: repository.to_owned(),
        };
        let mut connection = journal.connect(true)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let application: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if application == 0 && version == 0 {
            let tables: i64 = tx.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |row| row.get(0))?;
            if tables != 0 {
                return Err(Error::Invalid(
                    "merge journal contains an unknown schema".into(),
                ));
            }
            tx.execute_batch("CREATE TABLE merge_repository(singleton INTEGER PRIMARY KEY CHECK(singleton=1), repository TEXT NOT NULL) STRICT;
                CREATE TABLE merge_sessions(session TEXT PRIMARY KEY CHECK(length(session)=64), left_revision TEXT NOT NULL CHECK(length(left_revision)=64), right_revision TEXT NOT NULL CHECK(length(right_revision)=64), generation INTEGER NOT NULL CHECK(generation>=0), phase TEXT NOT NULL CHECK(phase IN ('planning','awaiting_base','conflicts','ready','publishing','completed','aborted')), base_selected INTEGER NOT NULL CHECK(base_selected IN (0,1)), base_revision TEXT, bases_json BLOB NOT NULL CHECK(length(bases_json)<=8388608), plan_json BLOB CHECK(plan_json IS NULL OR length(plan_json)<=16777216), result_revision TEXT) STRICT;
                CREATE TABLE merge_operations(operation TEXT PRIMARY KEY, session TEXT NOT NULL REFERENCES merge_sessions(session), kind TEXT NOT NULL CHECK(kind IN ('begin','base','resolve','complete','abort')), request_digest TEXT NOT NULL CHECK(length(request_digest)=64), expected_generation INTEGER NOT NULL CHECK(expected_generation>=0), intent_json BLOB CHECK(intent_json IS NULL OR length(intent_json)<=4096), manual_bytes BLOB CHECK(manual_bytes IS NULL OR length(manual_bytes)<=67108864), outcome_json BLOB CHECK(outcome_json IS NULL OR length(outcome_json)<=4096), CHECK((outcome_json IS NULL AND kind='resolve' AND intent_json IS NOT NULL) OR (outcome_json IS NULL AND kind!='resolve' AND intent_json IS NULL AND manual_bytes IS NULL) OR (outcome_json IS NOT NULL AND intent_json IS NULL AND manual_bytes IS NULL)), CHECK(manual_bytes IS NULL OR (kind='resolve' AND outcome_json IS NULL AND intent_json IS NOT NULL))) STRICT;
                CREATE UNIQUE INDEX merge_one_pending ON merge_operations(session) WHERE outcome_json IS NULL;")?;
            tx.execute(
                "INSERT INTO merge_repository(singleton,repository) VALUES(1,?1)",
                params![repository],
            )?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if application != APPLICATION_ID || version != SCHEMA_VERSION {
            return Err(Error::Invalid(
                "unsupported merge journal identity or version".into(),
            ));
        }
        let bound: String = tx.query_row(
            "SELECT repository FROM merge_repository WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if bound != repository {
            return Err(Error::Conflict(
                "merge journal belongs to another repository".into(),
            ));
        }
        tx.commit()?;
        Ok(journal)
    }

    pub fn inspect(&self, session: &str) -> Result<MergeState> {
        validate_id(session)?;
        Ok(read_row(&self.connect(false)?, session)?.state)
    }

    /// Find the durable session even when begin stopped before returning an outcome.
    pub fn operation_session(&self, operation: &str) -> Result<String> {
        self.validate_operation(operation)?;
        self.connect(false)?
            .query_row(
                "SELECT session FROM merge_operations WHERE operation=?1",
                params![operation],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound("merge operation".into()))
    }

    pub fn bases(&self, session: &str, cursor: Option<&str>, limit: usize) -> Result<BasePage> {
        check_page(limit)?;
        let row = read_row(&self.connect(false)?, session)?;
        let after = decode_cursor(cursor, session, row.state.generation, "bases")?;
        let mut entries: Vec<_> = row
            .bases
            .into_iter()
            .filter(|id| after.as_ref().is_none_or(|after| id > after))
            .take(limit + 1)
            .collect();
        let more = entries.len() > limit;
        if more {
            entries.pop();
        }
        let next = if more {
            Some(encode_cursor(
                session,
                row.state.generation,
                "bases",
                entries.last().expect("nonempty page"),
            )?)
        } else {
            None
        };
        Ok(BasePage { entries, next })
    }

    pub fn conflicts(
        &self,
        session: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ConflictPage> {
        check_page(limit)?;
        let row = read_row(&self.connect(false)?, session)?;
        let after = decode_cursor(cursor, session, row.state.generation, "conflicts")?;
        let mut conflicts = row.plan.map(|plan| plan.conflicts).unwrap_or_default();
        // Cursor comparison and emitted order must agree, regardless of plan path order.
        conflicts.sort_by(|a, b| a.id.cmp(&b.id));
        let mut entries: Vec<_> = conflicts
            .into_iter()
            .filter(|conflict| after.as_ref().is_none_or(|after| conflict.id > *after))
            .take(limit + 1)
            .collect();
        let more = entries.len() > limit;
        if more {
            entries.pop();
        }
        for conflict in &mut entries {
            conflict.preview = None;
        }
        let next = if more {
            Some(encode_cursor(
                session,
                row.state.generation,
                "conflicts",
                &entries.last().expect("nonempty page").id,
            )?)
        } else {
            None
        };
        Ok(ConflictPage { entries, next })
    }

    pub fn conflict(&self, session: &str, id: &str) -> Result<MergeConflict> {
        validate_id(id)?;
        read_row(&self.connect(false)?, session)?
            .plan
            .and_then(|plan| {
                plan.conflicts
                    .into_iter()
                    .find(|conflict| conflict.id == id)
            })
            .ok_or_else(|| Error::NotFound("merge conflict".into()))
    }

    pub fn begin(
        &mut self,
        engine: &Engine,
        operation: &str,
        left: &str,
        right: &str,
    ) -> Result<MergeOutcome> {
        self.validate_operation(operation)?;
        validate_id(left)?;
        validate_id(right)?;
        let request = request_digest(&self.repository, "begin", &(operation, left, right))?;
        let session = digest(
            format!(
                "agentscm-merge-session-v1\0{}\0{}",
                self.repository, request
            )
            .as_bytes(),
        );
        let mut connection = self.connect(false)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = retry(&tx, operation, &request)? {
            if let Some(outcome) = outcome {
                return Ok(outcome);
            }
            drop(tx);
            return self.finish(engine, &session, operation);
        }
        ensure_capacity(&tx, 1, 2)?;
        tx.execute(
            "INSERT INTO merge_sessions(session,left_revision,right_revision,generation,phase,base_selected,base_revision,bases_json,plan_json,result_revision) VALUES(?1,?2,?3,0,'planning',0,NULL,?4,NULL,NULL)",
            params![session, left, right, b"[]".as_slice()],
        )?;
        tx.execute(
            "INSERT INTO merge_operations(operation,session,kind,request_digest,expected_generation,intent_json,manual_bytes,outcome_json) VALUES(?1,?2,'begin',?3,0,NULL,NULL,NULL)",
            params![operation, session, request],
        )?;
        tx.commit()?;
        self.finish(engine, &session, operation)
    }

    pub fn select_base(
        &mut self,
        engine: &Engine,
        session: &str,
        operation: &str,
        expected_generation: u64,
        choice: BaseChoice,
    ) -> Result<MergeOutcome> {
        self.validate_operation(operation)?;
        validate_id(session)?;
        let request = request_digest(
            &self.repository,
            "base",
            &(session, operation, expected_generation, &choice),
        )?;
        let mut connection = self.connect(false)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = retry(&tx, operation, &request)? {
            if let Some(outcome) = outcome {
                return Ok(outcome);
            }
            drop(tx);
            return self.finish(engine, session, operation);
        }
        let row = read_row(&tx, session)?;
        require_generation(&row.state, expected_generation)?;
        if row.state.phase != MergePhase::AwaitingBase {
            return Err(Error::Conflict("merge does not await a base choice".into()));
        }
        // Retain a generation for terminal completion or abort after this choice.
        next_generation(next_generation(expected_generation)?)?;
        let base = match choice {
            BaseChoice::Revision(id) => {
                validate_id(&id)?;
                if !row.bases.contains(&id) {
                    return Err(Error::Invalid(
                        "chosen base is not a recorded best common ancestor".into(),
                    ));
                }
                Some(id)
            }
            BaseChoice::Empty => {
                if !row.bases.is_empty() {
                    return Err(Error::Invalid(
                        "empty baseline is only available for unrelated histories".into(),
                    ));
                }
                None
            }
        };
        ensure_no_pending(&tx, session)?;
        ensure_capacity(&tx, 1, 0)?;
        tx.execute(
            "INSERT INTO merge_operations(operation,session,kind,request_digest,expected_generation,intent_json,manual_bytes,outcome_json) VALUES(?1,?2,'base',?3,?4,NULL,NULL,NULL)",
            params![
                operation,
                session,
                request,
                sql_generation(expected_generation)?
            ],
        )?;
        tx.execute("UPDATE merge_sessions SET phase='planning',base_selected=1,base_revision=?2 WHERE session=?1",params![session,base])?;
        tx.commit()?;
        self.finish(engine, session, operation)
    }

    pub fn resolve(
        &mut self,
        engine: &Engine,
        session: &str,
        operation: &str,
        expected_generation: u64,
        conflict_id: &str,
        resolution: Resolution,
    ) -> Result<MergeOutcome> {
        if let Some(outcome) = self.prepare_resolution(
            session,
            operation,
            expected_generation,
            conflict_id,
            &resolution,
        )? {
            return Ok(outcome);
        }
        self.finish(engine, session, operation)
    }

    fn prepare_resolution(
        &mut self,
        session: &str,
        operation: &str,
        expected_generation: u64,
        conflict_id: &str,
        resolution: &Resolution,
    ) -> Result<Option<MergeOutcome>> {
        self.validate_operation(operation)?;
        validate_id(session)?;
        validate_id(conflict_id)?;
        if let Resolution::Bytes(bytes) = resolution
            && bytes.len() > crate::store::MAX_FILE_BYTES
        {
            return Err(Error::Limit(
                "merge resolution exceeds file byte limit".into(),
            ));
        }
        let request = request_digest(
            &self.repository,
            "resolve",
            &(
                session,
                operation,
                expected_generation,
                conflict_id,
                resolution,
            ),
        )?;
        let mut connection = self.connect(false)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = retry(&tx, operation, &request)? {
            return Ok(outcome);
        }
        let row = read_row(&tx, session)?;
        require_generation(&row.state, expected_generation)?;
        if !matches!(row.state.phase, MergePhase::Conflicts | MergePhase::Ready) {
            return Err(Error::Conflict(
                "merge does not await conflict resolution".into(),
            ));
        }
        ensure_no_pending(&tx, session)?;
        next_generation(next_generation(expected_generation)?)?;
        ensure_capacity(&tx, 1, 0)?;
        let plan = row
            .plan
            .as_ref()
            .ok_or_else(|| Error::Invalid("merge conflict plan is missing".into()))?;
        let conflict = plan
            .conflicts
            .iter()
            .find(|conflict| conflict.id == conflict_id)
            .ok_or_else(|| Error::NotFound("merge conflict".into()))?;
        if matches!(resolution, Resolution::Bytes(_)) && conflict.paths.len() != 1 {
            return Err(Error::Invalid(
                "manual bytes require a single-path conflict".into(),
            ));
        }
        let choice = match resolution {
            Resolution::Left => StoredResolutionChoice::Left,
            Resolution::Right => StoredResolutionChoice::Right,
            Resolution::Base => StoredResolutionChoice::Base,
            Resolution::Delete => StoredResolutionChoice::Delete,
            Resolution::Bytes(_) => StoredResolutionChoice::Bytes,
        };
        let intent = encode_bounded(
            &ResolutionIntent {
                conflict_id: conflict_id.to_owned(),
                choice,
            },
            MAX_RESOLUTION_INTENT_BYTES,
            "merge resolution intent",
        )?;
        let manual = match resolution {
            Resolution::Bytes(bytes) => Some(bytes.as_slice()),
            _ => None,
        };
        if let Some(bytes) = manual {
            let retained: i64 = tx.query_row(
                "SELECT coalesce(sum(length(manual_bytes)),0) FROM merge_operations WHERE outcome_json IS NULL",
                [],
                |row| row.get(0),
            )?;
            let incoming = i64::try_from(bytes.len())
                .map_err(|_| Error::Limit("merge resolution exceeds byte limit".into()))?;
            if retained
                .checked_add(incoming)
                .is_none_or(|total| total > MAX_PENDING_MANUAL_BYTES)
            {
                return Err(Error::Limit(
                    "pending merge resolution bytes exceed admission limit".into(),
                ));
            }
        }
        tx.execute(
            "INSERT INTO merge_operations(operation,session,kind,request_digest,expected_generation,intent_json,manual_bytes,outcome_json) VALUES(?1,?2,'resolve',?3,?4,?5,?6,NULL)",
            params![
                operation,
                session,
                request,
                sql_generation(expected_generation)?,
                intent,
                manual,
            ],
        )?;
        tx.commit()?;
        Ok(None)
    }

    pub fn complete(
        &mut self,
        engine: &Engine,
        session: &str,
        operation: &str,
        expected_generation: u64,
    ) -> Result<MergeOutcome> {
        self.validate_operation(operation)?;
        validate_id(session)?;
        let request = request_digest(
            &self.repository,
            "complete",
            &(session, operation, expected_generation),
        )?;
        let mut connection = self.connect(false)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = retry(&tx, operation, &request)? {
            if let Some(outcome) = outcome {
                return Ok(outcome);
            }
            drop(tx);
            return self.finish(engine, session, operation);
        }
        let row = read_row(&tx, session)?;
        require_generation(&row.state, expected_generation)?;
        if row.state.phase != MergePhase::Ready {
            return Err(Error::Conflict("merge is not fully resolved".into()));
        }
        ensure_no_pending(&tx, session)?;
        next_generation(expected_generation)?;
        // Publishing consumes one reserved slot; its remaining slot permits abort.
        ensure_capacity(&tx, 1, -1)?;
        tx.execute(
            "INSERT INTO merge_operations(operation,session,kind,request_digest,expected_generation,intent_json,manual_bytes,outcome_json) VALUES(?1,?2,'complete',?3,?4,NULL,NULL,NULL)",
            params![
                operation,
                session,
                request,
                sql_generation(expected_generation)?
            ],
        )?;
        tx.execute(
            "UPDATE merge_sessions SET phase='publishing' WHERE session=?1",
            params![session],
        )?;
        tx.commit()?;
        self.finish(engine, session, operation)
    }

    pub fn abort(
        &mut self,
        session: &str,
        operation: &str,
        expected_generation: u64,
    ) -> Result<MergeOutcome> {
        self.validate_operation(operation)?;
        validate_id(session)?;
        let request = request_digest(
            &self.repository,
            "abort",
            &(session, operation, expected_generation),
        )?;
        let mut connection = self.connect(false)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = retry(&tx, operation, &request)? {
            return outcome.ok_or_else(|| Error::Conflict("abort unexpectedly pending".into()));
        }
        let row = read_row(&tx, session)?;
        require_generation(&row.state, expected_generation)?;
        if matches!(row.state.phase, MergePhase::Completed | MergePhase::Aborted) {
            return Err(Error::Conflict("merge session is terminal".into()));
        }
        let released_reservations = if row.state.phase == MergePhase::Publishing {
            -1
        } else {
            -2
        };
        ensure_capacity(&tx, 1, released_reservations)?;
        let generation = next_generation(expected_generation)?;
        let outcome = MergeOutcome::Aborted {
            session: session.to_owned(),
            generation,
        };
        let encoded = encode_bounded(&outcome, MAX_OUTCOME_BYTES, "merge outcome")?;
        tx.execute(
            "UPDATE merge_operations SET intent_json=NULL,manual_bytes=NULL,outcome_json=?2 WHERE session=?1 AND outcome_json IS NULL",
            params![session, encoded],
        )?;
        tx.execute(
            "UPDATE merge_sessions SET phase='aborted',generation=?2 WHERE session=?1",
            params![session, sql_generation(generation)?],
        )?;
        insert_completed(
            &tx,
            operation,
            session,
            "abort",
            &request,
            expected_generation,
            &outcome,
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    pub fn recover(&mut self, engine: &Engine, session: &str) -> Result<MergeOutcome> {
        validate_id(session)?;
        let connection = self.connect(false)?;
        let operation: Option<String> = connection
            .query_row(
                "SELECT operation FROM merge_operations WHERE session=?1 AND outcome_json IS NULL",
                params![session],
                |row| row.get(0),
            )
            .optional()?;
        match operation {
            Some(operation) => self.finish(engine, session, &operation),
            None => Err(Error::NotFound("merge has no pending operation".into())),
        }
    }

    pub fn completion(&self, session: &str, expected_generation: u64) -> Result<MergeCompletion> {
        let row = read_row(&self.connect(false)?, session)?;
        require_generation(&row.state, expected_generation)?;
        if row.state.phase != MergePhase::Completed {
            return Err(Error::Conflict("merge has no completed source".into()));
        }
        Ok(MergeCompletion {
            repository: self.repository.clone(),
            session: session.to_owned(),
            generation: expected_generation,
            left: row.state.left,
            revision: row
                .state
                .revision
                .ok_or_else(|| Error::Invalid("completed merge lacks revision".into()))?,
        })
    }

    fn finish(&mut self, engine: &Engine, session: &str, operation: &str) -> Result<MergeOutcome> {
        self.finish_with_read_limit(engine, session, operation, MAX_SOURCE_READ_BYTES)
    }

    fn finish_with_read_limit(
        &mut self,
        engine: &Engine,
        session: &str,
        operation: &str,
        maximum_read_bytes: u64,
    ) -> Result<MergeOutcome> {
        let connection = self.connect(false)?;
        let kind: String = connection.query_row(
            "SELECT kind FROM merge_operations WHERE operation=?1 AND session=?2",
            params![operation, session],
            |row| row.get(0),
        )?;
        if kind == "resolve" {
            drop(connection);
            return self.finish_resolution_with_read_limit(
                engine,
                session,
                operation,
                maximum_read_bytes,
            );
        }
        let row = read_row(&connection, session)?;
        let (expected,stored): (i64,Option<Vec<u8>>)=connection.query_row("SELECT expected_generation,outcome_json FROM merge_operations WHERE operation=?1 AND session=?2",params![operation,session],|row|Ok((row.get(0)?,row.get(1)?)))?;
        if let Some(stored) = stored {
            return decode_bounded(&stored, MAX_OUTCOME_BYTES, "merge outcome");
        }
        let expected = u64::try_from(expected)
            .map_err(|_| Error::Invalid("negative merge generation".into()))?;
        require_generation(&row.state, expected)?;
        let (engine, reads) = engine.with_read_budget(maximum_read_bytes);
        let source = SourceReader::new(&engine);
        let computed = (|| {
            sync::qualify_reader(&source, std::slice::from_ref(&row.state.left))?;
            if row.state.right != row.state.left {
                sync::qualify_reader(&source, std::slice::from_ref(&row.state.right))?;
            }
            if kind == "complete" {
                if row.state.phase != MergePhase::Publishing {
                    return Err(Error::Conflict(
                        "merge publication is no longer pending".into(),
                    ));
                }
                let plan = row
                    .plan
                    .as_ref()
                    .ok_or_else(|| Error::Invalid("merge plan is missing".into()))?;
                if plan.conflicts.iter().any(|conflict| !conflict.resolved) {
                    return Err(Error::Conflict(
                        "merge still has unresolved conflicts".into(),
                    ));
                }
                let revision = if row.state.left == row.state.right {
                    row.state.left.clone()
                } else {
                    let changes: Vec<_> = plan
                        .files
                        .iter()
                        .filter(|file| file.result != file.left)
                        .map(|file| BlobChange {
                            path: file.path.clone(),
                            blob: file.result.clone(),
                        })
                        .collect();
                    engine.checkpoint_with_blob_changes(
                        &[row.state.left.clone(), row.state.right.clone()],
                        &changes,
                    )?
                };
                // The proof comes from actual complete candidate bytes, never journal flags.
                sync::qualify_reader(&source, std::slice::from_ref(&revision))?;
                Ok((
                    row.bases.clone(),
                    row.state.base.clone(),
                    true,
                    None,
                    Some(revision),
                    MergePhase::Completed,
                ))
            } else if kind == "begin" || kind == "base" {
                if row.state.phase != MergePhase::Planning {
                    return Err(Error::Conflict(
                        "merge planning is no longer pending".into(),
                    ));
                }
                let bases = if kind == "begin" {
                    ancestry::inspect(&source, &row.state.left, &row.state.right)?.bases
                } else {
                    row.bases.clone()
                };
                let selected = row.base_selected || bases.len() == 1;
                let base = if row.base_selected {
                    row.state.base.clone()
                } else {
                    bases.first().filter(|_| bases.len() == 1).cloned()
                };
                if !selected {
                    Ok((bases, base, false, None, None, MergePhase::AwaitingBase))
                } else {
                    let plan = plan::build(
                        &source,
                        &engine,
                        base.as_deref(),
                        &row.state.left,
                        &row.state.right,
                    )?;
                    let phase = if plan.conflicts.is_empty() {
                        MergePhase::Ready
                    } else {
                        MergePhase::Conflicts
                    };
                    Ok((bases, base, true, Some(plan), None, phase))
                }
            } else {
                Err(Error::Invalid(
                    "unexpected pending merge operation kind".into(),
                ))
            }
        })();
        let (bases, base, selected, plan, revision, phase) = reads.finish(computed)?;
        let bases_bytes = encode_bounded(&bases, MAX_BASE_BYTES, "merge bases")?;
        let plan_bytes = plan
            .as_ref()
            .map(|plan| encode_bounded(plan, MAX_PLAN_BYTES, "merge plan"))
            .transpose()?;
        let mut connection = self.connect(false)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_row(&tx, session)?;
        let completed: Option<Vec<u8>> = tx.query_row(
            "SELECT outcome_json FROM merge_operations WHERE operation=?1",
            params![operation],
            |row| row.get(0),
        )?;
        if let Some(completed) = completed {
            return decode_bounded(&completed, MAX_OUTCOME_BYTES, "merge outcome");
        }
        require_generation(&current.state, expected)?;
        if current.state.phase != row.state.phase {
            return Err(Error::Conflict(
                "merge operation changed while computing source".into(),
            ));
        }
        let generation = next_generation(expected)?;
        tx.execute("UPDATE merge_sessions SET generation=?2,phase=?3,base_selected=?4,base_revision=?5,bases_json=?6,plan_json=coalesce(?7,plan_json),result_revision=?8 WHERE session=?1",params![session,sql_generation(generation)?,phase_name(phase),selected,base,bases_bytes,plan_bytes,revision])?;
        let outcome = if phase == MergePhase::Completed {
            MergeOutcome::Completed {
                session: session.to_owned(),
                generation,
                revision: revision.expect("computed completed revision"),
            }
        } else if kind == "begin" {
            MergeOutcome::Prepared {
                session: session.to_owned(),
                generation,
                phase,
            }
        } else {
            MergeOutcome::Resolved {
                session: session.to_owned(),
                generation,
                phase,
            }
        };
        let outcome_bytes = encode_bounded(&outcome, MAX_OUTCOME_BYTES, "merge outcome")?;
        tx.execute(
            "UPDATE merge_operations SET intent_json=NULL,manual_bytes=NULL,outcome_json=?2 WHERE operation=?1",
            params![operation, outcome_bytes],
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    fn finish_resolution_with_read_limit(
        &mut self,
        engine: &Engine,
        session: &str,
        operation: &str,
        maximum_read_bytes: u64,
    ) -> Result<MergeOutcome> {
        let mut connection = self.connect(false)?;
        let snapshot = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        if let Some(outcome) = read_completed_outcome(&snapshot, operation, session)? {
            return Ok(outcome);
        }
        let pending = read_pending_resolution(&snapshot, &self.repository, operation, session)?;
        let row = read_row(&snapshot, session)?;
        require_generation(&row.state, pending.expected)?;
        if !matches!(row.state.phase, MergePhase::Conflicts | MergePhase::Ready) {
            return Err(Error::Conflict(
                "merge does not await conflict resolution".into(),
            ));
        }
        let frozen_phase = row.state.phase;
        let expected = pending.expected;
        let request = pending.request.clone();
        let intent = pending.intent.clone();
        let resolution = pending.into_resolution()?;
        let left = row.state.left.clone();
        snapshot.commit()?;
        drop(connection);

        let (engine, reads) = engine.with_read_budget(maximum_read_bytes);
        let source = SourceReader::new(&engine);
        let computed = (|| {
            let mut plan = row
                .plan
                .ok_or_else(|| Error::Invalid("merge conflict plan is missing".into()))?;
            let conflict_index = plan
                .conflicts
                .iter()
                .position(|conflict| conflict.id == intent.conflict_id)
                .ok_or_else(|| Error::NotFound("merge conflict".into()))?;
            let paths = plan.conflicts[conflict_index].paths.clone();
            if matches!(&resolution, Resolution::Bytes(_)) && paths.len() != 1 {
                return Err(Error::Invalid(
                    "manual bytes require a single-path conflict".into(),
                ));
            }
            let file_indices: Vec<_> = paths
                .iter()
                .map(|path| {
                    let index = plan
                        .files
                        .binary_search_by(|file| file.path.cmp(&path.path))
                        .map_err(|_| Error::Invalid("conflict path is not in merge plan".into()))?;
                    if plan.files[index].conflict.as_deref() != Some(intent.conflict_id.as_str()) {
                        return Err(Error::Invalid(
                            "merge conflict/path binding mismatch".into(),
                        ));
                    }
                    Ok(index)
                })
                .collect::<Result<_>>()?;
            // Publication follows validation of the durable conflict/path binding.
            let manual = match &resolution {
                Resolution::Bytes(bytes) => Some(engine.publish_blob(bytes)?),
                _ => None,
            };
            let mut resolved_paths = Vec::with_capacity(paths.len());
            for (path_index, (path, file_index)) in paths.iter().zip(file_indices).enumerate() {
                let result = match &resolution {
                    Resolution::Left => path.left.clone(),
                    Resolution::Right => path.right.clone(),
                    Resolution::Base => path.base.clone(),
                    Resolution::Delete => None,
                    Resolution::Bytes(_) => manual.clone(),
                };
                resolved_paths.push(super::ResolvedPath {
                    path_index,
                    blob: result.clone(),
                });
                plan.files[file_index].result = result;
            }
            let conflict = &mut plan.conflicts[conflict_index];
            conflict.resolution = Some(resolved_paths);
            conflict.resolved = true;
            match plan::validate_results(&source, &left, &plan.files) {
                Ok(()) => {}
                Err(Error::Conflict(_)) => {
                    return Ok(ResolutionComputation::Rejected(
                        MergeRejectionReason::StructuralConflict,
                    ));
                }
                Err(error) => return Err(error),
            }
            let phase = if plan.conflicts.iter().any(|conflict| !conflict.resolved) {
                MergePhase::Conflicts
            } else {
                MergePhase::Ready
            };
            Ok(ResolutionComputation::Applied(plan, phase))
        })();
        let computed = reads.finish(computed)?;
        let (plan_update, outcome) = match computed {
            ResolutionComputation::Applied(plan, phase) => {
                let generation = next_generation(expected)?;
                (
                    Some((
                        encode_bounded(&plan, MAX_PLAN_BYTES, "merge plan")?,
                        phase,
                        generation,
                    )),
                    MergeOutcome::Resolved {
                        session: session.to_owned(),
                        generation,
                        phase,
                    },
                )
            }
            ResolutionComputation::Rejected(reason) => (
                None,
                MergeOutcome::Rejected {
                    session: session.to_owned(),
                    generation: expected,
                    reason,
                },
            ),
        };
        let outcome_bytes = encode_bounded(&outcome, MAX_OUTCOME_BYTES, "merge outcome")?;

        let mut connection = self.connect(false)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = read_completed_outcome(&tx, operation, session)? {
            return Ok(outcome);
        }
        let current_pending = read_pending_resolution(&tx, &self.repository, operation, session)?;
        if current_pending.request != request
            || current_pending.expected != expected
            || current_pending.intent != intent
        {
            return Err(Error::Conflict(
                "pending merge resolution changed while computing source".into(),
            ));
        }
        let current = read_row(&tx, session)?;
        require_generation(&current.state, expected)?;
        if current.state.phase != frozen_phase {
            return Err(Error::Conflict(
                "merge resolution changed while computing source".into(),
            ));
        }
        if let Some((plan_bytes, phase, generation)) = plan_update {
            tx.execute(
                "UPDATE merge_sessions SET generation=?2,phase=?3,plan_json=?4 WHERE session=?1",
                params![
                    session,
                    sql_generation(generation)?,
                    phase_name(phase),
                    plan_bytes
                ],
            )?;
        }
        let changed = tx.execute(
            "UPDATE merge_operations SET intent_json=NULL,manual_bytes=NULL,outcome_json=?2 WHERE operation=?1 AND session=?3 AND outcome_json IS NULL AND request_digest=?4 AND expected_generation=?5",
            params![
                operation,
                outcome_bytes,
                session,
                request,
                sql_generation(expected)?
            ],
        )?;
        if changed != 1 {
            return Err(Error::Conflict(
                "pending merge resolution changed before acknowledgement".into(),
            ));
        }
        tx.commit()?;
        Ok(outcome)
    }

    fn validate_operation(&self, operation: &str) -> Result<()> {
        ChangeId {
            repository: self.repository.clone(),
            id: operation.to_owned(),
        }
        .validate()
    }
    fn connect(&self, create: bool) -> Result<Connection> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | if create {
                OpenFlags::SQLITE_OPEN_CREATE
            } else {
                OpenFlags::empty()
            };
        let connection = Connection::open_with_flags(&self.database, flags)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        if !create {
            let app: i64 =
                connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
            let version: i64 =
                connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
            let repository: String = connection.query_row(
                "SELECT repository FROM merge_repository WHERE singleton=1",
                [],
                |row| row.get(0),
            )?;
            if app != APPLICATION_ID || version != SCHEMA_VERSION || repository != self.repository {
                return Err(Error::Invalid("merge journal identity changed".into()));
            }
        }
        Ok(connection)
    }
}

fn read_completed_outcome(
    connection: &Connection,
    operation: &str,
    session: &str,
) -> Result<Option<MergeOutcome>> {
    let length: Option<usize> = connection.query_row(
        "SELECT length(outcome_json) FROM merge_operations WHERE operation=?1 AND session=?2",
        params![operation, session],
        |row| row.get(0),
    )?;
    let Some(length) = length else {
        return Ok(None);
    };
    if length > MAX_OUTCOME_BYTES {
        return Err(Error::Limit("merge outcome exceeds byte limit".into()));
    }
    let bytes: Vec<u8> = connection.query_row(
        "SELECT outcome_json FROM merge_operations WHERE operation=?1 AND session=?2",
        params![operation, session],
        |row| row.get(0),
    )?;
    Ok(Some(decode_bounded(
        &bytes,
        MAX_OUTCOME_BYTES,
        "merge outcome",
    )?))
}

fn read_pending_resolution(
    connection: &Connection,
    repository: &str,
    operation: &str,
    session: &str,
) -> Result<PendingResolution> {
    let (kind, request, expected, intent_length, manual_length, outcome_length): (
        String,
        String,
        i64,
        Option<usize>,
        Option<usize>,
        Option<usize>,
    ) = connection.query_row(
        "SELECT kind,request_digest,expected_generation,length(intent_json),length(manual_bytes),length(outcome_json) FROM merge_operations WHERE operation=?1 AND session=?2",
        params![operation, session],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    if kind != "resolve" || outcome_length.is_some() {
        return Err(Error::Invalid(
            "merge operation is not a pending resolution".into(),
        ));
    }
    let intent_length = intent_length
        .ok_or_else(|| Error::Invalid("pending merge resolution lacks intent".into()))?;
    if intent_length > MAX_RESOLUTION_INTENT_BYTES {
        return Err(Error::Limit(
            "merge resolution intent exceeds byte limit".into(),
        ));
    }
    if manual_length.is_some_and(|length| length > crate::store::MAX_FILE_BYTES) {
        return Err(Error::Limit(
            "pending merge resolution bytes exceed file limit".into(),
        ));
    }
    #[cfg(test)]
    run_after_pending_metadata();
    let (intent_bytes, manual_bytes): (Vec<u8>, Option<Vec<u8>>) = connection.query_row(
        "SELECT intent_json,manual_bytes FROM merge_operations WHERE operation=?1 AND session=?2",
        params![operation, session],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let intent: ResolutionIntent = decode_bounded(
        &intent_bytes,
        MAX_RESOLUTION_INTENT_BYTES,
        "merge resolution intent",
    )?;
    validate_id(&intent.conflict_id)?;
    if manual_bytes.as_ref().map(Vec::len) != manual_length {
        return Err(Error::Invalid(
            "pending merge resolution length changed while reading".into(),
        ));
    }
    let expected =
        u64::try_from(expected).map_err(|_| Error::Invalid("negative merge generation".into()))?;
    let reconstructed = match (intent.choice, manual_bytes) {
        (StoredResolutionChoice::Left, None) => Resolution::Left,
        (StoredResolutionChoice::Right, None) => Resolution::Right,
        (StoredResolutionChoice::Base, None) => Resolution::Base,
        (StoredResolutionChoice::Delete, None) => Resolution::Delete,
        (StoredResolutionChoice::Bytes, Some(bytes)) => Resolution::Bytes(bytes),
        _ => {
            return Err(Error::Invalid(
                "pending merge resolution payload does not match its intent".into(),
            ));
        }
    };
    let actual = request_digest(
        repository,
        "resolve",
        &(
            session,
            operation,
            expected,
            intent.conflict_id.as_str(),
            &reconstructed,
        ),
    )?;
    if request != actual {
        return Err(Error::Invalid(
            "pending merge resolution request digest mismatch".into(),
        ));
    }
    let manual_bytes = match reconstructed {
        Resolution::Bytes(bytes) => Some(bytes),
        Resolution::Left | Resolution::Right | Resolution::Base | Resolution::Delete => None,
    };
    Ok(PendingResolution {
        request,
        expected,
        intent,
        manual_bytes,
    })
}

/// Validate retained recovery roots with the same typed decoder as normal reads.
/// Caller already holds the offline lease and an exclusive SQLite connection.
pub(crate) fn validate_recovery(
    connection: &Connection,
    repository: &str,
    source: &SourceReader<'_>,
) -> Result<()> {
    let bound: String = connection.query_row(
        "SELECT repository FROM merge_repository WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    if bound != repository {
        return Err(Error::Invalid(
            "merge journal belongs to another repository".into(),
        ));
    }
    let mut sessions = connection.prepare("SELECT session FROM merge_sessions ORDER BY session")?;
    let mut checked_revisions = std::collections::BTreeSet::new();
    let mut checked_blobs = std::collections::BTreeSet::new();
    for (index, session) in sessions
        .query_map([], |row| row.get::<_, String>(0))?
        .enumerate()
    {
        if index >= MAX_OPERATION_ROWS as usize {
            return Err(Error::Limit(
                "recovery merge sessions exceed retained bound".into(),
            ));
        }
        let row = read_row(connection, &session?)?;
        for revision in std::iter::once(&row.state.left)
            .chain(std::iter::once(&row.state.right))
            .chain(row.state.base.iter())
            .chain(row.state.revision.iter())
            .chain(row.bases.iter())
        {
            if checked_revisions.insert(revision.clone()) {
                source.load_revision(revision)?;
            }
        }
        if let Some(plan) = row.plan {
            let mut check_blob = |id: &String| -> Result<()> {
                if checked_blobs.insert(id.clone()) {
                    let bytes = source.read_encoded(id)?;
                    if !matches!(
                        crate::store::inspect_encoded_object(id, &bytes)?,
                        crate::store::EncodedObject::Blob
                    ) {
                        return Err(Error::Invalid(
                            "merge plan recovery reference is not a blob".into(),
                        ));
                    }
                }
                Ok(())
            };
            for file in plan.files {
                for id in file
                    .base
                    .iter()
                    .chain(file.left.iter())
                    .chain(file.right.iter())
                    .chain(file.result.iter())
                {
                    check_blob(id)?;
                }
            }
            for conflict in plan.conflicts {
                for path in &conflict.paths {
                    for id in path
                        .base
                        .iter()
                        .chain(path.left.iter())
                        .chain(path.right.iter())
                    {
                        check_blob(id)?;
                    }
                }
                if let Some(resolutions) = conflict.resolution {
                    for resolution in resolutions {
                        if resolution.path_index >= conflict.paths.len() {
                            return Err(Error::Invalid(
                                "merge recovery resolution path is out of bounds".into(),
                            ));
                        }
                        if let Some(id) = resolution.blob {
                            check_blob(&id)?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn read_row(connection: &Connection, session: &str) -> Result<Row> {
    validate_id(session)?;
    let sizes: Option<(usize, Option<usize>)> = connection
        .query_row(
            "SELECT length(bases_json),length(plan_json) FROM merge_sessions WHERE session=?1",
            params![session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (bases_size, plan_size) = sizes.ok_or_else(|| Error::NotFound("merge session".into()))?;
    if bases_size > MAX_BASE_BYTES || plan_size.is_some_and(|size| size > MAX_PLAN_BYTES) {
        return Err(Error::Limit("merge journal payload exceeds bound".into()));
    }
    struct StoredRow {
        left: String,
        right: String,
        generation: i64,
        phase: String,
        selected: bool,
        base: Option<String>,
        bases: Vec<u8>,
        plan: Option<Vec<u8>>,
        revision: Option<String>,
    }
    let StoredRow { left, right, generation, phase, selected, base, bases, plan, revision } = connection.query_row(
        "SELECT left_revision,right_revision,generation,phase,base_selected,base_revision,bases_json,plan_json,result_revision FROM merge_sessions WHERE session=?1",
        params![session], |row| Ok(StoredRow {
            left: row.get(0)?, right: row.get(1)?, generation: row.get(2)?,
            phase: row.get(3)?, selected: row.get(4)?, base: row.get(5)?,
            bases: row.get(6)?, plan: row.get(7)?, revision: row.get(8)?,
        }),
    )?;
    validate_id(&left)?;
    validate_id(&right)?;
    for id in base.iter().chain(revision.iter()) {
        validate_id(id)?;
    }
    let bases: Vec<String> = decode_bounded(&bases, MAX_BASE_BYTES, "merge bases")?;
    if bases.len() > 100_000 || bases.windows(2).any(|ids| ids[0] >= ids[1]) {
        return Err(Error::Invalid(
            "merge journal base set is not bounded/canonical".into(),
        ));
    }
    for id in &bases {
        validate_id(id)?;
    }
    let plan: Option<plan::Plan> = plan
        .map(|plan| decode_bounded(&plan, MAX_PLAN_BYTES, "merge plan"))
        .transpose()?;
    let phase = parse_phase(&phase)?;
    let pending_operation: Option<String> = connection
        .query_row(
            "SELECT operation FROM merge_operations WHERE session=?1 AND outcome_json IS NULL",
            params![session],
            |row| row.get(0),
        )
        .optional()?;
    let state = MergeState {
        session: session.to_owned(),
        generation: u64::try_from(generation)
            .map_err(|_| Error::Invalid("negative merge generation".into()))?,
        phase,
        left,
        right,
        base,
        base_count: bases.len(),
        unresolved: plan.as_ref().map_or(0, |plan| {
            plan.conflicts
                .iter()
                .filter(|conflict| !conflict.resolved)
                .count()
        }),
        pending_operation,
        revision,
    };
    Ok(Row {
        state,
        base_selected: selected,
        bases,
        plan,
    })
}

fn request_digest<T: serde::Serialize>(
    repository: &str,
    kind: &str,
    request: &T,
) -> Result<String> {
    let mut output = DigestWriter(Sha256::new());
    serde_json::to_writer(
        &mut output,
        &("agentscm-merge-request-v1", repository, kind, request),
    )?;
    Ok(format!("{:x}", output.0.finalize()))
}
struct DigestWriter(Sha256);
impl io::Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode_bounded<T: serde::Serialize>(value: &T, limit: usize, label: &str) -> Result<Vec<u8>> {
    let mut output = BoundedBuffer {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    serde_json::to_writer(&mut output, value).map_err(|error| {
        if output.exceeded {
            Error::Limit(format!("{label} exceeds byte limit"))
        } else {
            Error::Json(error)
        }
    })?;
    Ok(output.bytes)
}
struct BoundedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}
impl io::Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.limit - self.bytes.len();
        if bytes.len() > remaining {
            self.exceeded = true;
            return Err(io::Error::other("bounded serialization exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn decode_bounded<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    limit: usize,
    label: &str,
) -> Result<T> {
    if bytes.len() > limit {
        return Err(Error::Limit(format!("{label} exceeds byte limit")));
    }
    Ok(serde_json::from_slice(bytes)?)
}
fn retry(
    connection: &Connection,
    operation: &str,
    request: &str,
) -> Result<Option<Option<MergeOutcome>>> {
    let existing: Option<(String, Option<Vec<u8>>)> = connection
        .query_row(
            "SELECT request_digest,outcome_json FROM merge_operations WHERE operation=?1",
            params![operation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match existing {
        None => Ok(None),
        Some((digest, outcome)) => {
            if digest != request {
                return Err(Error::Conflict(
                    "merge operation ID reused with different inputs".into(),
                ));
            }
            Ok(Some(
                outcome
                    .map(|bytes| decode_bounded(&bytes, MAX_OUTCOME_BYTES, "merge outcome"))
                    .transpose()?,
            ))
        }
    }
}
fn insert_completed(
    connection: &Connection,
    operation: &str,
    session: &str,
    kind: &str,
    request: &str,
    expected: u64,
    outcome: &MergeOutcome,
) -> Result<()> {
    connection.execute(
        "INSERT INTO merge_operations(operation,session,kind,request_digest,expected_generation,intent_json,manual_bytes,outcome_json) VALUES(?1,?2,?3,?4,?5,NULL,NULL,?6)",
        params![
            operation,
            session,
            kind,
            request,
            sql_generation(expected)?,
            encode_bounded(outcome, MAX_OUTCOME_BYTES, "merge outcome")?
        ],
    )?;
    Ok(())
}
fn ensure_no_pending(connection: &Connection, session: &str) -> Result<()> {
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM merge_operations WHERE session=?1 AND outcome_json IS NULL",
        params![session],
        |row| row.get(0),
    )?;
    if count != 0 {
        return Err(Error::Conflict(
            "merge has a pending operation to recover".into(),
        ));
    }
    Ok(())
}
fn ensure_capacity(connection: &Connection, operations: i64, active: i64) -> Result<()> {
    let operations_now: i64 =
        connection.query_row("SELECT count(*) FROM merge_operations", [], |row| {
            row.get(0)
        })?;
    let active_now: i64 = connection.query_row(
        "SELECT coalesce(sum(CASE WHEN phase IN ('completed','aborted') THEN 0 WHEN phase='publishing' THEN 1 ELSE 2 END),0) FROM merge_sessions",
        [],
        |row| row.get(0),
    )?;
    if operations_now + operations + active_now + active > MAX_OPERATION_ROWS {
        return Err(Error::Limit(
            "merge journal operation capacity reached".into(),
        ));
    }
    Ok(())
}
fn sql_generation(generation: u64) -> Result<i64> {
    i64::try_from(generation)
        .map_err(|_| Error::Limit("merge generation exceeds supported range".into()))
}
fn next_generation(generation: u64) -> Result<u64> {
    let next = generation
        .checked_add(1)
        .ok_or_else(|| Error::Limit("merge generation exhausted".into()))?;
    sql_generation(next)?;
    Ok(next)
}
fn require_generation(state: &MergeState, expected: u64) -> Result<()> {
    sql_generation(expected)?;
    if state.generation != expected {
        return Err(Error::Conflict("stale merge session generation".into()));
    }
    Ok(())
}
fn phase_name(phase: MergePhase) -> &'static str {
    match phase {
        MergePhase::Planning => "planning",
        MergePhase::AwaitingBase => "awaiting_base",
        MergePhase::Conflicts => "conflicts",
        MergePhase::Ready => "ready",
        MergePhase::Publishing => "publishing",
        MergePhase::Completed => "completed",
        MergePhase::Aborted => "aborted",
    }
}
fn parse_phase(value: &str) -> Result<MergePhase> {
    match value {
        "planning" => Ok(MergePhase::Planning),
        "awaiting_base" => Ok(MergePhase::AwaitingBase),
        "conflicts" => Ok(MergePhase::Conflicts),
        "ready" => Ok(MergePhase::Ready),
        "publishing" => Ok(MergePhase::Publishing),
        "completed" => Ok(MergePhase::Completed),
        "aborted" => Ok(MergePhase::Aborted),
        _ => Err(Error::Invalid("unknown merge phase".into())),
    }
}
fn check_page(limit: usize) -> Result<()> {
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(Error::Limit(
            "merge page limit must be between1 and1000".into(),
        ));
    }
    Ok(())
}
fn encode_cursor(session: &str, generation: u64, kind: &str, after: &str) -> Result<String> {
    let body = serde_json::to_vec(&("agentscm-merge-cursor-v1", session, generation, kind, after))?;
    let checksum = digest(&body);
    Ok(format!(
        "{}:{}",
        String::from_utf8(body).expect("JSON UTF8"),
        checksum
    ))
}
fn decode_cursor(
    cursor: Option<&str>,
    session: &str,
    generation: u64,
    kind: &str,
) -> Result<Option<String>> {
    match cursor {
        None => Ok(None),
        Some(cursor) => {
            if cursor.len() > 1024 {
                return Err(Error::Limit("merge cursor exceeds bound".into()));
            }
            let (body, checksum) = cursor
                .rsplit_once(':')
                .ok_or_else(|| Error::Invalid("malformed merge cursor".into()))?;
            if digest(body.as_bytes()) != checksum {
                return Err(Error::Invalid("merge cursor checksum mismatch".into()));
            }
            let (version, bound_session, bound_generation, bound_kind, after): (
                String,
                String,
                u64,
                String,
                String,
            ) = serde_json::from_str(body)?;
            if version != "agentscm-merge-cursor-v1"
                || bound_session != session
                || bound_generation != generation
                || bound_kind != kind
            {
                return Err(Error::Conflict(
                    "merge cursor subject or generation changed".into(),
                ));
            }
            validate_id(&after)?;
            Ok(Some(after))
        }
    }
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

    fn prepared(outcome: MergeOutcome) -> (String, u64, MergePhase) {
        match outcome {
            MergeOutcome::Prepared {
                session,
                generation,
                phase,
            } => (session, generation, phase),
            _ => panic!("expected prepared merge"),
        }
    }

    fn object_count(data: &Path) -> usize {
        std::fs::read_dir(data.join("objects"))
            .unwrap()
            .map(|directory| {
                std::fs::read_dir(directory.unwrap().path())
                    .unwrap()
                    .count()
            })
            .sum()
    }

    fn after_pending_metadata(callback: impl FnOnce() + 'static) {
        AFTER_PENDING_METADATA.with(|slot| {
            assert!(slot.borrow_mut().replace(Box::new(callback)).is_none());
        });
    }

    fn conflict_fixture(operation: &str) -> (TempDir, Engine, MergeJournal, String, u64, String) {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let base = engine.checkpoint(None, &[put("file", b"base\n")]).unwrap();
        let left = engine
            .checkpoint(Some(&base), &[put("file", b"left\n")])
            .unwrap();
        let right = engine
            .checkpoint(Some(&base), &[put("file", b"right\n")])
            .unwrap();
        let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
        let (session, generation, phase) =
            prepared(journal.begin(&engine, operation, &left, &right).unwrap());
        assert_eq!(phase, MergePhase::Conflicts);
        let conflict = journal.conflicts(&session, None, 1).unwrap().entries[0]
            .id
            .clone();
        (data, engine, journal, session, generation, conflict)
    }

    #[test]
    fn pending_manual_resolution_survives_reopen_and_binds_exact_input() {
        let (data, engine, mut journal, session, generation, conflict) =
            conflict_fixture("begin-durable");
        let original = Resolution::Bytes(b"durable choice\n".to_vec());
        let objects_before_prepare = object_count(data.path());
        assert!(
            journal
                .prepare_resolution(
                    &session,
                    "resolve-durable",
                    generation,
                    &conflict,
                    &original,
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(object_count(data.path()), objects_before_prepare);
        let state = journal.inspect(&session).unwrap();
        assert_eq!(state.generation, generation);
        assert_eq!(state.phase, MergePhase::Conflicts);
        assert_eq!(state.pending_operation.as_deref(), Some("resolve-durable"));
        drop(journal);

        let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
        assert!(matches!(
            journal.resolve(
                &engine,
                &session,
                "resolve-durable",
                generation,
                &conflict,
                Resolution::Bytes(b"different choice\n".to_vec()),
            ),
            Err(Error::Conflict(_))
        ));
        let recovered = journal.recover(&engine, &session).unwrap();
        assert_eq!(
            journal
                .resolve(
                    &engine,
                    &session,
                    "resolve-durable",
                    generation,
                    &conflict,
                    original,
                )
                .unwrap(),
            recovered
        );
        let detail = journal.conflict(&session, &conflict).unwrap();
        assert!(detail.resolved);
        let blob = detail.resolution.unwrap()[0].blob.clone().unwrap();
        engine.read_encoded(&blob).unwrap();
        let connection = journal.connect(false).unwrap();
        let retained: (Option<usize>, Option<usize>) = connection
            .query_row(
                "SELECT length(intent_json),length(manual_bytes) FROM merge_operations WHERE operation='resolve-durable'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(retained, (None, None));
    }

    #[test]
    fn resolution_snapshot_survives_concurrent_completion_payload_cleanup() {
        let (data, engine, mut journal, session, generation, conflict) =
            conflict_fixture("begin-snapshot-complete");
        let resolution = Resolution::Bytes(b"snapshot completion\n".to_vec());
        journal
            .prepare_resolution(
                &session,
                "resolve-snapshot-complete",
                generation,
                &conflict,
                &resolution,
            )
            .unwrap();

        let terminal = std::rc::Rc::new(std::cell::RefCell::new(None));
        let terminal_from_hook = terminal.clone();
        let data_path = data.path().to_owned();
        let engine_from_hook = engine.clone();
        let session_from_hook = session.clone();
        let conflict_from_hook = conflict.clone();
        let resolution_from_hook = resolution.clone();
        after_pending_metadata(move || {
            let mut other = MergeJournal::open(data_path, "repository").unwrap();
            let outcome = other
                .resolve(
                    &engine_from_hook,
                    &session_from_hook,
                    "resolve-snapshot-complete",
                    generation,
                    &conflict_from_hook,
                    resolution_from_hook,
                )
                .unwrap();
            *terminal_from_hook.borrow_mut() = Some(outcome);
        });

        let recovered = journal.recover(&engine, &session).unwrap();
        let completed = terminal.borrow().clone().expect("hook completed operation");
        assert!(matches!(completed, MergeOutcome::Resolved { .. }));
        assert_eq!(recovered, completed);
        assert_eq!(
            journal
                .resolve(
                    &engine,
                    &session,
                    "resolve-snapshot-complete",
                    generation,
                    &conflict,
                    resolution,
                )
                .unwrap(),
            completed
        );
    }

    #[test]
    fn resolution_snapshot_survives_concurrent_abort_payload_cleanup() {
        let (data, engine, mut journal, session, generation, conflict) =
            conflict_fixture("begin-snapshot-abort");
        let resolution = Resolution::Bytes(b"snapshot abort\n".to_vec());
        journal
            .prepare_resolution(
                &session,
                "resolve-snapshot-abort",
                generation,
                &conflict,
                &resolution,
            )
            .unwrap();

        let terminal = std::rc::Rc::new(std::cell::RefCell::new(None));
        let terminal_from_hook = terminal.clone();
        let data_path = data.path().to_owned();
        let session_from_hook = session.clone();
        after_pending_metadata(move || {
            let mut other = MergeJournal::open(data_path, "repository").unwrap();
            let outcome = other
                .abort(&session_from_hook, "abort-snapshot", generation)
                .unwrap();
            *terminal_from_hook.borrow_mut() = Some(outcome);
        });

        let recovered = journal.recover(&engine, &session).unwrap();
        let aborted = terminal.borrow().clone().expect("hook aborted operation");
        assert!(matches!(aborted, MergeOutcome::Aborted { .. }));
        assert_eq!(recovered, aborted);
        assert_eq!(
            journal
                .resolve(
                    &engine,
                    &session,
                    "resolve-snapshot-abort",
                    generation,
                    &conflict,
                    resolution,
                )
                .unwrap(),
            aborted
        );
        let state = journal.inspect(&session).unwrap();
        assert_eq!(state.phase, MergePhase::Aborted);
        assert_eq!(state.pending_operation, None);
    }

    #[test]
    fn abort_clears_pending_resolution_payload_and_cancels_exact_retry() {
        let (_data, engine, mut journal, session, generation, conflict) =
            conflict_fixture("begin-abort");
        let resolution = Resolution::Bytes(b"discarded choice\n".to_vec());
        journal
            .prepare_resolution(
                &session,
                "resolve-abort",
                generation,
                &conflict,
                &resolution,
            )
            .unwrap();
        let aborted = journal
            .abort(&session, "abort-pending", generation)
            .unwrap();
        assert_eq!(
            journal
                .resolve(
                    &engine,
                    &session,
                    "resolve-abort",
                    generation,
                    &conflict,
                    resolution,
                )
                .unwrap(),
            aborted
        );
        let connection = journal.connect(false).unwrap();
        let retained: (Option<usize>, Option<usize>) = connection
            .query_row(
                "SELECT length(intent_json),length(manual_bytes) FROM merge_operations WHERE operation='resolve-abort'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(retained, (None, None));
        assert!(journal.recover(&engine, &session).is_err());
    }

    #[test]
    fn structural_resolution_rejection_allows_a_corrected_operation() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let base = engine
            .checkpoint(None, &[put("Dir/file", b"base\n")])
            .unwrap();
        let left = engine
            .checkpoint(
                Some(&base),
                &[delete("Dir/file"), put("dir/keep", b"keep\n")],
            )
            .unwrap();
        let right = engine
            .checkpoint(Some(&base), &[put("Dir/file", b"right\n")])
            .unwrap();
        let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
        let (session, generation, phase) = prepared(
            journal
                .begin(&engine, "begin-structural-rejection", &left, &right)
                .unwrap(),
        );
        assert_eq!(phase, MergePhase::Conflicts);
        let conflict = journal.conflicts(&session, None, 1).unwrap().entries[0]
            .id
            .clone();

        let rejected = journal
            .resolve(
                &engine,
                &session,
                "reject-structural-choice",
                generation,
                &conflict,
                Resolution::Right,
            )
            .unwrap();
        assert_eq!(
            rejected,
            MergeOutcome::Rejected {
                session: session.clone(),
                generation,
                reason: MergeRejectionReason::StructuralConflict,
            }
        );
        let state = journal.inspect(&session).unwrap();
        assert_eq!(state.generation, generation);
        assert_eq!(state.phase, MergePhase::Conflicts);
        assert_eq!(state.pending_operation, None);
        let detail = journal.conflict(&session, &conflict).unwrap();
        assert!(!detail.resolved);
        assert_eq!(detail.resolution, None);
        let retained: (Option<usize>, Option<usize>, Option<usize>) = journal
            .connect(false)
            .unwrap()
            .query_row(
                "SELECT length(intent_json),length(manual_bytes),length(outcome_json) FROM merge_operations WHERE operation='reject-structural-choice'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(retained.0, None);
        assert_eq!(retained.1, None);
        assert!(retained.2.is_some());
        assert_eq!(
            journal
                .resolve(
                    &engine,
                    &session,
                    "reject-structural-choice",
                    generation,
                    &conflict,
                    Resolution::Right,
                )
                .unwrap(),
            rejected
        );
        assert!(matches!(
            journal.resolve(
                &engine,
                &session,
                "reject-structural-choice",
                generation,
                &conflict,
                Resolution::Left,
            ),
            Err(Error::Conflict(_))
        ));

        let corrected = journal
            .resolve(
                &engine,
                &session,
                "accept-corrected-choice",
                generation,
                &conflict,
                Resolution::Left,
            )
            .unwrap();
        assert_eq!(
            corrected,
            MergeOutcome::Resolved {
                session: session.clone(),
                generation: generation + 1,
                phase: MergePhase::Ready,
            }
        );
        let detail = journal.conflict(&session, &conflict).unwrap();
        assert!(detail.resolved);
        assert_eq!(detail.resolution.unwrap()[0].blob, None);
    }

    #[test]
    fn complete_uses_one_budget_with_repeated_resolution_blob_references() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let base = engine
            .checkpoint(None, &[put("first", b"base\n"), put("second", b"base\n")])
            .unwrap();
        let left = engine
            .checkpoint(
                Some(&base),
                &[put("first", b"left\n"), put("second", b"left\n")],
            )
            .unwrap();
        let right = engine
            .checkpoint(
                Some(&base),
                &[put("first", b"right\n"), put("second", b"right\n")],
            )
            .unwrap();
        let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
        let (session, mut generation, phase) = prepared(
            journal
                .begin(&engine, "begin-budget", &left, &right)
                .unwrap(),
        );
        assert_eq!(phase, MergePhase::Conflicts);
        let conflicts = journal.conflicts(&session, None, MAX_PAGE).unwrap().entries;
        assert_eq!(conflicts.len(), 2);
        for (index, conflict) in conflicts.into_iter().enumerate() {
            let outcome = journal
                .resolve(
                    &engine,
                    &session,
                    &format!("resolve-budget-{index}"),
                    generation,
                    &conflict.id,
                    Resolution::Right,
                )
                .unwrap();
            generation = match outcome {
                MergeOutcome::Resolved { generation, .. } => generation,
                _ => panic!("expected resolved merge"),
            };
        }
        assert_eq!(journal.inspect(&session).unwrap().phase, MergePhase::Ready);
        let operation = "complete-budget";
        let request = request_digest(
            "repository",
            "complete",
            &(session.as_str(), operation, generation),
        )
        .unwrap();
        let mut connection = journal.connect(false).unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute(
            "INSERT INTO merge_operations(operation,session,kind,request_digest,expected_generation,intent_json,manual_bytes,outcome_json) VALUES(?1,?2,'complete',?3,?4,NULL,NULL,NULL)",
            params![operation, session, request, sql_generation(generation).unwrap()],
        )
        .unwrap();
        tx.execute(
            "UPDATE merge_sessions SET phase='publishing' WHERE session=?1",
            params![session],
        )
        .unwrap();
        tx.commit().unwrap();
        // Measure actual parser reads, including revisits, rather than summing unique
        // descriptors. The limit admits all three qualifications, but no writer reads.
        // An unmetered writer would therefore acknowledge this exact candidate.
        struct CountedObjects<'a> {
            engine: &'a Engine,
            bytes: std::cell::Cell<u64>,
        }
        struct CountedRead<'a> {
            inner: Box<dyn std::io::Read + 'a>,
            bytes: &'a std::cell::Cell<u64>,
        }
        impl std::io::Read for CountedRead<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let count = self.inner.read(output)?;
                self.bytes.set(self.bytes.get() + count as u64);
                Ok(count)
            }
        }
        impl crate::store::EncodedObjectReader for CountedObjects<'_> {
            fn open_encoded(&self, id: &str) -> Result<Box<dyn std::io::Read + '_>> {
                Ok(Box::new(CountedRead {
                    inner: crate::store::EncodedObjectReader::open_encoded(self.engine, id)?,
                    bytes: &self.bytes,
                }))
            }
        }
        let row = read_row(&journal.connect(false).unwrap(), &session).unwrap();
        let changes: Vec<_> = row
            .plan
            .unwrap()
            .files
            .into_iter()
            .filter(|file| file.result != file.left)
            .map(|file| BlobChange {
                path: file.path,
                blob: file.result,
            })
            .collect();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].blob, changes[1].blob);
        let candidate = engine
            .checkpoint_with_blob_changes(&[left.clone(), right.clone()], &changes)
            .unwrap();
        let counted = CountedObjects {
            engine: &engine,
            bytes: std::cell::Cell::new(0),
        };
        let source = SourceReader::new(&counted);
        for root in [&left, &right, &candidate] {
            sync::qualify_reader(&source, std::slice::from_ref(root)).unwrap();
        }
        let qualification_bytes = counted.bytes.get();
        assert!(qualification_bytes > 0);
        assert!(matches!(
            journal.finish_with_read_limit(&engine, &session, operation, qualification_bytes),
            Err(Error::Limit(_))
        ));
        let pending = journal.inspect(&session).unwrap();
        assert_eq!(pending.generation, generation);
        assert_eq!(pending.phase, MergePhase::Publishing);
        assert_eq!(pending.pending_operation.as_deref(), Some(operation));
        let recovered = journal.recover(&engine, &session).unwrap();
        assert!(matches!(
            recovered,
            MergeOutcome::Completed { revision, .. } if revision == candidate
        ));
    }
}
