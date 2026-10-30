use std::collections::HashSet;
use std::time::Instant;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::context::{ChangeId, InputIdentity, ReviewContext};
use crate::store::Engine;
use crate::{Error, Result};

pub const SOURCE_PATHS_CAPABILITY: &str = "agentscm.checks.source-paths/v1";
pub const SOURCE_PATHS_EVIDENCE: &str = "source_paths/v1";

const CHECK_VERSION: u32 = 1;
const RECIPE_VERSION: u32 = 1;
const CHECK_KIND: &str = "source_paths/v1";
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_EXPECTATIONS: usize = 256;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MIN_OUTPUT_BYTES: u64 = 256;
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
const MAX_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
const MAX_REASON_BYTES: usize = 2 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckLimits {
    pub max_paths: u32,
    pub max_total_bytes: u64,
    pub max_output_bytes: u64,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourcePathExpectation {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourcePathsRecipe {
    pub version: u32,
    pub expectations: Vec<SourcePathExpectation>,
    pub limits: CheckLimits,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourcePathsRequest {
    pub attempt_id: String,
    pub change: ChangeId,
    pub change_version: u64,
    pub context_digest: String,
    pub revision: String,
    pub recipe: SourcePathsRecipe,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckSubject {
    pub repository: String,
    pub change: ChangeId,
    pub change_version: u64,
    pub context_digest: String,
    pub revision: String,
    pub recipe_sha256: String,
    pub toolchain_sha256: String,
    pub environment_sha256: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Pending,
    Running,
    Success,
    Fail,
    Incomplete,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckAttempt {
    pub version: u32,
    pub attempt_id: String,
    pub request_digest: String,
    pub subject: CheckSubject,
    pub check_kind: String,
    pub limits: CheckLimits,
    pub state: CheckState,
    pub run_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_digest: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PathObservation {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckEvidence {
    pub version: u32,
    pub attempt_id: String,
    pub request_digest: String,
    pub subject: CheckSubject,
    pub check_kind: String,
    pub limits: CheckLimits,
    pub state: CheckState,
    pub run_generation: u64,
    pub execution_basis: String,
    pub observations: Vec<PathObservation>,
    pub output_sha256: String,
    pub output_bytes: u64,
    pub reason: String,
}

#[derive(Serialize)]
struct CanonicalRecipe<'a> {
    version: u32,
    expectations: &'a [SourcePathExpectation],
    limits: &'a CheckLimits,
}

#[derive(Serialize)]
struct CanonicalRequest<'a> {
    attempt_id: &'a str,
    subject: &'a CheckSubject,
    check_kind: &'a str,
    recipe: &'a SourcePathsRecipe,
}

impl SourcePathsRecipe {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let canonical = self.canonicalized();
        Ok(serde_json::to_vec(&CanonicalRecipe {
            version: canonical.version,
            expectations: &canonical.expectations,
            limits: &canonical.limits,
        })?)
    }

    pub fn digest(&self) -> Result<String> {
        Ok(sha256(&self.canonical_bytes()?))
    }

    fn validate(&self) -> Result<()> {
        if self.version != RECIPE_VERSION {
            return Err(Error::Invalid(format!(
                "unsupported source-paths recipe version {}",
                self.version
            )));
        }
        if self.expectations.is_empty() {
            return Err(Error::Invalid(
                "source-paths recipe has no expectations".to_owned(),
            ));
        }
        if self.expectations.len() > MAX_EXPECTATIONS
            || self.expectations.len() > self.limits.max_paths as usize
        {
            return Err(Error::Limit(format!(
                "source-paths recipe exceeds its path limit of {}",
                self.limits.max_paths
            )));
        }
        self.limits.validate()?;
        let mut paths = HashSet::new();
        for expectation in &self.expectations {
            validate_path(&expectation.path)?;
            validate_digest("expected file digest", &expectation.sha256)?;
            if !paths.insert(expectation.path.as_str()) {
                return Err(Error::Invalid(format!(
                    "duplicate source-paths expectation: {}",
                    expectation.path
                )));
            }
        }
        Ok(())
    }

    fn canonicalized(&self) -> Self {
        let mut canonical = self.clone();
        canonical
            .expectations
            .sort_by(|left, right| left.path.cmp(&right.path));
        canonical
    }
}

impl CheckLimits {
    fn validate(&self) -> Result<()> {
        if self.max_paths == 0 || self.max_paths as usize > MAX_EXPECTATIONS {
            return Err(Error::Limit(format!(
                "check max_paths must be between 1 and {MAX_EXPECTATIONS}"
            )));
        }
        if self.max_total_bytes == 0 || self.max_total_bytes > MAX_TOTAL_BYTES {
            return Err(Error::Limit(format!(
                "check max_total_bytes must be between 1 and {MAX_TOTAL_BYTES}"
            )));
        }
        if self.max_output_bytes < MIN_OUTPUT_BYTES || self.max_output_bytes > MAX_OUTPUT_BYTES {
            return Err(Error::Limit(format!(
                "check max_output_bytes must be between {MIN_OUTPUT_BYTES} and {MAX_OUTPUT_BYTES}"
            )));
        }
        if self.timeout_ms == 0 || self.timeout_ms > MAX_TIMEOUT_MS {
            return Err(Error::Limit(format!(
                "check timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"
            )));
        }
        Ok(())
    }
}

impl CheckEvidence {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    pub fn digest(&self) -> Result<String> {
        Ok(sha256(&self.canonical_bytes()?))
    }

    fn validate(&self) -> Result<()> {
        if self.version != CHECK_VERSION {
            return Err(Error::Invalid(format!(
                "unsupported check evidence version {}",
                self.version
            )));
        }
        validate_identifier("attempt ID", &self.attempt_id)?;
        validate_digest("request digest", &self.request_digest)?;
        validate_subject(&self.subject)?;
        if self.check_kind != CHECK_KIND {
            return Err(Error::Invalid(format!(
                "unsupported check kind {}",
                self.check_kind
            )));
        }
        self.limits.validate()?;
        if !self.state.is_terminal() {
            return Err(Error::Invalid(
                "check evidence must have a terminal state".to_owned(),
            ));
        }
        if !matches!(
            self.execution_basis.as_str(),
            "controller_builtin" | "controller_control"
        ) {
            return Err(Error::Invalid(format!(
                "unsupported execution basis {}",
                self.execution_basis
            )));
        }
        validate_reason(&self.reason)?;
        if self.output_bytes != self.reason.len() as u64
            || self.output_bytes > self.limits.max_output_bytes
            || self.output_sha256 != sha256(self.reason.as_bytes())
        {
            return Err(Error::Invalid(
                "check evidence output metadata does not match its reason".to_owned(),
            ));
        }
        if self.observations.len() > self.limits.max_paths as usize {
            return Err(Error::Limit(
                "check evidence exceeds its path observation limit".to_owned(),
            ));
        }
        let mut paths = HashSet::new();
        let mut total_bytes = 0u64;
        for observation in &self.observations {
            validate_path(&observation.path)?;
            validate_digest("observed file digest", &observation.sha256)?;
            if !paths.insert(observation.path.as_str()) {
                return Err(Error::Invalid(format!(
                    "duplicate path observation: {}",
                    observation.path
                )));
            }
            total_bytes = total_bytes
                .checked_add(observation.bytes)
                .ok_or_else(|| Error::Limit("observation byte count overflowed".to_owned()))?;
            if total_bytes > self.limits.max_total_bytes {
                return Err(Error::Limit(
                    "check evidence exceeds its byte observation limit".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

impl CheckState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Success => "success",
            Self::Fail => "fail",
            Self::Incomplete => "incomplete",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "success" => Ok(Self::Success),
            "fail" => Ok(Self::Fail),
            "incomplete" => Ok(Self::Incomplete),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(Error::Invalid(format!("unknown check state {value}"))),
        }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Success | Self::Fail | Self::Incomplete | Self::Cancelled
        )
    }
}

pub(crate) fn create_schema_v3(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE check_attempts (
            attempt_id TEXT PRIMARY KEY,
            request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
            repository TEXT NOT NULL,
            change_id TEXT NOT NULL,
            change_version INTEGER NOT NULL CHECK (change_version > 0),
            context_digest TEXT NOT NULL CHECK (length(context_digest) = 64),
            revision TEXT NOT NULL CHECK (length(revision) = 64),
            recipe_sha256 TEXT NOT NULL CHECK (length(recipe_sha256) = 64),
            toolchain_sha256 TEXT NOT NULL CHECK (length(toolchain_sha256) = 64),
            environment_sha256 TEXT NOT NULL CHECK (length(environment_sha256) = 64),
            check_kind TEXT NOT NULL CHECK (check_kind = 'source_paths/v1'),
            recipe_json BLOB NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('pending', 'running', 'success', 'fail', 'incomplete', 'cancelled')),
            run_generation INTEGER NOT NULL CHECK (run_generation >= 0),
            evidence_digest TEXT UNIQUE CHECK (evidence_digest IS NULL OR length(evidence_digest) = 64),
            CHECK (length(attempt_id) BETWEEN 1 AND 128),
            CHECK ((state IN ('pending', 'running')) = (evidence_digest IS NULL))
        ) STRICT;
        CREATE TABLE check_evidence (
            digest TEXT PRIMARY KEY CHECK (length(digest) = 64),
            attempt_id TEXT NOT NULL UNIQUE,
            canonical_json BLOB NOT NULL,
            FOREIGN KEY (attempt_id) REFERENCES check_attempts(attempt_id)
        ) STRICT;
        CREATE INDEX check_success_subject ON check_attempts(
            repository, change_id, change_version, context_digest, revision,
            recipe_sha256, toolchain_sha256, environment_sha256, state
        );",
    )?;
    Ok(())
}

pub(crate) fn create_source_paths_attempt(
    connection: &mut Connection,
    request: &SourcePathsRequest,
) -> Result<CheckAttempt> {
    validate_identifier("attempt ID", &request.attempt_id)?;
    request.change.validate()?;
    validate_positive_sql_integer("change version", request.change_version)?;
    validate_digest("context digest", &request.context_digest)?;
    validate_digest("revision", &request.revision)?;
    let recipe_bytes = request.recipe.canonical_bytes()?;
    let recipe = SourcePathsRecipe::from_canonical(&recipe_bytes)?;
    let recipe_digest = sha256(&recipe_bytes);

    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure_repository(&tx, &request.change.repository)?;
    if let Some((attempt, stored_recipe)) = read_attempt_with_recipe(&tx, &request.attempt_id)? {
        if attempt.subject.change != request.change
            || attempt.subject.change_version != request.change_version
            || attempt.subject.context_digest != request.context_digest
            || attempt.subject.revision != request.revision
            || attempt.subject.recipe_sha256 != recipe_digest
            || stored_recipe != recipe
        {
            return Err(Error::Conflict(format!(
                "check attempt ID {} is already bound to different input",
                request.attempt_id
            )));
        }
        tx.commit()?;
        return Ok(attempt);
    }
    let context = load_current_context(
        &tx,
        &request.change,
        request.change_version,
        &request.context_digest,
        &request.revision,
    )?;
    let subject = subject_from_context(&context, recipe_digest)?;
    let request_digest = request_digest(&request.attempt_id, &subject, &recipe)?;
    let change_version = sql_i64("change version", subject.change_version)?;
    tx.execute(
        "INSERT INTO check_attempts(
            attempt_id, request_digest, repository, change_id, change_version,
            context_digest, revision, recipe_sha256, toolchain_sha256,
            environment_sha256, check_kind, recipe_json, state, run_generation
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'pending', 0)",
        params![
            request.attempt_id,
            request_digest,
            subject.repository,
            subject.change.id,
            change_version,
            subject.context_digest,
            subject.revision,
            subject.recipe_sha256,
            subject.toolchain_sha256,
            subject.environment_sha256,
            CHECK_KIND,
            recipe_bytes,
        ],
    )?;
    let attempt = read_attempt_with_recipe(&tx, &request.attempt_id)?
        .expect("inserted check attempt")
        .0;
    tx.commit()?;
    Ok(attempt)
}

pub(crate) fn run_source_paths_attempt(
    connection: &mut Connection,
    engine: &Engine,
    attempt_id: &str,
    expected_generation: u64,
) -> Result<CheckAttempt> {
    validate_identifier("attempt ID", attempt_id)?;
    let expected = sql_i64("run generation", expected_generation)?;
    let (running, recipe) = {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (attempt, recipe) = read_attempt_with_recipe(&tx, attempt_id)?
            .ok_or_else(|| Error::NotFound(format!("check attempt {attempt_id}")))?;
        if attempt.state.is_terminal() {
            tx.commit()?;
            return Ok(attempt);
        }
        if attempt.state == CheckState::Running {
            return Err(Error::Conflict(format!(
                "check attempt {attempt_id} is already running"
            )));
        }
        if attempt.run_generation != expected_generation {
            return Err(Error::Conflict(format!(
                "check attempt {attempt_id} is no longer at run generation {expected_generation}"
            )));
        }
        validate_current_subject(&tx, &attempt.subject, &recipe)?;
        let next = expected
            .checked_add(1)
            .ok_or_else(|| Error::Limit("run generation exhausted".to_owned()))?;
        if tx.execute(
            "UPDATE check_attempts SET state = 'running', run_generation = ?1
             WHERE attempt_id = ?2 AND state = 'pending' AND run_generation = ?3",
            params![next, attempt_id, expected],
        )? != 1
        {
            return Err(Error::Conflict(format!(
                "check attempt {attempt_id} changed before it could run"
            )));
        }
        let running = read_attempt_with_recipe(&tx, attempt_id)?
            .expect("running check attempt")
            .0;
        tx.commit()?;
        (running, recipe)
    };

    let result = execute_source_paths(engine, &running, &recipe);
    finish_attempt(connection, &running, result)
}

pub(crate) fn cancel_attempt(
    connection: &mut Connection,
    attempt_id: &str,
    expected_generation: u64,
    reason: &str,
) -> Result<CheckAttempt> {
    terminal_transition(
        connection,
        attempt_id,
        expected_generation,
        reason,
        CheckState::Cancelled,
        &[CheckState::Pending, CheckState::Running],
    )
}

pub(crate) fn recover_attempt(
    connection: &mut Connection,
    attempt_id: &str,
    expected_generation: u64,
    reason: &str,
) -> Result<CheckAttempt> {
    terminal_transition(
        connection,
        attempt_id,
        expected_generation,
        reason,
        CheckState::Incomplete,
        &[CheckState::Running],
    )
}

pub(crate) fn read_attempt(connection: &Connection, attempt_id: &str) -> Result<CheckAttempt> {
    validate_identifier("attempt ID", attempt_id)?;
    read_attempt_with_recipe(connection, attempt_id)?
        .map(|value| value.0)
        .ok_or_else(|| Error::NotFound(format!("check attempt {attempt_id}")))
}

pub(crate) fn read_evidence(connection: &Connection, digest: &str) -> Result<CheckEvidence> {
    validate_digest("evidence digest", digest)?;
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT canonical_json FROM check_evidence WHERE digest = ?1",
            [digest],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("check evidence {digest}")))?;
    let evidence: CheckEvidence = serde_json::from_slice(&bytes)?;
    if evidence.canonical_bytes()? != bytes || evidence.digest()? != digest {
        return Err(Error::Invalid(format!(
            "stored check evidence {digest} failed canonical integrity validation"
        )));
    }
    Ok(evidence)
}

pub(crate) fn require_current_success(
    connection: &Connection,
    context: &ReviewContext,
) -> Result<()> {
    let Some((recipe, toolchain, environment)) =
        required_binding(context, SOURCE_PATHS_CAPABILITY, SOURCE_PATHS_EVIDENCE)?
    else {
        return Ok(());
    };
    let successful: Option<(String, String)> = connection
        .query_row(
            "SELECT attempt_id, evidence_digest FROM check_attempts
             WHERE repository = ?1 AND change_id = ?2 AND change_version = ?3
               AND context_digest = ?4 AND revision = ?5 AND recipe_sha256 = ?6
               AND toolchain_sha256 = ?7 AND environment_sha256 = ?8
               AND check_kind = ?9 AND state = 'success' AND evidence_digest IS NOT NULL
             ORDER BY attempt_id LIMIT 1",
            params![
                context.change.repository,
                context.change.id,
                sql_i64("change version", context.change_version)?,
                context.digest()?,
                context.revision,
                recipe,
                toolchain,
                environment,
                CHECK_KIND,
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (attempt_id, digest) = successful.ok_or_else(|| {
        Error::Invalid(format!(
            "required evidence {SOURCE_PATHS_EVIDENCE} has no exact successful controller attempt"
        ))
    })?;
    let attempt = read_attempt_with_recipe(connection, &attempt_id)?
        .expect("successful attempt selected from check_attempts")
        .0;
    let evidence = read_evidence(connection, &digest)?;
    if evidence.state != CheckState::Success
        || evidence.execution_basis != "controller_builtin"
        || evidence.attempt_id != attempt.attempt_id
        || evidence.request_digest != attempt.request_digest
        || evidence.run_generation != attempt.run_generation
        || evidence.limits != attempt.limits
        || attempt.evidence_digest.as_deref() != Some(digest.as_str())
        || evidence.subject.change != context.change
        || evidence.subject.change_version != context.change_version
        || evidence.subject.context_digest != context.digest()?
        || evidence.subject.revision != context.revision
        || evidence.subject.recipe_sha256 != recipe
        || evidence.subject.toolchain_sha256 != toolchain
        || evidence.subject.environment_sha256 != environment
    {
        return Err(Error::Invalid(
            "stored source-paths evidence does not match its successful attempt".to_owned(),
        ));
    }
    Ok(())
}

fn execute_source_paths(
    engine: &Engine,
    attempt: &CheckAttempt,
    recipe: &SourcePathsRecipe,
) -> ExecutionResult {
    let started = Instant::now();
    let mut total_bytes = 0u64;
    let mut observations = Vec::with_capacity(recipe.expectations.len());
    for expectation in &recipe.expectations {
        if elapsed_ms(started) >= recipe.limits.timeout_ms {
            return ExecutionResult::terminal(
                CheckState::Incomplete,
                observations,
                "source-paths check exceeded its wall-time limit".to_owned(),
            );
        }
        let bytes = match engine.read_entry(&attempt.subject.revision, &expectation.path) {
            // The recipe binds bytes only, not kind or mode.
            Ok((kind, bytes)) => match crate::store::require_regular_entry(
                "source-paths check",
                &expectation.path,
                kind,
            ) {
                Ok(()) => bytes,
                Err(error) => {
                    return ExecutionResult::terminal(
                        CheckState::Incomplete,
                        observations,
                        error.to_string(),
                    );
                }
            },
            Err(Error::NotFound(message)) if message == format!("path: {}", expectation.path) => {
                return ExecutionResult::terminal(
                    CheckState::Fail,
                    observations,
                    format!("required path is missing: {}", expectation.path),
                );
            }
            Err(error) => {
                return ExecutionResult::terminal(
                    CheckState::Incomplete,
                    observations,
                    format!("could not read {}: {error}", expectation.path),
                );
            }
        };
        let bytes_len = bytes.len() as u64;
        total_bytes = match total_bytes.checked_add(bytes_len) {
            Some(total) if total <= recipe.limits.max_total_bytes => total,
            _ => {
                return ExecutionResult::terminal(
                    CheckState::Incomplete,
                    observations,
                    "source-paths check exceeded its byte limit".to_owned(),
                );
            }
        };
        let observed = sha256(&bytes);
        observations.push(PathObservation {
            path: expectation.path.clone(),
            sha256: observed.clone(),
            bytes: bytes_len,
        });
        if observed != expectation.sha256 {
            return ExecutionResult::terminal(
                CheckState::Fail,
                observations,
                format!("content digest does not match for {}", expectation.path),
            );
        }
    }
    if elapsed_ms(started) >= recipe.limits.timeout_ms {
        return ExecutionResult::terminal(
            CheckState::Incomplete,
            observations,
            "source-paths check exceeded its wall-time limit".to_owned(),
        );
    }
    ExecutionResult::terminal(
        CheckState::Success,
        observations,
        format!(
            "{} source path expectations matched",
            recipe.expectations.len()
        ),
    )
}

#[derive(Debug)]
struct ExecutionResult {
    state: CheckState,
    observations: Vec<PathObservation>,
    reason: String,
}

impl ExecutionResult {
    fn terminal(state: CheckState, observations: Vec<PathObservation>, reason: String) -> Self {
        Self {
            state,
            observations,
            reason,
        }
    }
}

fn finish_attempt(
    connection: &mut Connection,
    running: &CheckAttempt,
    result: ExecutionResult,
) -> Result<CheckAttempt> {
    let mut reason = result.reason;
    if reason.len() as u64 > running.limits.max_output_bytes {
        reason = "source-paths check output exceeded its byte limit".to_owned();
    }
    reason = truncate_utf8(reason, MAX_REASON_BYTES);
    let next_generation = running
        .run_generation
        .checked_add(1)
        .ok_or_else(|| Error::Limit("run generation exhausted".to_owned()))?;
    let evidence = evidence_for(
        running,
        result.state,
        next_generation,
        "controller_builtin",
        result.observations,
        reason,
    )?;
    store_terminal_evidence(connection, running, CheckState::Running, evidence)
}

fn terminal_transition(
    connection: &mut Connection,
    attempt_id: &str,
    expected_generation: u64,
    reason: &str,
    terminal_state: CheckState,
    allowed: &[CheckState],
) -> Result<CheckAttempt> {
    validate_identifier("attempt ID", attempt_id)?;
    validate_reason(reason)?;
    let attempt = read_attempt(connection, attempt_id)?;
    if reason.len() as u64 > attempt.limits.max_output_bytes {
        return Err(Error::Limit(format!(
            "check terminal reason exceeds {} bytes",
            attempt.limits.max_output_bytes
        )));
    }
    if attempt.state.is_terminal() {
        let expected_terminal_generation = expected_generation
            .checked_add(1)
            .ok_or_else(|| Error::Limit("run generation exhausted".to_owned()))?;
        let evidence = attempt
            .evidence_digest
            .as_deref()
            .ok_or_else(|| Error::Invalid("terminal check has no evidence digest".to_owned()))
            .and_then(|digest| read_evidence(connection, digest))?;
        if attempt.state == terminal_state
            && attempt.run_generation == expected_terminal_generation
            && evidence.execution_basis == "controller_control"
            && evidence.reason == reason
        {
            return Ok(attempt);
        }
        return Err(Error::Conflict(format!(
            "check attempt {attempt_id} already has a different terminal outcome"
        )));
    }
    if attempt.run_generation != expected_generation || !allowed.contains(&attempt.state) {
        return Err(Error::Conflict(format!(
            "check attempt {attempt_id} cannot move from {} at run generation {}",
            attempt.state.as_str(),
            attempt.run_generation
        )));
    }
    let next_generation = expected_generation
        .checked_add(1)
        .ok_or_else(|| Error::Limit("run generation exhausted".to_owned()))?;
    let evidence = evidence_for(
        &attempt,
        terminal_state,
        next_generation,
        "controller_control",
        Vec::new(),
        reason.to_owned(),
    )?;
    store_terminal_evidence(connection, &attempt, attempt.state, evidence)
}

fn evidence_for(
    attempt: &CheckAttempt,
    state: CheckState,
    run_generation: u64,
    execution_basis: &str,
    observations: Vec<PathObservation>,
    reason: String,
) -> Result<CheckEvidence> {
    if !state.is_terminal() {
        return Err(Error::Invalid(
            "cannot create evidence for a nonterminal check".to_owned(),
        ));
    }
    let output_bytes = reason.len() as u64;
    Ok(CheckEvidence {
        version: CHECK_VERSION,
        attempt_id: attempt.attempt_id.clone(),
        request_digest: attempt.request_digest.clone(),
        subject: attempt.subject.clone(),
        check_kind: attempt.check_kind.clone(),
        limits: attempt.limits.clone(),
        state,
        run_generation,
        execution_basis: execution_basis.to_owned(),
        observations,
        output_sha256: sha256(reason.as_bytes()),
        output_bytes,
        reason,
    })
}

fn store_terminal_evidence(
    connection: &mut Connection,
    attempt: &CheckAttempt,
    expected_state: CheckState,
    evidence: CheckEvidence,
) -> Result<CheckAttempt> {
    let digest = evidence.digest()?;
    let canonical = evidence.canonical_bytes()?;
    let expected_generation = sql_i64("run generation", attempt.run_generation)?;
    let next_generation = sql_i64("run generation", evidence.run_generation)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE check_attempts SET state = ?1, run_generation = ?2, evidence_digest = ?3
         WHERE attempt_id = ?4 AND state = ?5 AND run_generation = ?6 AND evidence_digest IS NULL",
        params![
            evidence.state.as_str(),
            next_generation,
            digest,
            attempt.attempt_id,
            expected_state.as_str(),
            expected_generation,
        ],
    )?;
    if changed != 1 {
        let current = read_attempt_with_recipe(&tx, &attempt.attempt_id)?
            .ok_or_else(|| Error::NotFound(format!("check attempt {}", attempt.attempt_id)))?
            .0;
        tx.commit()?;
        if current.state.is_terminal() {
            return Ok(current);
        }
        return Err(Error::Conflict(format!(
            "check attempt {} changed before terminal evidence was stored",
            attempt.attempt_id
        )));
    }
    tx.execute(
        "INSERT INTO check_evidence(digest, attempt_id, canonical_json) VALUES (?1, ?2, ?3)",
        params![digest, attempt.attempt_id, canonical],
    )?;
    let current = read_attempt_with_recipe(&tx, &attempt.attempt_id)?
        .expect("terminal check attempt")
        .0;
    tx.commit()?;
    Ok(current)
}

fn read_attempt_with_recipe(
    connection: &Connection,
    attempt_id: &str,
) -> Result<Option<(CheckAttempt, SourcePathsRecipe)>> {
    connection
        .query_row(
            "SELECT request_digest, repository, change_id, change_version,
                    context_digest, revision, recipe_sha256, toolchain_sha256,
                    environment_sha256, check_kind, recipe_json, state,
                    run_generation, evidence_digest
             FROM check_attempts WHERE attempt_id = ?1",
            [attempt_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, Vec<u8>>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, Option<String>>(13)?,
                ))
            },
        )
        .optional()?
        .map(
            |(
                request_digest,
                repository,
                change_id,
                change_version,
                context_digest,
                revision,
                recipe_sha256,
                toolchain_sha256,
                environment_sha256,
                check_kind,
                recipe_bytes,
                state,
                run_generation,
                evidence_digest,
            )| {
                let recipe = SourcePathsRecipe::from_canonical(&recipe_bytes)?;
                if sha256(&recipe_bytes) != recipe_sha256 {
                    return Err(Error::Invalid(format!(
                        "stored check attempt {attempt_id} failed recipe integrity validation"
                    )));
                }
                let attempt = CheckAttempt {
                    version: CHECK_VERSION,
                    attempt_id: attempt_id.to_owned(),
                    request_digest,
                    subject: CheckSubject {
                        repository: repository.clone(),
                        change: ChangeId {
                            repository,
                            id: change_id,
                        },
                        change_version: sql_u64("change version", change_version)?,
                        context_digest,
                        revision,
                        recipe_sha256,
                        toolchain_sha256,
                        environment_sha256,
                    },
                    check_kind,
                    limits: recipe.limits.clone(),
                    state: CheckState::parse(&state)?,
                    run_generation: sql_u64("run generation", run_generation)?,
                    evidence_digest,
                };
                validate_subject(&attempt.subject)?;
                if attempt.subject.repository != attempt.subject.change.repository
                    || attempt.check_kind != CHECK_KIND
                    || attempt.state.is_terminal() != attempt.evidence_digest.is_some()
                    || request_digest_for_attempt(&attempt, &recipe)? != attempt.request_digest
                {
                    return Err(Error::Invalid(format!(
                        "stored check attempt {attempt_id} failed canonical integrity validation"
                    )));
                }
                Ok((attempt, recipe))
            },
        )
        .transpose()
}

impl SourcePathsRecipe {
    fn from_canonical(bytes: &[u8]) -> Result<Self> {
        let recipe: Self = serde_json::from_slice(bytes)?;
        if recipe.canonical_bytes()? != bytes {
            return Err(Error::Invalid(
                "source-paths recipe is not canonical".to_owned(),
            ));
        }
        Ok(recipe)
    }
}

pub(crate) fn load_current_context(
    connection: &Connection,
    change: &ChangeId,
    change_version: u64,
    context_digest: &str,
    revision: &str,
) -> Result<ReviewContext> {
    ensure_repository(connection, &change.repository)?;
    let state: Option<(i64, String)> = connection
        .query_row(
            "SELECT version, context_digest FROM changes WHERE repository = ?1 AND change_id = ?2",
            params![change.repository, change.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (stored_version, stored_digest) =
        state.ok_or_else(|| Error::NotFound(format!("change {}", change.id)))?;
    if sql_u64("change version", stored_version)? != change_version
        || stored_digest != context_digest
    {
        return Err(Error::Conflict(format!(
            "change {} no longer matches version {change_version} and context {context_digest}",
            change.id
        )));
    }
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT canonical_json FROM review_contexts WHERE digest = ?1",
            [context_digest],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("review context {context_digest}")))?;
    let context = ReviewContext::from_slice(&bytes)?;
    if context.digest()? != context_digest
        || context.change != *change
        || context.change_version != change_version
        || context.revision != revision
    {
        return Err(Error::Conflict(
            "current review context does not bind the requested check subject".to_owned(),
        ));
    }
    Ok(context)
}

pub(crate) fn ensure_repository(connection: &Connection, requested: &str) -> Result<()> {
    let repository: String = connection.query_row(
        "SELECT repository_id FROM authority_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if repository != requested {
        return Err(Error::Conflict(format!(
            "repository {} does not match this authority",
            requested
        )));
    }
    Ok(())
}

fn subject_from_context(context: &ReviewContext, recipe_digest: String) -> Result<CheckSubject> {
    let Some((required_recipe, toolchain, environment)) =
        required_binding(context, SOURCE_PATHS_CAPABILITY, SOURCE_PATHS_EVIDENCE)?
    else {
        return Err(Error::Invalid(format!(
            "review context does not request capability {SOURCE_PATHS_CAPABILITY}"
        )));
    };
    if recipe_digest != required_recipe {
        return Err(Error::Conflict(
            "source-paths recipe does not match the current review context".to_owned(),
        ));
    }
    Ok(CheckSubject {
        repository: context.change.repository.clone(),
        change: context.change.clone(),
        change_version: context.change_version,
        context_digest: context.digest()?,
        revision: context.revision.clone(),
        recipe_sha256: required_recipe,
        toolchain_sha256: toolchain,
        environment_sha256: environment,
    })
}

fn validate_current_subject(
    connection: &Connection,
    subject: &CheckSubject,
    recipe: &SourcePathsRecipe,
) -> Result<()> {
    let context = load_current_context(
        connection,
        &subject.change,
        subject.change_version,
        &subject.context_digest,
        &subject.revision,
    )?;
    let current = subject_from_context(&context, recipe.digest()?)?;
    if &current != subject {
        return Err(Error::Conflict(
            "check subject no longer matches the current review context".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn required_binding(
    context: &ReviewContext,
    capability: &str,
    evidence_identity: &str,
) -> Result<Option<(String, String, String)>> {
    if !context
        .required_capabilities
        .iter()
        .any(|required| required == capability)
    {
        return Ok(None);
    }
    let recipe = digest_input("recipe", &context.recipe)?;
    let toolchain = digest_input("toolchain", &context.toolchain)?;
    let environment = digest_input("environment", &context.environment)?;
    let required = context
        .required_evidence
        .iter()
        .find(|evidence| evidence.identity == evidence_identity)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "capability {capability} requires evidence identity {evidence_identity}"
            ))
        })?;
    let evidence_recipe = digest_input("required evidence", &required.digest)?;
    if evidence_recipe != recipe {
        return Err(Error::Invalid(format!(
            "required evidence {evidence_identity} digest does not match the context recipe"
        )));
    }
    Ok(Some((recipe, toolchain, environment)))
}

fn digest_input(kind: &str, input: &InputIdentity) -> Result<String> {
    match input {
        InputIdentity::Digest { sha256 } => {
            validate_digest(kind, sha256)?;
            Ok(sha256.clone())
        }
        InputIdentity::Unknown {} => Err(Error::Invalid(format!("{kind} is unknown"))),
        InputIdentity::Unavailable { reason } => {
            Err(Error::Invalid(format!("{kind} is unavailable: {reason}")))
        }
    }
}

pub(crate) fn validate_subject(subject: &CheckSubject) -> Result<()> {
    validate_identifier("repository ID", &subject.repository)?;
    subject.change.validate()?;
    if subject.repository != subject.change.repository {
        return Err(Error::Invalid(
            "check subject repository identities do not match".to_owned(),
        ));
    }
    validate_positive_sql_integer("change version", subject.change_version)?;
    validate_digest("context digest", &subject.context_digest)?;
    validate_digest("revision", &subject.revision)?;
    validate_digest("recipe digest", &subject.recipe_sha256)?;
    validate_digest("toolchain digest", &subject.toolchain_sha256)?;
    validate_digest("environment digest", &subject.environment_sha256)
}

fn request_digest(
    attempt_id: &str,
    subject: &CheckSubject,
    recipe: &SourcePathsRecipe,
) -> Result<String> {
    let bytes = serde_json::to_vec(&CanonicalRequest {
        attempt_id,
        subject,
        check_kind: CHECK_KIND,
        recipe,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.check-request.v1\0");
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn request_digest_for_attempt(
    attempt: &CheckAttempt,
    recipe: &SourcePathsRecipe,
) -> Result<String> {
    request_digest(&attempt.attempt_id, &attempt.subject, recipe)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn validate_identifier(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(Error::Limit(format!(
            "{kind} must contain 1 to {MAX_IDENTIFIER_BYTES} bytes"
        )));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
    }) {
        return Err(Error::Invalid(format!(
            "{kind} contains unsupported characters"
        )));
    }
    Ok(())
}

pub(crate) fn validate_digest(kind: &str, value: &str) -> Result<()> {
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

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > 1_024 {
        return Err(Error::Invalid(
            "source path length must be between 1 and 1024 bytes".to_owned(),
        ));
    }
    if !path.is_ascii() || path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(Error::Invalid(
            "source path must be relative portable ASCII with '/' separators".to_owned(),
        ));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(Error::Invalid(
                "source path contains an empty or dot segment".to_owned(),
            ));
        }
        if segment.len() > 255 {
            return Err(Error::Invalid(
                "source path component exceeds 255 bytes".to_owned(),
            ));
        }
        if segment.ends_with(['.', ' '])
            || segment.bytes().any(|byte| {
                byte < 0x20 || matches!(byte, b'<' | b'>' | b':' | b'"' | b'|' | b'?' | b'*')
            })
        {
            return Err(Error::Invalid(
                "source path is not portable across supported filesystems".to_owned(),
            ));
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
            return Err(Error::Invalid(
                "source path contains a reserved device name".to_owned(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_reason(reason: &str) -> Result<()> {
    if reason.is_empty() || reason.len() > MAX_REASON_BYTES || reason.contains('\0') {
        return Err(Error::Limit(format!(
            "check terminal reason must contain 1 to {MAX_REASON_BYTES} bytes"
        )));
    }
    Ok(())
}

pub(crate) fn validate_positive_sql_integer(kind: &str, value: u64) -> Result<()> {
    if value == 0 {
        return Err(Error::Invalid(format!("{kind} must be positive")));
    }
    sql_i64(kind, value).map(|_| ())
}

pub(crate) fn sql_i64(kind: &str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Limit(format!("{kind} exceeds SQLite INTEGER")))
}

pub(crate) fn sql_u64(kind: &str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Invalid(format!("stored {kind} is negative")))
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn truncate_utf8(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}
