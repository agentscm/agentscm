use std::path::Path;
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use sha2::{Digest, Sha256};

use crate::checks::{self, CheckAttempt, CheckEvidence, SourcePathsRequest};
use crate::context::{
    BaseState, ChangeId, ChangeState, ContextReadiness, ReviewContext, SUPPORTED_CAPABILITIES,
};
use crate::model::{
    Approval, EventKind, IntegrateRequest, IntegrationEvent, LandingPreparation,
    PrepareLandingRequest, Revision, TargetState,
};
use crate::store::{Engine, create_directory_durable, validate_id};
use crate::{Error, Result};

pub mod workflows;

const APPLICATION_ID: i64 = 0x4153_434d;
const SCHEMA_VERSION: i64 = 7;
const CURRENT_SEMANTICS_VERSION: i64 = 2;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_EVENTS: usize = 1_000;
const MAX_LANDING_CHAIN: usize = 100_000;

pub struct Authority {
    connection: Connection,
    _access: crate::local_recovery::AccessGuard,
    read_only: bool,
}

impl Authority {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        crate::local_recovery::check_before_create(path.as_ref(), false)?;
        create_directory_durable(path.as_ref())?;
        let access = crate::local_recovery::AccessGuard::shared(path.as_ref())?;
        let mut connection = Connection::open_with_flags(
            path.as_ref().join("state.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
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
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        let foreign_keys: i64 =
            connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        if synchronous != 2 {
            return Err(Error::Invalid(
                "SQLite refused FULL synchronous mode".to_owned(),
            ));
        }
        if foreign_keys != 1 {
            return Err(Error::Invalid(
                "SQLite refused foreign-key enforcement".to_owned(),
            ));
        }
        initialize_schema(&mut connection)?;
        Ok(Self {
            connection,
            _access: access,
            read_only: false,
        })
    }

    /// Read existing current-profile authority without migration or logical writes.
    /// Requires an existing managed access lock; a missing lock is not created.
    /// SQLite may use WAL/SHM sidecars; this is not a hostile-filesystem sandbox.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let root = path.as_ref();
        let directory = std::fs::symlink_metadata(root)?;
        let database = root.join("state.sqlite");
        let metadata = std::fs::symlink_metadata(&database)?;
        if !directory.is_dir()
            || directory.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.file_type().is_symlink()
        {
            return Err(Error::Invalid(
                "read-only authority requires an existing directory and regular database".into(),
            ));
        }
        let access = crate::local_recovery::AccessGuard::shared_existing(root)?;
        let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        let application: i64 =
            connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let schema: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if application != APPLICATION_ID || schema != SCHEMA_VERSION {
            return Err(Error::Invalid("read-only authority requires current application/schema; migrate explicitly with a writer".into()));
        }
        let (count, epoch, repository): (i64, i64, String) = connection.query_row(
            "SELECT count(*),policy_epoch,repository_id FROM authority_state WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if count != 1
            || epoch < 0
            || repository.len() != 32
            || !repository
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Invalid("invalid current authority state".into()));
        }
        validate_current_tables(&connection)?;
        let authority = Self {
            connection,
            _access: access,
            read_only: true,
        };
        authority.events(0, 1)?;
        authority.target("main")?;
        for result in [
            authority
                .change(&ChangeId {
                    repository: repository.clone(),
                    id: "readonly-schema-probe".into(),
                })
                .map(|_| ()),
            authority.review_context(&"0".repeat(64)).map(|_| ()),
            authority.check_attempt("readonly-schema-probe").map(|_| ()),
            authority.check_evidence(&"0".repeat(64)).map(|_| ()),
        ] {
            match result {
                Ok(()) | Err(Error::NotFound(_)) => (),
                Err(error) => return Err(error),
            }
        }
        Ok(authority)
    }

    fn require_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::Conflict("authority was opened read-only".into()));
        }
        Ok(())
    }

    pub fn repository_id(&self) -> Result<String> {
        self.connection
            .query_row(
                "SELECT repository_id FROM authority_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(Error::from)
    }

    pub fn policy_epoch(&self) -> Result<u64> {
        let value = read_policy(&self.connection)?;
        sql_u64("policy epoch", value)
    }

    pub fn advance_policy(&mut self, expected: u64) -> Result<u64> {
        self.require_writable()?;
        let expected = sql_i64("policy epoch", expected)?;
        let next = expected
            .checked_add(1)
            .ok_or_else(|| Error::Limit("policy epoch exhausted".to_owned()))?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "UPDATE authority_state SET policy_epoch = ?1 WHERE singleton = 1 AND policy_epoch = ?2",
            params![next, expected],
        )? != 1 {
            return Err(Error::Conflict("policy epoch changed before it could be advanced".to_owned()));
        }
        tx.commit()?;
        sql_u64("policy epoch", next)
    }

    pub fn start_change(&mut self, context: &ReviewContext) -> Result<ChangeState> {
        self.require_writable()?;
        context.validate()?;
        if context.change_version != 1 {
            return Err(Error::Invalid(
                "a new change must start at version 1".to_owned(),
            ));
        }
        let canonical = context.canonical_bytes()?;
        let digest = context.digest()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_context_authority(&tx, context)?;
        insert_review_context(&tx, context.version, &digest, &canonical)?;
        if tx.execute(
            "INSERT INTO changes(repository, change_id, version, context_digest) VALUES (?1, ?2, 1, ?3) ON CONFLICT(repository, change_id) DO NOTHING",
            params![context.change.repository, context.change.id, digest],
        )? != 1 {
            return Err(Error::Conflict(format!("change {} already exists", context.change.id)));
        }
        tx.commit()?;
        Ok(ChangeState {
            id: context.change.clone(),
            version: 1,
            context_digest: digest,
        })
    }

    pub fn update_change_context(
        &mut self,
        expected_version: u64,
        context: &ReviewContext,
    ) -> Result<ChangeState> {
        self.require_writable()?;
        context.validate()?;
        let expected = sql_i64("expected change version", expected_version)?;
        let next = expected
            .checked_add(1)
            .ok_or_else(|| Error::Limit("change version exhausted".to_owned()))?;
        if context.change_version != sql_u64("change version", next)? {
            return Err(Error::Invalid(format!(
                "review context version {} must follow expected change version {expected_version}",
                context.change_version
            )));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = apply_context_update(&tx, expected_version, context)?;
        tx.commit()?;
        Ok(state)
    }

    pub fn change(&self, id: &ChangeId) -> Result<ChangeState> {
        id.validate()?;
        ensure_repository(&self.connection, &id.repository)?;
        read_change(&self.connection, id)?
            .ok_or_else(|| Error::NotFound(format!("change {}", id.id)))
    }

    pub fn review_context(&self, digest: &str) -> Result<ReviewContext> {
        validate_digest("context digest", digest)?;
        read_review_context(&self.connection, digest)?
            .ok_or_else(|| Error::NotFound(format!("review context {digest}")))
    }

    pub fn create_source_paths_attempt(
        &mut self,
        request: &SourcePathsRequest,
    ) -> Result<CheckAttempt> {
        self.require_writable()?;
        checks::create_source_paths_attempt(&mut self.connection, request)
    }

    pub fn run_source_paths_attempt(
        &mut self,
        engine: &Engine,
        attempt_id: &str,
        expected_run_generation: u64,
    ) -> Result<CheckAttempt> {
        self.require_writable()?;
        checks::run_source_paths_attempt(
            &mut self.connection,
            engine,
            attempt_id,
            expected_run_generation,
        )
    }

    pub fn cancel_check_attempt(
        &mut self,
        attempt_id: &str,
        expected_run_generation: u64,
        reason: &str,
    ) -> Result<CheckAttempt> {
        self.require_writable()?;
        checks::cancel_attempt(
            &mut self.connection,
            attempt_id,
            expected_run_generation,
            reason,
        )
    }

    pub fn recover_check_attempt(
        &mut self,
        attempt_id: &str,
        expected_run_generation: u64,
        reason: &str,
    ) -> Result<CheckAttempt> {
        self.require_writable()?;
        checks::recover_attempt(
            &mut self.connection,
            attempt_id,
            expected_run_generation,
            reason,
        )
    }

    pub fn check_attempt(&self, attempt_id: &str) -> Result<CheckAttempt> {
        checks::read_attempt(&self.connection, attempt_id)
    }

    pub fn check_evidence(&self, digest: &str) -> Result<CheckEvidence> {
        checks::read_evidence(&self.connection, digest)
    }

    #[cfg(unix)]
    pub fn create_command_attempt(
        &mut self,
        request: &crate::execution::CommandCheckRequest,
    ) -> Result<crate::execution::CommandAttempt> {
        self.require_writable()?;
        crate::execution::create_command_attempt(&mut self.connection, request)
    }

    #[cfg(unix)]
    pub fn run_command_attempt(
        &mut self,
        engine: &Engine,
        backend: &crate::execution::OciBackend,
        work_dir: &Path,
        attempt_id: &str,
        expected_run_generation: u64,
        cancel: &crate::execution::Cancellation,
    ) -> Result<crate::execution::CommandAttempt> {
        self.require_writable()?;
        crate::execution::run_command_attempt(
            &mut self.connection,
            engine,
            backend,
            work_dir,
            attempt_id,
            expected_run_generation,
            cancel,
        )
    }

    #[cfg(unix)]
    pub fn cancel_command_attempt(
        &mut self,
        backend: Option<&crate::execution::OciBackend>,
        work_dir: &Path,
        attempt_id: &str,
        expected_run_generation: u64,
        reason: &str,
    ) -> Result<crate::execution::CommandAttempt> {
        self.require_writable()?;
        crate::execution::stop_command_attempt(
            &mut self.connection,
            backend,
            work_dir,
            attempt_id,
            expected_run_generation,
            crate::execution::ExecutionOutcome::Cancelled,
            false,
            reason,
        )
    }

    #[cfg(unix)]
    pub fn recover_command_attempt(
        &mut self,
        backend: Option<&crate::execution::OciBackend>,
        work_dir: &Path,
        attempt_id: &str,
        expected_run_generation: u64,
        abandon_unconfirmed: bool,
        reason: &str,
    ) -> Result<crate::execution::CommandAttempt> {
        self.require_writable()?;
        crate::execution::stop_command_attempt(
            &mut self.connection,
            backend,
            work_dir,
            attempt_id,
            expected_run_generation,
            crate::execution::ExecutionOutcome::Incomplete,
            abandon_unconfirmed,
            reason,
        )
    }

    #[cfg(unix)]
    pub fn command_attempt(&self, attempt_id: &str) -> Result<crate::execution::CommandAttempt> {
        crate::execution::read_command_attempt(&self.connection, attempt_id)
    }

    /// Publishes source objects through the engine only; it grants no approval or evidence.
    #[cfg(unix)]
    pub fn command_candidate(
        &self,
        engine: &Engine,
        attempt_id: &str,
    ) -> Result<crate::execution::CommandCandidate> {
        crate::execution::command_candidate(&self.connection, engine, attempt_id)
    }

    #[cfg(unix)]
    pub fn command_record(&self, digest: &str) -> Result<crate::execution::ExecutionRecord> {
        crate::execution::read_command_record(&self.connection, digest)
    }

    pub fn approve(&mut self, approval: &Approval) -> Result<()> {
        self.require_writable()?;
        validate_identifier("approval ID", &approval.approval_id)?;
        validate_identifier("target name", &approval.target)?;
        validate_id(&approval.revision)?;
        validate_digest("context digest", &approval.context_digest)?;
        let policy_epoch = sql_i64("policy epoch", approval.policy_epoch)?;
        let (change, change_version) = require_change_binding(
            approval.change.as_ref(),
            approval.change_version,
            "new approval",
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_current_context(
            &tx,
            change,
            change_version,
            &approval.context_digest,
            &approval.target,
            &approval.revision,
            policy_epoch,
        )?;
        if tx
            .query_row(
                "SELECT 1 FROM approvals WHERE approval_id = ?1",
                [&approval.approval_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(Error::Conflict(format!(
                "approval ID {} is already registered",
                approval.approval_id
            )));
        }
        tx.execute(
            "INSERT INTO approvals(approval_id, target, revision, context_digest, policy_epoch, change_repository, change_id, change_version, semantics_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 2)",
            params![approval.approval_id, approval.target, approval.revision, approval.context_digest, policy_epoch, change.repository, change.id, sql_i64("change version", change_version)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn revoke_approval(&mut self, id: &str) -> Result<()> {
        self.require_writable()?;
        validate_identifier("approval ID", id)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "UPDATE approvals SET revoked = 1 WHERE approval_id = ?1",
            [id],
        )? == 0
        {
            return Err(Error::NotFound(format!("approval {id}")));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn target(&self, name: &str) -> Result<Option<TargetState>> {
        validate_identifier("target name", name)?;
        read_target(&self.connection, name)
    }

    pub fn integrate(
        &mut self,
        engine: &Engine,
        request: &IntegrateRequest,
    ) -> Result<IntegrationEvent> {
        self.require_writable()?;
        validate_identifier("operation ID", &request.operation_id)?;
        if request.change.is_some() != request.change_version.is_some() {
            return Err(Error::Invalid(
                "change identity and version must be supplied together".to_owned(),
            ));
        }
        let request_digest = integration_request_digest(request);
        if let Some((stored, event)) = lookup_operation(&self.connection, &request.operation_id)? {
            return matching_retry(&request.operation_id, &request_digest, stored, event);
        }
        validate_integration_request(request)?;
        let (change, change_version) = require_change_binding(
            request.change.as_ref(),
            request.change_version,
            "new integration",
        )?;
        let candidate = engine.load_revision(&request.revision)?;
        if !matches!(candidate.version, 1 | 2) {
            return Err(Error::Invalid(format!(
                "revision {} has unsupported version {}",
                request.revision, candidate.version
            )));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((stored, event)) = lookup_operation(&tx, &request.operation_id)? {
            return matching_retry(&request.operation_id, &request_digest, stored, event);
        }
        let request_policy = sql_i64("policy epoch", request.policy_epoch)?;
        let current = read_target(&tx, &request.target)?;
        if current != request.expected_target {
            return Err(Error::Conflict(format!(
                "target {} no longer matches the expected revision and version",
                request.target
            )));
        }
        validate_current_context(
            &tx,
            change,
            change_version,
            &request.context_digest,
            &request.target,
            &request.revision,
            request_policy,
        )?;
        validate_approval(&tx, request, request_policy, change, change_version)?;
        validate_direct_parent(&candidate, current.as_ref())?;

        let next_version = match &current {
            Some(state) => state
                .version
                .checked_add(1)
                .ok_or_else(|| Error::Limit("target version exhausted".to_owned()))?,
            None => 1,
        };
        let next_version_sql = sql_i64("target version", next_version)?;
        match &current {
            Some(_) => {
                if tx.execute(
                    "UPDATE targets SET revision = ?1, version = ?2 WHERE name = ?3",
                    params![request.revision, next_version_sql, request.target],
                )? != 1
                {
                    return Err(Error::Conflict(format!(
                        "target {} disappeared during integration",
                        request.target
                    )));
                }
            }
            None => {
                tx.execute(
                    "INSERT INTO targets(name, revision, version) VALUES (?1, ?2, ?3)",
                    params![request.target, request.revision, next_version_sql],
                )?;
            }
        }
        let (before_revision, before_version) = match &current {
            Some(state) => (
                Some(state.revision.as_str()),
                Some(sql_i64("target version", state.version)?),
            ),
            None => (None, None),
        };
        tx.execute(
            "INSERT INTO integration_events(operation_id, target, before_revision, before_version, after_revision, after_version, approval_id, context_digest, policy_epoch, change_repository, change_id, change_version, semantics_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 2)",
            params![request.operation_id, request.target, before_revision, before_version, request.revision, next_version_sql, request.approval_id, request.context_digest, request_policy, change.repository, change.id, sql_i64("change version", change_version)?],
        )?;
        let sequence = tx.last_insert_rowid();
        if tx.execute(
            "UPDATE approvals SET consumed_event = ?1 WHERE approval_id = ?2 AND revoked = 0 AND consumed_event IS NULL",
            params![sequence, request.approval_id],
        )? != 1 {
            return Err(Error::Conflict(format!("approval {} was revoked or consumed during integration", request.approval_id)));
        }
        tx.execute("INSERT INTO operations(operation_id, request_digest, event_sequence) VALUES (?1, ?2, ?3)", params![request.operation_id, request_digest, sequence])?;
        let event = IntegrationEvent {
            kind: EventKind::Integrate,
            semantics_version: CURRENT_SEMANTICS_VERSION as u32,
            sequence: sql_u64("event sequence", sequence)?,
            operation_id: request.operation_id.clone(),
            target: request.target.clone(),
            before: current,
            after: TargetState {
                revision: request.revision.clone(),
                version: next_version,
            },
            approval_id: request.approval_id.clone(),
            context_digest: request.context_digest.clone(),
            change: Some(change.clone()),
            change_version: Some(change_version),
            policy_epoch: request.policy_epoch,
        };
        tx.commit()?;
        Ok(event)
    }

    /// Initializes an absent target at a fully published imported or snapshot head.
    /// Unlike `integrate`, the head may have any parent count; an existing target is never adopted.
    pub fn initialize_target(
        &mut self,
        engine: &Engine,
        request: &IntegrateRequest,
    ) -> Result<IntegrationEvent> {
        self.require_writable()?;
        validate_identifier("operation ID", &request.operation_id)?;
        let (change, change_version) = require_change_binding(
            request.change.as_ref(),
            request.change_version,
            "target initialization",
        )?;
        if request.expected_target.is_some() {
            return Err(Error::Invalid(
                "initialization requires an expected-absent target".to_owned(),
            ));
        }
        let request_digest = initialization_request_digest(request, change, change_version);
        if let Some((stored, event)) = lookup_operation(&self.connection, &request.operation_id)? {
            return initialization_retry(&request.operation_id, &request_digest, stored, event);
        }
        validate_integration_request(request)?;
        let candidate = engine.load_revision(&request.revision)?;
        if !matches!(candidate.version, 1 | 2) {
            return Err(Error::Invalid(format!(
                "revision {} has unsupported version {}",
                request.revision, candidate.version
            )));
        }
        crate::sync::qualify_source(engine, std::slice::from_ref(&request.revision))?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((stored, event)) = lookup_operation(&tx, &request.operation_id)? {
            return initialization_retry(&request.operation_id, &request_digest, stored, event);
        }
        let request_policy = sql_i64("policy epoch", request.policy_epoch)?;
        if read_target(&tx, &request.target)?.is_some() {
            return Err(Error::Conflict(format!(
                "target {} is already initialized; initialization cannot adopt or replace an existing target",
                request.target
            )));
        }
        validate_current_context(
            &tx,
            change,
            change_version,
            &request.context_digest,
            &request.target,
            &request.revision,
            request_policy,
        )?;
        validate_approval(&tx, request, request_policy, change, change_version)?;
        tx.execute(
            "INSERT INTO targets(name, revision, version) VALUES (?1, ?2, 1)",
            params![request.target, request.revision],
        )?;
        tx.execute(
            "INSERT INTO integration_events(operation_id, target, before_revision, before_version, after_revision, after_version, approval_id, context_digest, policy_epoch, change_repository, change_id, change_version, semantics_version, kind) VALUES (?1, ?2, NULL, NULL, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, 2, 'initialize')",
            params![request.operation_id, request.target, request.revision, request.approval_id, request.context_digest, request_policy, change.repository, change.id, sql_i64("change version", change_version)?],
        )?;
        let sequence = tx.last_insert_rowid();
        if tx.execute(
            "UPDATE approvals SET consumed_event = ?1 WHERE approval_id = ?2 AND revoked = 0 AND consumed_event IS NULL",
            params![sequence, request.approval_id],
        )? != 1 {
            return Err(Error::Conflict(format!("approval {} was revoked or consumed during initialization", request.approval_id)));
        }
        tx.execute("INSERT INTO operations(operation_id, request_digest, event_sequence) VALUES (?1, ?2, ?3)", params![request.operation_id, request_digest, sequence])?;
        let event = IntegrationEvent {
            kind: EventKind::Initialize,
            semantics_version: CURRENT_SEMANTICS_VERSION as u32,
            sequence: sql_u64("event sequence", sequence)?,
            operation_id: request.operation_id.clone(),
            target: request.target.clone(),
            before: None,
            after: TargetState {
                revision: request.revision.clone(),
                version: 1,
            },
            approval_id: request.approval_id.clone(),
            context_digest: request.context_digest.clone(),
            change: Some(change.clone()),
            change_version: Some(change_version),
            policy_epoch: request.policy_epoch,
        };
        tx.commit()?;
        Ok(event)
    }

    /// Builds the landing revision `[T, B]` for work T -> ... -> B and rebinds the Change to it.
    /// Never approves, consumes approvals, or moves a target; a serial landing without three-way merge.
    pub fn prepare_landing(
        &mut self,
        engine: &Engine,
        request: &PrepareLandingRequest,
    ) -> Result<LandingPreparation> {
        self.require_writable()?;
        validate_identifier("operation ID", &request.operation_id)?;
        validate_identifier("target name", &request.target)?;
        validate_id(&request.source_tip)?;
        validate_id(&request.expected_target.revision)?;
        if request.expected_target.version == 0 || request.change_version == 0 {
            return Err(Error::Invalid(
                "expected target and change versions must be positive".to_owned(),
            ));
        }
        sql_i64("expected target version", request.expected_target.version)?;
        let next_change = request
            .change_version
            .checked_add(1)
            .ok_or_else(|| Error::Limit("change version exhausted".to_owned()))?;
        sql_i64("change version", next_change)?;
        let context = &request.context;
        context.validate()?;
        let expected_base = BaseState {
            revision: request.expected_target.revision.clone(),
            target_version: request.expected_target.version,
        };
        if context.revision != request.source_tip
            || context.base.as_ref() != Some(&expected_base)
            || context.target != request.target
            || context.change_version != next_change
        {
            return Err(Error::Invalid(format!(
                "the supplied context must describe the work being landed: revision {} (source tip), base {}@{} (expected target), target {}, change version {next_change}",
                request.source_tip,
                expected_base.revision,
                expected_base.target_version,
                request.target
            )));
        }
        let request_digest = preparation_request_digest(request, &context.digest()?);
        if let Some(outcome) = lookup_preparation(&self.connection, request, &request_digest)? {
            return Ok(outcome);
        }
        let base = request.expected_target.clone();
        let base_revision = engine.load_revision(&base.revision)?;
        let mut cursor = engine.load_revision(&request.source_tip)?;
        let source_tree = cursor.tree.clone();
        let mut current = request.source_tip.clone();
        let mut steps = 0;
        while current != base.revision {
            steps += 1;
            if steps > MAX_LANDING_CHAIN {
                return Err(Error::Limit(format!(
                    "source chain exceeds {MAX_LANDING_CHAIN} first-parent steps"
                )));
            }
            let Some(parent) = cursor.parents.first().cloned() else {
                return Err(Error::Conflict(format!(
                    "workspace base diverged from target {} at {}: source tip {} does not descend from it by first parent; source edits are preserved and nothing was changed; recover by starting from the current target (for example a native merge session, docs/advanced/merge.md) or checkpointing the work onto the current target; preparation does not perform a three-way merge",
                    request.target, base.revision, request.source_tip
                )));
            };
            cursor = engine.load_revision(&parent)?;
            current = parent;
        }
        let revision = if request.source_tip == base.revision || source_tree == base_revision.tree {
            None
        } else {
            let landing = engine.publish_revision_tree(
                &[base.revision.clone(), request.source_tip.clone()],
                &source_tree,
            )?;
            // Full-closure qualification on every call; incremental qualification from an
            // admitted target is the upgrade path.
            crate::sync::qualify_source(engine, std::slice::from_ref(&landing))?;
            Some(landing)
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = lookup_preparation(&tx, request, &request_digest)? {
            return Ok(outcome);
        }
        if read_target(&tx, &request.target)?.as_ref() != Some(&base) {
            return Err(Error::Conflict(format!(
                "target {} no longer matches the expected revision and version; re-prepare from the current target",
                request.target
            )));
        }
        let outcome = match revision {
            Some(revision) => {
                let mut rebound = context.clone();
                rebound.revision = revision.clone();
                LandingPreparation::Prepared {
                    operation_id: request.operation_id.clone(),
                    target: request.target.clone(),
                    base,
                    source_tip: request.source_tip.clone(),
                    revision,
                    change: apply_context_update(&tx, request.change_version, &rebound)?,
                }
            }
            None => {
                validate_context_authority(&tx, context)?;
                match read_change(&tx, &context.change)? {
                    Some(state) if state.version == request.change_version => {}
                    _ => {
                        return Err(Error::Conflict(format!(
                            "change {} is missing or no longer at version {}",
                            context.change.id, request.change_version
                        )));
                    }
                }
                LandingPreparation::AlreadyCurrent {
                    operation_id: request.operation_id.clone(),
                    target: request.target.clone(),
                    base,
                    source_tip: request.source_tip.clone(),
                }
            }
        };
        tx.execute(
            "INSERT INTO landing_preparations(operation_id, request_digest, outcome_json) VALUES (?1, ?2, ?3)",
            params![request.operation_id, request_digest, serde_json::to_vec(&outcome)?],
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    pub fn events(&self, after: u64, limit: usize) -> Result<Vec<IntegrationEvent>> {
        if limit > MAX_EVENTS {
            return Err(Error::Limit(format!(
                "event limit exceeds maximum {MAX_EVENTS}"
            )));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT sequence, operation_id, target, before_revision, before_version, after_revision, after_version, approval_id, context_digest, policy_epoch, change_repository, change_id, change_version, semantics_version, kind FROM integration_events WHERE sequence > ?1 ORDER BY sequence LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                sql_i64("event sequence", after)?,
                i64::try_from(limit)
                    .map_err(|_| Error::Limit("event limit does not fit SQLite".to_owned()))?
            ],
            read_raw_event,
        )?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?.into_event()?);
        }
        Ok(events)
    }
}

/// Required table shapes for read-only opens and consistent local recovery.
pub(crate) fn validate_current_tables(connection: &Connection) -> Result<()> {
    for sql in [
        "SELECT digest,version,canonical_json FROM review_contexts LIMIT 0",
        "SELECT repository,change_id,version,context_digest FROM changes LIMIT 0",
        "SELECT approval_id,revision,revoked,consumed_event,semantics_version FROM approvals LIMIT 0",
        "SELECT name,revision,version FROM targets LIMIT 0",
        "SELECT operation_id,request_digest,event_sequence FROM operations LIMIT 0",
        "SELECT sequence,operation_id,kind,semantics_version,after_revision,context_digest FROM integration_events LIMIT 0",
        "SELECT operation_id,request_digest,outcome_json FROM landing_preparations LIMIT 0",
        "SELECT attempt_id,state,run_generation,evidence_digest FROM check_attempts LIMIT 0",
        "SELECT digest,attempt_id,canonical_json FROM check_evidence LIMIT 0",
        "SELECT attempt_id,state,run_generation,record_digest,custody_json FROM command_attempts LIMIT 0",
        "SELECT digest,attempt_id,canonical_json FROM command_evidence LIMIT 0",
        "SELECT target,digest,installed_policy_epoch,canonical_json FROM workflow_requirements LIMIT 0",
        "SELECT digest,requirement_digest,run_id,plan_digest,skill_digest,repository,revision,target,policy_epoch,change_repository,change_id,change_version,context_digest,canonical_json FROM workflow_receipts LIMIT 0",
    ] {
        connection.prepare(sql)?;
    }
    Ok(())
}

fn initialize_schema(connection: &mut Connection) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let application_id: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let schema_version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match (application_id, schema_version) {
        (0, 0) => {
            create_schema_v2(&tx)?;
            checks::create_schema_v3(&tx)?;
            create_schema_v4(&tx)?;
            create_schema_v5(&tx)?;
            create_schema_v6(&tx)?;
            workflows::create_schema_v7(&tx)?;
        }
        (APPLICATION_ID, 1) => {
            migrate_v1_to_v2(&tx)?;
            checks::create_schema_v3(&tx)?;
            create_schema_v4(&tx)?;
            create_schema_v5(&tx)?;
            create_schema_v6(&tx)?;
            workflows::create_schema_v7(&tx)?;
        }
        (APPLICATION_ID, 2) => {
            checks::create_schema_v3(&tx)?;
            create_schema_v4(&tx)?;
            create_schema_v5(&tx)?;
            create_schema_v6(&tx)?;
            workflows::create_schema_v7(&tx)?;
        }
        (APPLICATION_ID, 3) => {
            create_schema_v4(&tx)?;
            create_schema_v5(&tx)?;
            create_schema_v6(&tx)?;
            workflows::create_schema_v7(&tx)?;
        }
        (APPLICATION_ID, 4) => {
            create_schema_v5(&tx)?;
            create_schema_v6(&tx)?;
            workflows::create_schema_v7(&tx)?;
        }
        (APPLICATION_ID, 5) => {
            create_schema_v6(&tx)?;
            workflows::create_schema_v7(&tx)?;
        }
        (APPLICATION_ID, 6) => workflows::create_schema_v7(&tx)?,
        (APPLICATION_ID, SCHEMA_VERSION) => {
            tx.commit()?;
            return Ok(());
        }
        (APPLICATION_ID, other) => {
            return Err(Error::Invalid(format!(
                "unsupported authority schema version {other}"
            )));
        }
        (other, _) => {
            return Err(Error::Invalid(format!(
                "state.sqlite belongs to a different application ({other})"
            )));
        }
    }
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn create_schema_v4(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "ALTER TABLE integration_events ADD COLUMN kind TEXT NOT NULL DEFAULT 'integrate' CHECK (kind IN ('integrate', 'initialize'));
         CREATE TABLE landing_preparations (operation_id TEXT PRIMARY KEY CHECK (length(operation_id) BETWEEN 1 AND 128), request_digest TEXT NOT NULL CHECK (length(request_digest) = 64), outcome_json BLOB NOT NULL) STRICT;",
    )?;
    Ok(())
}

fn create_schema_v5(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE command_attempts (
            attempt_id TEXT PRIMARY KEY CHECK (length(attempt_id) BETWEEN 1 AND 128),
            request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
            repository TEXT NOT NULL,
            change_id TEXT NOT NULL,
            change_version INTEGER NOT NULL CHECK (change_version > 0),
            context_digest TEXT NOT NULL CHECK (length(context_digest) = 64),
            revision TEXT NOT NULL CHECK (length(revision) = 64),
            recipe_sha256 TEXT NOT NULL CHECK (length(recipe_sha256) = 64),
            toolchain_sha256 TEXT NOT NULL CHECK (length(toolchain_sha256) = 64),
            environment_sha256 TEXT NOT NULL CHECK (length(environment_sha256) = 64),
            recipe_json BLOB NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('pending', 'running', 'finished')),
            outcome TEXT CHECK (outcome IS NULL OR outcome IN ('success', 'failed', 'timed_out', 'cancelled', 'output_limit', 'unavailable', 'incomplete', 'indeterminate')),
            run_generation INTEGER NOT NULL CHECK (run_generation >= 0),
            record_digest TEXT UNIQUE CHECK (record_digest IS NULL OR length(record_digest) = 64),
            CHECK ((state = 'finished') = (record_digest IS NOT NULL)),
            CHECK ((state = 'finished') = (outcome IS NOT NULL))
        ) STRICT;
        CREATE TABLE command_evidence (
            digest TEXT PRIMARY KEY CHECK (length(digest) = 64),
            attempt_id TEXT NOT NULL UNIQUE REFERENCES command_attempts(attempt_id),
            canonical_json BLOB NOT NULL
        ) STRICT;
        CREATE INDEX command_success_subject ON command_attempts(
            repository, change_id, change_version, context_digest, revision,
            recipe_sha256, toolchain_sha256, environment_sha256, outcome
        );",
    )?;
    Ok(())
}

fn create_schema_v6(tx: &Transaction<'_>) -> Result<()> {
    // v5 runs persisted no cleanup custody, so recovery could not prove their containers gone.
    let running: i64 = tx.query_row(
        "SELECT count(*) FROM command_attempts WHERE state = 'running'",
        [],
        |row| row.get(0),
    )?;
    if running != 0 {
        return Err(Error::Conflict(
            "finish or remove running command attempts and their containers before upgrading to schema v6".to_owned(),
        ));
    }
    tx.execute_batch("ALTER TABLE command_attempts ADD COLUMN custody_json BLOB;")?;
    Ok(())
}

fn create_schema_v2(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE authority_state (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), policy_epoch INTEGER NOT NULL CHECK (policy_epoch >= 0), repository_id TEXT NOT NULL CHECK (length(repository_id) = 32)) STRICT;
         INSERT INTO authority_state(singleton, policy_epoch, repository_id) VALUES (1, 0, lower(hex(randomblob(16))));
         CREATE TABLE review_contexts (digest TEXT PRIMARY KEY CHECK (length(digest) = 64), version INTEGER NOT NULL CHECK (version = 1), canonical_json BLOB NOT NULL) STRICT;
         CREATE TABLE changes (repository TEXT NOT NULL, change_id TEXT NOT NULL, version INTEGER NOT NULL CHECK (version > 0), context_digest TEXT NOT NULL, PRIMARY KEY(repository, change_id), FOREIGN KEY (context_digest) REFERENCES review_contexts(digest)) STRICT;
         CREATE TABLE approvals (approval_id TEXT PRIMARY KEY, target TEXT NOT NULL, revision TEXT NOT NULL, context_digest TEXT NOT NULL, policy_epoch INTEGER NOT NULL CHECK (policy_epoch >= 0), revoked INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0, 1)), consumed_event INTEGER UNIQUE, change_repository TEXT, change_id TEXT, change_version INTEGER CHECK (change_version IS NULL OR change_version > 0), semantics_version INTEGER NOT NULL CHECK (semantics_version IN (1, 2)), CHECK ((change_repository IS NULL) = (change_id IS NULL)), CHECK ((change_id IS NULL) = (change_version IS NULL)), CHECK (length(approval_id) BETWEEN 1 AND 128), CHECK (length(target) BETWEEN 1 AND 128), CHECK (length(revision) = 64), CHECK (length(context_digest) = 64)) STRICT;
         CREATE TABLE targets (name TEXT PRIMARY KEY, revision TEXT NOT NULL, version INTEGER NOT NULL CHECK (version > 0), CHECK (length(name) BETWEEN 1 AND 128), CHECK (length(revision) = 64)) STRICT;
         CREATE TABLE integration_events (sequence INTEGER PRIMARY KEY AUTOINCREMENT, operation_id TEXT NOT NULL UNIQUE, target TEXT NOT NULL, before_revision TEXT, before_version INTEGER, after_revision TEXT NOT NULL, after_version INTEGER NOT NULL CHECK (after_version > 0), approval_id TEXT NOT NULL, context_digest TEXT NOT NULL, policy_epoch INTEGER NOT NULL CHECK (policy_epoch >= 0), change_repository TEXT, change_id TEXT, change_version INTEGER CHECK (change_version IS NULL OR change_version > 0), semantics_version INTEGER NOT NULL CHECK (semantics_version IN (1, 2)), CHECK ((before_revision IS NULL) = (before_version IS NULL)), CHECK (before_version IS NULL OR before_version > 0), CHECK ((change_repository IS NULL) = (change_id IS NULL)), CHECK ((change_id IS NULL) = (change_version IS NULL)), FOREIGN KEY (approval_id) REFERENCES approvals(approval_id)) STRICT;
         CREATE TABLE operations (operation_id TEXT PRIMARY KEY, request_digest TEXT NOT NULL CHECK (length(request_digest) = 64), event_sequence INTEGER NOT NULL UNIQUE, FOREIGN KEY (event_sequence) REFERENCES integration_events(sequence)) STRICT;",
    )?;
    Ok(())
}

fn migrate_v1_to_v2(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "ALTER TABLE authority_state ADD COLUMN repository_id TEXT;
         UPDATE authority_state SET repository_id = lower(hex(randomblob(16))) WHERE singleton = 1 AND repository_id IS NULL;
         CREATE TABLE review_contexts (digest TEXT PRIMARY KEY CHECK (length(digest) = 64), version INTEGER NOT NULL CHECK (version = 1), canonical_json BLOB NOT NULL) STRICT;
         CREATE TABLE changes (repository TEXT NOT NULL, change_id TEXT NOT NULL, version INTEGER NOT NULL CHECK (version > 0), context_digest TEXT NOT NULL, PRIMARY KEY(repository, change_id), FOREIGN KEY (context_digest) REFERENCES review_contexts(digest)) STRICT;
         ALTER TABLE approvals ADD COLUMN change_repository TEXT;
         ALTER TABLE approvals ADD COLUMN change_id TEXT;
         ALTER TABLE approvals ADD COLUMN change_version INTEGER;
         ALTER TABLE approvals ADD COLUMN semantics_version INTEGER NOT NULL DEFAULT 1;
         ALTER TABLE integration_events ADD COLUMN change_repository TEXT;
         ALTER TABLE integration_events ADD COLUMN change_id TEXT;
         ALTER TABLE integration_events ADD COLUMN change_version INTEGER;
         ALTER TABLE integration_events ADD COLUMN semantics_version INTEGER NOT NULL DEFAULT 1;
         UPDATE approvals SET revoked = 1 WHERE consumed_event IS NULL AND revoked = 0;",
    )?;
    Ok(())
}

/// Moves a Change from `expected_version` to `context.change_version` inside the caller's transaction.
fn apply_context_update(
    tx: &Transaction<'_>,
    expected_version: u64,
    context: &ReviewContext,
) -> Result<ChangeState> {
    let expected = sql_i64("expected change version", expected_version)?;
    let next = sql_i64("change version", context.change_version)?;
    let canonical = context.canonical_bytes()?;
    let digest = context.digest()?;
    validate_context_authority(tx, context)?;
    insert_review_context(tx, context.version, &digest, &canonical)?;
    if tx.execute(
        "UPDATE changes SET version = ?1, context_digest = ?2 WHERE repository = ?3 AND change_id = ?4 AND version = ?5",
        params![next, digest, context.change.repository, context.change.id, expected],
    )? != 1 {
        return Err(Error::Conflict(format!("change {} is missing or no longer at version {expected_version}", context.change.id)));
    }
    Ok(ChangeState {
        id: context.change.clone(),
        version: context.change_version,
        context_digest: digest,
    })
}

fn validate_context_authority(tx: &Transaction<'_>, context: &ReviewContext) -> Result<()> {
    ensure_repository(tx, &context.change.repository)?;
    let policy = read_policy(tx)?;
    if policy != sql_i64("policy epoch", context.policy_epoch)? {
        return Err(Error::Conflict(format!(
            "review context policy epoch {} is not current epoch {policy}",
            context.policy_epoch
        )));
    }
    if !base_matches_target(
        context.base.as_ref(),
        read_target(tx, &context.target)?.as_ref(),
    ) {
        return Err(Error::Conflict(format!(
            "review context base does not match target {}",
            context.target
        )));
    }
    Ok(())
}

fn validate_current_context(
    tx: &Transaction<'_>,
    change: &ChangeId,
    change_version: u64,
    digest: &str,
    target: &str,
    revision: &str,
    policy_epoch: i64,
) -> Result<ReviewContext> {
    let context = validate_current_context_without_workflow(
        tx,
        change,
        change_version,
        digest,
        target,
        revision,
        policy_epoch,
    )?;
    workflows::require_current_success(tx, &context)?;
    Ok(context)
}

pub(super) fn validate_current_context_without_workflow(
    tx: &Transaction<'_>,
    change: &ChangeId,
    change_version: u64,
    digest: &str,
    target: &str,
    revision: &str,
    policy_epoch: i64,
) -> Result<ReviewContext> {
    ensure_repository(tx, &change.repository)?;
    let current_policy = read_policy(tx)?;
    if current_policy != policy_epoch {
        return Err(Error::Conflict(format!(
            "request policy epoch {policy_epoch} is not current epoch {current_policy}"
        )));
    }
    let state =
        read_change(tx, change)?.ok_or_else(|| Error::NotFound(format!("change {}", change.id)))?;
    if state.version != change_version || state.context_digest != digest {
        return Err(Error::Conflict(format!(
            "change {} no longer matches version {change_version} and context {digest}",
            change.id
        )));
    }
    let context = read_review_context(tx, digest)?
        .ok_or_else(|| Error::NotFound(format!("review context {digest}")))?;
    if context.change != *change
        || context.change_version != change_version
        || context.target != target
        || context.revision != revision
        || sql_i64("context policy epoch", context.policy_epoch)? != policy_epoch
    {
        return Err(Error::Conflict("current review context does not bind the requested change, target, revision, and policy".to_owned()));
    }
    if !base_matches_target(context.base.as_ref(), read_target(tx, target)?.as_ref()) {
        return Err(Error::Conflict(format!(
            "current review context base no longer matches target {target}"
        )));
    }
    if let ContextReadiness::Incomplete { reasons } = context.readiness(SUPPORTED_CAPABILITIES) {
        return Err(Error::Invalid(format!(
            "review context is incomplete: {}",
            reasons.join("; ")
        )));
    }
    checks::require_current_success(tx, &context)?;
    #[cfg(unix)]
    crate::execution::require_current_command_success(tx, &context)?;
    Ok(context)
}

fn insert_review_context(
    tx: &Transaction<'_>,
    version: u32,
    digest: &str,
    canonical: &[u8],
) -> Result<()> {
    tx.execute("INSERT INTO review_contexts(digest, version, canonical_json) VALUES (?1, ?2, ?3) ON CONFLICT(digest) DO NOTHING", params![digest, i64::from(version), canonical])?;
    let stored: (i64, Vec<u8>) = tx.query_row(
        "SELECT version, canonical_json FROM review_contexts WHERE digest = ?1",
        [digest],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if stored.0 != i64::from(version) || stored.1 != canonical {
        return Err(Error::Conflict(format!(
            "review context digest {digest} is already bound to different bytes"
        )));
    }
    Ok(())
}

fn read_review_context(connection: &Connection, digest: &str) -> Result<Option<ReviewContext>> {
    connection
        .query_row(
            "SELECT version, canonical_json FROM review_contexts WHERE digest = ?1",
            [digest],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?
        .map(|(version, bytes)| {
            let context = ReviewContext::from_slice(&bytes)?;
            if i64::from(context.version) != version
                || context.canonical_bytes()? != bytes
                || context.digest()? != digest
            {
                return Err(Error::Invalid(format!(
                    "stored review context {digest} failed canonical integrity validation"
                )));
            }
            Ok(context)
        })
        .transpose()
}

fn read_change(connection: &Connection, id: &ChangeId) -> Result<Option<ChangeState>> {
    connection
        .query_row(
            "SELECT version, context_digest FROM changes WHERE repository = ?1 AND change_id = ?2",
            params![id.repository, id.id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|(version, context_digest)| {
            Ok(ChangeState {
                id: id.clone(),
                version: sql_u64("change version", version)?,
                context_digest,
            })
        })
        .transpose()
}

fn ensure_repository(connection: &Connection, repository: &str) -> Result<()> {
    let actual: String = connection.query_row(
        "SELECT repository_id FROM authority_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if actual != repository {
        return Err(Error::Conflict(format!(
            "repository {repository} does not match this authority"
        )));
    }
    Ok(())
}

fn base_matches_target(base: Option<&BaseState>, target: Option<&TargetState>) -> bool {
    match (base, target) {
        (None, None) => true,
        (Some(base), Some(target)) => {
            base.revision == target.revision && base.target_version == target.version
        }
        _ => false,
    }
}

fn require_change_binding<'a>(
    change: Option<&'a ChangeId>,
    version: Option<u64>,
    operation: &str,
) -> Result<(&'a ChangeId, u64)> {
    let change = change.ok_or_else(|| {
        Error::Invalid(format!(
            "{operation} requires a change identity and version"
        ))
    })?;
    change.validate()?;
    let version = version.ok_or_else(|| {
        Error::Invalid(format!(
            "{operation} requires a change identity and version"
        ))
    })?;
    if version == 0 {
        return Err(Error::Invalid("change version must be positive".to_owned()));
    }
    sql_i64("change version", version)?;
    Ok((change, version))
}

fn validate_integration_request(request: &IntegrateRequest) -> Result<()> {
    validate_identifier("target name", &request.target)?;
    validate_id(&request.revision)?;
    validate_digest("context digest", &request.context_digest)?;
    validate_identifier("approval ID", &request.approval_id)?;
    sql_i64("policy epoch", request.policy_epoch)?;
    if let Some(expected) = &request.expected_target {
        validate_id(&expected.revision)?;
        if expected.version == 0 {
            return Err(Error::Invalid(
                "expected target version must be positive".to_owned(),
            ));
        }
        sql_i64("expected target version", expected.version)?;
    }
    Ok(())
}

fn validate_identifier(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(Error::Limit(format!(
            "{kind} must contain 1 to {MAX_IDENTIFIER_BYTES} bytes"
        )));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
    }) {
        return Err(Error::Invalid(format!(
            "{kind} contains an unsupported character"
        )));
    }
    Ok(())
}

fn validate_digest(kind: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(format!(
            "{kind} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

fn sql_i64(kind: &str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Limit(format!("{kind} exceeds SQLite range")))
}
fn sql_u64(kind: &str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Invalid(format!("stored {kind} is negative ({value})")))
}
fn read_policy(connection: &Connection) -> Result<i64> {
    connection
        .query_row(
            "SELECT policy_epoch FROM authority_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(Error::from)
}

fn read_target(connection: &Connection, name: &str) -> Result<Option<TargetState>> {
    connection
        .query_row(
            "SELECT revision, version FROM targets WHERE name = ?1",
            [name],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        .map(|(revision, version)| {
            Ok(TargetState {
                revision,
                version: sql_u64("target version", version)?,
            })
        })
        .transpose()
}

fn validate_direct_parent(candidate: &Revision, current: Option<&TargetState>) -> Result<()> {
    match current {
        None if candidate.parents.is_empty() => Ok(()),
        None => Err(Error::Conflict(
            "an empty target accepts only a genesis revision".to_owned(),
        )),
        Some(state) if candidate.parents.first() == Some(&state.revision) => Ok(()),
        Some(_) => Err(Error::Conflict(
            "candidate revision is not the direct child of the current target".to_owned(),
        )),
    }
}

fn validate_approval(
    tx: &Transaction<'_>,
    request: &IntegrateRequest,
    request_policy: i64,
    change: &ChangeId,
    change_version: u64,
) -> Result<()> {
    let approval = tx.query_row(
        "SELECT target, revision, context_digest, policy_epoch, revoked, consumed_event, change_repository, change_id, change_version, semantics_version FROM approvals WHERE approval_id = ?1",
        [&request.approval_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, Option<i64>>(5)?, row.get::<_, Option<String>>(6)?, row.get::<_, Option<String>>(7)?, row.get::<_, Option<i64>>(8)?, row.get::<_, i64>(9)?)),
    ).optional()?.ok_or_else(|| Error::NotFound(format!("approval {}", request.approval_id)))?;
    if approval.0 != request.target
        || approval.1 != request.revision
        || approval.2 != request.context_digest
        || approval.3 != request_policy
        || approval.6.as_deref() != Some(change.repository.as_str())
        || approval.7.as_deref() != Some(change.id.as_str())
        || approval.8 != Some(sql_i64("change version", change_version)?)
        || approval.9 != CURRENT_SEMANTICS_VERSION
    {
        return Err(Error::Conflict(format!(
            "approval {} does not bind the requested repository, change, target, revision, context, and policy",
            request.approval_id
        )));
    }
    if approval.4 != 0 {
        return Err(Error::Conflict(format!(
            "approval {} is revoked",
            request.approval_id
        )));
    }
    if approval.5.is_some() {
        return Err(Error::Conflict(format!(
            "approval {} has already been consumed",
            request.approval_id
        )));
    }
    Ok(())
}

fn integration_request_digest(request: &IntegrateRequest) -> String {
    if request.change.is_none() && request.change_version.is_none() {
        return legacy_integration_request_digest(request);
    }
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.integration-request.v2\0");
    hash_request_prefix(&mut hasher, request);
    let change = request
        .change
        .as_ref()
        .expect("change binding checked by digest shape");
    hash_field(&mut hasher, change.repository.as_bytes());
    hash_field(&mut hasher, change.id.as_bytes());
    hasher.update(request.change_version.unwrap_or(0).to_be_bytes());
    hasher.update(request.policy_epoch.to_be_bytes());
    format!("{:x}", hasher.finalize())
}

fn initialization_request_digest(
    request: &IntegrateRequest,
    change: &ChangeId,
    change_version: u64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.initialize-request.v1\0");
    hash_request_prefix(&mut hasher, request);
    hash_field(&mut hasher, change.repository.as_bytes());
    hash_field(&mut hasher, change.id.as_bytes());
    hasher.update(change_version.to_be_bytes());
    hasher.update(request.policy_epoch.to_be_bytes());
    format!("{:x}", hasher.finalize())
}

fn initialization_retry(
    operation_id: &str,
    request_digest: &str,
    stored_digest: String,
    event: IntegrationEvent,
) -> Result<IntegrationEvent> {
    if event.kind != EventKind::Initialize {
        return Err(Error::Conflict(format!(
            "operation ID {operation_id} was already used for a different request"
        )));
    }
    matching_retry(operation_id, request_digest, stored_digest, event)
}

fn preparation_exists(connection: &Connection, operation_id: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM landing_preparations WHERE operation_id = ?1",
            [operation_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn preparation_request_digest(request: &PrepareLandingRequest, context_digest: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.prepare-landing-request.v1\0");
    hash_field(&mut hasher, request.operation_id.as_bytes());
    hash_field(&mut hasher, request.target.as_bytes());
    hash_field(&mut hasher, request.expected_target.revision.as_bytes());
    hasher.update(request.expected_target.version.to_be_bytes());
    hash_field(&mut hasher, request.source_tip.as_bytes());
    hasher.update(request.change_version.to_be_bytes());
    hash_field(&mut hasher, context_digest.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Returns the stored outcome for an identical retry; any other use of the operation ID conflicts.
fn lookup_preparation(
    connection: &Connection,
    request: &PrepareLandingRequest,
    request_digest: &str,
) -> Result<Option<LandingPreparation>> {
    let reused = || {
        Error::Conflict(format!(
            "operation ID {} was already used for a different request",
            request.operation_id
        ))
    };
    if connection
        .query_row(
            "SELECT 1 FROM operations WHERE operation_id = ?1",
            [&request.operation_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        return Err(reused());
    }
    let stored: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT request_digest, outcome_json FROM landing_preparations WHERE operation_id = ?1",
            [&request.operation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match stored {
        None => Ok(None),
        Some((digest, _)) if digest != request_digest => Err(reused()),
        Some((_, bytes)) => Ok(Some(serde_json::from_slice(&bytes)?)),
    }
}

fn legacy_integration_request_digest(request: &IntegrateRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.integration-request.v1\0");
    hash_request_prefix(&mut hasher, request);
    hasher.update(request.policy_epoch.to_be_bytes());
    format!("{:x}", hasher.finalize())
}

fn hash_request_prefix(hasher: &mut Sha256, request: &IntegrateRequest) {
    hash_field(hasher, request.operation_id.as_bytes());
    hash_field(hasher, request.target.as_bytes());
    match &request.expected_target {
        Some(expected) => {
            hasher.update([1]);
            hash_field(hasher, expected.revision.as_bytes());
            hasher.update(expected.version.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    hash_field(hasher, request.revision.as_bytes());
    hash_field(hasher, request.context_digest.as_bytes());
    hash_field(hasher, request.approval_id.as_bytes());
}

fn hash_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u64).to_be_bytes());
    hasher.update(field);
}

fn matching_retry(
    operation_id: &str,
    request_digest: &str,
    stored_digest: String,
    event: IntegrationEvent,
) -> Result<IntegrationEvent> {
    if stored_digest == request_digest {
        Ok(event)
    } else {
        Err(Error::Conflict(format!(
            "operation ID {operation_id} was already used for a different request"
        )))
    }
}

fn lookup_operation(
    connection: &Connection,
    operation_id: &str,
) -> Result<Option<(String, IntegrationEvent)>> {
    if preparation_exists(connection, operation_id)? {
        return Err(Error::Conflict(format!(
            "operation ID {operation_id} was already used for a different request"
        )));
    }
    connection.query_row(
        "SELECT o.request_digest, e.sequence, e.operation_id, e.target, e.before_revision, e.before_version, e.after_revision, e.after_version, e.approval_id, e.context_digest, e.policy_epoch, e.change_repository, e.change_id, e.change_version, e.semantics_version, e.kind FROM operations o JOIN integration_events e ON e.sequence = o.event_sequence WHERE o.operation_id = ?1",
        [operation_id],
        |row| Ok((row.get::<_, String>(0)?, RawEvent { sequence: row.get(1)?, operation_id: row.get(2)?, target: row.get(3)?, before_revision: row.get(4)?, before_version: row.get(5)?, after_revision: row.get(6)?, after_version: row.get(7)?, approval_id: row.get(8)?, context_digest: row.get(9)?, policy_epoch: row.get(10)?, change_repository: row.get(11)?, change_id: row.get(12)?, change_version: row.get(13)?, semantics_version: row.get(14)?, kind: row.get(15)? })),
    ).optional()?.map(|(digest, raw)| Ok((digest, raw.into_event()?))).transpose()
}

struct RawEvent {
    sequence: i64,
    operation_id: String,
    target: String,
    before_revision: Option<String>,
    before_version: Option<i64>,
    after_revision: String,
    after_version: i64,
    approval_id: String,
    context_digest: String,
    policy_epoch: i64,
    change_repository: Option<String>,
    change_id: Option<String>,
    change_version: Option<i64>,
    semantics_version: i64,
    kind: String,
}

impl RawEvent {
    fn into_event(self) -> Result<IntegrationEvent> {
        let before = match (self.before_revision, self.before_version) {
            (Some(revision), Some(version)) => Some(TargetState {
                revision,
                version: sql_u64("target version", version)?,
            }),
            (None, None) => None,
            _ => {
                return Err(Error::Invalid(
                    "stored integration event has a partial before state".to_owned(),
                ));
            }
        };
        let change = match (self.change_repository, self.change_id) {
            (Some(repository), Some(id)) => Some(ChangeId { repository, id }),
            (None, None) => None,
            _ => {
                return Err(Error::Invalid(
                    "stored integration event has a partial change identity".to_owned(),
                ));
            }
        };
        if change.is_some() != self.change_version.is_some() {
            return Err(Error::Invalid(
                "stored integration event has a partial change binding".to_owned(),
            ));
        }
        let kind = match self.kind.as_str() {
            "integrate" => EventKind::Integrate,
            "initialize" => EventKind::Initialize,
            other => {
                return Err(Error::Invalid(format!(
                    "stored integration event has unknown kind {other}"
                )));
            }
        };
        Ok(IntegrationEvent {
            kind,
            semantics_version: u32::try_from(self.semantics_version).map_err(|_| {
                Error::Invalid("stored semantics version is outside the u32 range".to_owned())
            })?,
            sequence: sql_u64("event sequence", self.sequence)?,
            operation_id: self.operation_id,
            target: self.target,
            before,
            after: TargetState {
                revision: self.after_revision,
                version: sql_u64("target version", self.after_version)?,
            },
            approval_id: self.approval_id,
            context_digest: self.context_digest,
            change,
            change_version: self
                .change_version
                .map(|value| sql_u64("change version", value))
                .transpose()?,
            policy_epoch: sql_u64("policy epoch", self.policy_epoch)?,
        })
    }
}

fn read_raw_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEvent> {
    Ok(RawEvent {
        sequence: row.get(0)?,
        operation_id: row.get(1)?,
        target: row.get(2)?,
        before_revision: row.get(3)?,
        before_version: row.get(4)?,
        after_revision: row.get(5)?,
        after_version: row.get(6)?,
        approval_id: row.get(7)?,
        context_digest: row.get(8)?,
        policy_epoch: row.get(9)?,
        change_repository: row.get(10)?,
        change_id: row.get(11)?,
        change_version: row.get(12)?,
        semantics_version: row.get(13)?,
        kind: row.get(14)?,
    })
}
