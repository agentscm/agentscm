//! Agent-owned attempts over the existing skill-workflow journal.
//!
//! This module is a trusted-local core. Its authorization callback must be backed by current
//! workspace authorization before a network adapter exposes it. Retained assertions and signed
//! observations never update workflow steps or create authority receipts in this phase.

use std::fs::symlink_metadata;
use std::path::Path;

use ed25519_dalek::{Signature, VerifyingKey};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Journal, OpenMode};
use crate::authority::Authority;
use crate::context::ReviewContext;
use crate::model::TargetState;
use crate::skill_workflow::compiler::{CompiledPlan, compile};
use crate::skill_workflow::contracts::{StepAction, StepDefinition};
use crate::skill_workflow::task_contracts::*;
use crate::store::Engine;
use crate::{Error, Result};

const MAX_TASKS: i64 = 4_096;
const MAX_ATTEMPTS: i64 = 16_384;
const MAX_OPERATIONS: i64 = 32_768;
const MAX_OUTCOME_BYTES: usize = 512 * 1024;
const TASK_ID_DOMAIN: &[u8] = b"agentscm.agent-workflow-task-id.v1\0";
const POLICY_DIGEST_DOMAIN: &[u8] = b"agentscm.agent-workflow-policy.v1\0";

pub trait TaskAuthorizationGuard: Send + Sync {
    fn authorize(&self, actor: &AuthenticatedAgentIdentity, repository: &str) -> Result<()>;

    /// Adapters must hold current actor/workspace authorization across this bounded callback.
    /// Lock order: actor/workspace guard -> journal -> native authority writer fence.
    /// The default is for trusted-local guards with no mutable authorization state.
    fn with_current_authorization(
        &self,
        actor: &AuthenticatedAgentIdentity,
        repository: &str,
        transition: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        self.authorize(actor, repository)?;
        transition()
    }
}

pub trait TaskClock: Send + Sync {
    fn now_ms(&self) -> Result<u64>;
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentAttemptState {
    Leased,
    Expired,
    Finished,
    Cancelled,
    Replaced,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskAttemptView {
    pub attempt: WorkflowTaskAttempt,
    pub effective_expires_at_ms: u64,
    pub state: AgentAttemptState,
    pub assertion_digest: Option<String>,
    pub superseding_fence: Option<u64>,
    /// Always true in phase 2: retained agent material is not an admission receipt.
    pub admission_nonpassing: bool,
    /// Current subject and enrollment applicability, not permission to replay execution.
    /// Retained outcomes may describe an old lease even when this is false.
    pub currently_applicable: bool,
}

impl TaskAttemptView {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        if input.len() > MAX_OUTCOME_BYTES {
            return Err(Error::Limit("task attempt view exceeds limit".into()));
        }
        let value: Self = serde_json::from_slice(input)?;
        value.validate()?;
        Ok(value)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_OUTCOME_BYTES {
            return Err(Error::Limit("task attempt view exceeds limit".into()));
        }
        Ok(bytes)
    }

    fn validate(&self) -> Result<()> {
        self.attempt.canonical_bytes()?;
        let maximum = self
            .attempt
            .lease
            .issued_at_ms
            .checked_add(24 * 60 * 60 * 1_000)
            .ok_or_else(|| Error::Limit("task lease timestamp overflow".into()))?;
        if self.effective_expires_at_ms < self.attempt.lease.expires_at_ms
            || self.effective_expires_at_ms > maximum
        {
            return Err(Error::Invalid(
                "effective task lease is outside its immutable attempt bounds".into(),
            ));
        }
        if !self.admission_nonpassing {
            return Err(Error::Invalid(
                "agent task attempts cannot be admission-passing in phase 2".into(),
            ));
        }
        if let Some(digest) = &self.assertion_digest {
            validate_raw_digest(digest)?;
        }
        match self.state {
            AgentAttemptState::Finished if self.assertion_digest.is_none() => Err(Error::Invalid(
                "finished task attempt is missing its retained assertion".into(),
            )),
            AgentAttemptState::Replaced if self.superseding_fence.is_none() => Err(Error::Invalid(
                "replaced task attempt is missing its superseding fence".into(),
            )),
            AgentAttemptState::Replaced
                if self.superseding_fence <= Some(self.attempt.lease.fence) =>
            {
                Err(Error::Invalid(
                    "replacement fence is not strictly monotonic".into(),
                ))
            }
            _ if self.state != AgentAttemptState::Finished && self.assertion_digest.is_some() => {
                Err(Error::Invalid(
                    "non-finished task attempt cannot retain an assertion".into(),
                ))
            }
            _ => Ok(()),
        }
    }
}

/// Shared local core intended for later thin API/CLI/MCP adapters.
pub struct AgentTaskService<'a> {
    data_dir: &'a Path,
    profile: &'a str,
    clock: &'a dyn TaskClock,
    authorize: &'a dyn TaskAuthorizationGuard,
}

impl<'a> AgentTaskService<'a> {
    pub fn new(
        data_dir: &'a Path,
        profile: &'a str,
        clock: &'a dyn TaskClock,
        authorize: &'a dyn TaskAuthorizationGuard,
    ) -> Result<Self> {
        validate_identifier(profile)?;
        Ok(Self {
            data_dir,
            profile,
            clock,
            authorize,
        })
    }

    /// Derives installed logical tasks without creating or migrating a journal.
    pub fn requirements(
        &self,
        context: &ReviewContext,
        actor: &AuthenticatedAgentIdentity,
    ) -> Result<Vec<WorkflowTask>> {
        self.authorize
            .authorize(actor, &context.change.repository)?;
        Ok(self.current(context)?.tasks)
    }

    /// Selects the next graph task using retained task-attempt state when present.
    /// This is scheduling within the existing workflow journal, not a second inbox.
    pub fn next_task(
        &self,
        context: &ReviewContext,
        actor: &AuthenticatedAgentIdentity,
    ) -> Result<Option<WorkflowTask>> {
        self.authorize
            .authorize(actor, &context.change.repository)?;
        let current = self.current(context)?;
        let states = self.read_task_states(&current.tasks)?;
        for (index, step) in current.plan.ordered_steps().iter().enumerate() {
            if states[index] != LogicalState::Pending
                || require_observation_task(&current.tasks[index]).is_err()
            {
                continue;
            }
            let ready = step.needs.iter().all(|dependency| {
                current
                    .plan
                    .ordered_steps()
                    .iter()
                    .position(|candidate| &candidate.id == dependency)
                    .is_some_and(|position| states[position] == LogicalState::CompletedObservation)
            });
            if ready {
                return Ok(Some(current.tasks[index].clone()));
            }
        }
        Ok(None)
    }

    pub fn instruction_entrypoint(
        &self,
        context: &ReviewContext,
        actor: &AuthenticatedAgentIdentity,
        task_id: &str,
        task_digest: &str,
        supported_schema_versions: &[u32],
    ) -> Result<InstructionEntrypointResponse> {
        self.authorize
            .authorize(actor, &context.change.repository)?;
        if !supported_schema_versions.contains(&AGENT_TASK_CONTRACT_VERSION) {
            let response = InstructionEntrypointResponse::SchemaMismatch {
                capability: AGENT_TASK_CAPABILITY.into(),
                required_schema_version: AGENT_TASK_CONTRACT_VERSION,
                supported_schema_versions: supported_schema_versions.to_vec(),
            };
            return InstructionEntrypointResponse::from_slice(&serde_json::to_vec(&response)?);
        }
        let current = self.current(context)?;
        let task = exact_task(&current.tasks, task_id, task_digest)?;
        Ok(InstructionEntrypointResponse::Available {
            entrypoint: task.entrypoint.clone(),
        })
    }

    /// Reads immutable native source selected only by the stored task reference.
    /// It never opens the workflow journal and therefore cannot initialize it.
    pub fn reference(
        &self,
        context: &ReviewContext,
        request: &ReferenceFetchRequest,
    ) -> Result<ReferenceFetchResponse> {
        self.authorize
            .authorize(&request.actor, &context.change.repository)?;
        let current = self.current(context)?;
        let task = exact_task(&current.tasks, &request.task_id, &request.task_digest)?;
        request.validate_for_task(task)?;
        let reference = task
            .entrypoint
            .references
            .iter()
            .find(|reference| reference.name == request.reference_name)
            .ok_or_else(|| Error::Invalid("unknown instruction reference".into()))?;
        let engine = Engine::open(self.data_dir)?;
        let response = match engine.read_regular_file_bounded(
            &task.subject.revision,
            &reference.path,
            MAX_REFERENCE_BYTES,
        ) {
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(content) if sha256(content.as_bytes()) == reference.sha256 => {
                    ReferenceFetchResponse::Available {
                        task_id: task.task_id.clone(),
                        task_digest: task.digest()?,
                        reference_name: reference.name.clone(),
                        sha256: reference.sha256.clone(),
                        schema_version: AGENT_TASK_CONTRACT_VERSION,
                        content,
                    }
                }
                Ok(_) => ReferenceFetchResponse::Uncertain {
                    task_id: task.task_id.clone(),
                    task_digest: task.digest()?,
                    reference_name: reference.name.clone(),
                },
                Err(_) => ReferenceFetchResponse::Denied {
                    task_id: task.task_id.clone(),
                    task_digest: task.digest()?,
                    reference_name: reference.name.clone(),
                },
            },
            Err(Error::Invalid(_)) | Err(Error::SourceDenied) => ReferenceFetchResponse::Denied {
                task_id: task.task_id.clone(),
                task_digest: task.digest()?,
                reference_name: reference.name.clone(),
            },
            Err(Error::NotFound(_)) => ReferenceFetchResponse::Missing {
                task_id: task.task_id.clone(),
                task_digest: task.digest()?,
                reference_name: reference.name.clone(),
            },
            Ok(None) | Err(Error::Limit(_)) => ReferenceFetchResponse::Limit {
                task_id: task.task_id.clone(),
                task_digest: task.digest()?,
                reference_name: reference.name.clone(),
            },
            Err(error) => return Err(error),
        };
        response.validate_for_request(request)?;
        Ok(response)
    }

    pub fn claim(
        &self,
        context: &ReviewContext,
        request: &TaskClaimRequest,
    ) -> Result<TaskAttemptView> {
        self.authorize
            .authorize(&request.agent, &context.change.repository)?;
        if let Some(view) = self.retained_retry(
            context,
            &request.agent,
            &request.request_id,
            "claim",
            &request.digest()?,
        )? {
            view.attempt.validate_for_claim(request)?;
            return Ok(view);
        }
        let current = self.current(context)?;
        self.with_authorized(&request.agent, context, || {
            self.claim_current(context, request, &current)
        })
    }

    fn claim_current(
        &self,
        context: &ReviewContext,
        request: &TaskClaimRequest,
        current: &CurrentTasks,
    ) -> Result<TaskAttemptView> {
        let task = exact_task(&current.tasks, &request.task_id, &request.task_digest)?;
        let authority = Authority::open_read_only(self.data_dir)?;
        let registration =
            authority.require_current_workflow_executor(&task.subject, &request.executor)?;
        request.validate_for_task(task, &registration.enrollment)?;
        let now = self.clock.now_ms()?;
        let expires = now
            .checked_add(request.requested_lease_ms)
            .ok_or_else(|| Error::Limit("task lease timestamp overflow".into()))?;
        let mut journal = Journal::open(self.data_dir, OpenMode::Create)?;
        let tx = journal
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(view) = operation_retry(
            &tx,
            &request.agent,
            &request.request_id,
            "claim",
            &request.digest()?,
        )? {
            let view = self.retry_applicability(&tx, context, view)?;
            tx.commit()?;
            return Ok(view);
        }
        require_observation_task(task)?;
        let reused: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_workflow_attempts WHERE execution_operation_id=?1)",
            [&request.execution_operation_id],
            |row| row.get(0),
        )?;
        if reused {
            return Err(Error::Conflict(
                "fresh execution requires a fresh execution operation ID".into(),
            ));
        }
        let states = read_task_states(&tx, &current.tasks)?;
        let index = current
            .tasks
            .iter()
            .position(|candidate| candidate.task_id == task.task_id)
            .unwrap();
        if states[index] != LogicalState::Pending || !dependencies_ready(current, &states, index) {
            return Err(Error::Conflict(
                "task is not pending with completed dependencies".into(),
            ));
        }
        enforce_inventory(&tx)?;
        refuse_unresolved_prior_tasks(&tx, task, now)?;
        store_task(&tx, task)?;
        if let Some(state) = latest_attempt_state(&tx, &task.task_id, now)?
            && state != AgentAttemptState::Replaced
        {
            return Err(Error::Conflict(
                "task already has an attempt; explicit reconciliation is required before replacement"
                    .into(),
            ));
        }
        let fence: i64 = tx.query_row(
            "SELECT next_fence FROM agent_workflow_tasks WHERE task_id=?1",
            [&task.task_id],
            |row| row.get(0),
        )?;
        let attempt = WorkflowTaskAttempt {
            version: AGENT_TASK_CONTRACT_VERSION,
            claim_request_id: request.request_id.clone(),
            task_id: task.task_id.clone(),
            task_digest: task.digest()?,
            attempt_id: request.attempt_id.clone(),
            execution_operation_id: request.execution_operation_id.clone(),
            agent: request.agent.clone(),
            executor: request.executor.clone(),
            lease: AttemptLease {
                fence: to_u64("task fence", fence)?,
                issued_at_ms: now,
                expires_at_ms: expires,
            },
        };
        attempt.validate_for_task(task, &registration.enrollment)?;
        attempt.validate_for_claim(request)?;
        let view = TaskAttemptView {
            attempt,
            effective_expires_at_ms: expires,
            state: AgentAttemptState::Leased,
            assertion_digest: None,
            superseding_fence: None,
            admission_nonpassing: true,
            currently_applicable: true,
        };
        let attempt_bytes = view.attempt.canonical_bytes()?;
        tx.execute(
            "INSERT INTO agent_workflow_attempts(attempt_id,task_id,fence,lease_expires_at,state,superseding_fence,assertion_digest,attempt_json,execution_operation_id) VALUES(?1,?2,?3,?4,'leased',NULL,NULL,?5,?6)",
            params![view.attempt.attempt_id, view.attempt.task_id, fence, to_i64("task lease expiry", expires)?, attempt_bytes, view.attempt.execution_operation_id],
        )?;
        tx.execute(
            "UPDATE agent_workflow_tasks SET next_fence=?1 WHERE task_id=?2 AND next_fence=?3",
            params![
                fence
                    .checked_add(1)
                    .ok_or_else(|| Error::Limit("task fence exhausted".into()))?,
                task.task_id,
                fence
            ],
        )?;
        store_operation(
            &tx,
            &request.agent,
            &request.request_id,
            "claim",
            &request.digest()?,
            &view,
        )?;
        self.commit_fenced(tx, context, task, &request.executor, false)?;
        Ok(view)
    }

    pub fn heartbeat(
        &self,
        context: &ReviewContext,
        request: &TaskMutationRequest,
    ) -> Result<TaskAttemptView> {
        if !matches!(request.action, TaskMutationAction::Heartbeat {}) {
            return Err(Error::Invalid(
                "heartbeat requires a heartbeat mutation".into(),
            ));
        }
        self.mutate(context, request, Mutation::Heartbeat)
    }

    pub fn cancel_attempt(
        &self,
        context: &ReviewContext,
        request: &TaskMutationRequest,
    ) -> Result<TaskAttemptView> {
        if !matches!(request.action, TaskMutationAction::Cancel { .. }) {
            return Err(Error::Invalid(
                "cancellation requires a cancel mutation".into(),
            ));
        }
        self.mutate(context, request, Mutation::Cancel)
    }

    pub fn reconcile_attempt(
        &self,
        context: &ReviewContext,
        request: &TaskMutationRequest,
    ) -> Result<TaskAttemptView> {
        if !matches!(request.action, TaskMutationAction::Reconcile {}) {
            return Err(Error::Invalid(
                "reconciliation requires a reconcile mutation".into(),
            ));
        }
        self.mutate(context, request, Mutation::Reconcile)
    }

    pub fn submit_result(
        &self,
        context: &ReviewContext,
        assertion: &TaskResultAssertion,
    ) -> Result<TaskAttemptView> {
        self.authorize
            .authorize(&assertion.agent, &context.change.repository)?;
        if let Some(view) = self.retained_retry(
            context,
            &assertion.agent,
            &assertion.request_id,
            "result",
            &assertion.request_digest()?,
        )? {
            let journal = open_optional_task_journal(self.data_dir)?
                .ok_or_else(|| Error::Invalid("retained journal disappeared".into()))?;
            let task = retained_task(
                &journal.connection,
                context,
                self.profile,
                &assertion.task_id,
                &assertion.task_digest,
            )?;
            assertion.validate_for_task_attempt(&task, &view.attempt)?;
            return Ok(view);
        }
        let current = self.current(context)?;
        self.with_authorized(&assertion.agent, context, || {
            self.submit_current_result(context, assertion, &current)
        })
    }

    fn submit_current_result(
        &self,
        context: &ReviewContext,
        assertion: &TaskResultAssertion,
        current: &CurrentTasks,
    ) -> Result<TaskAttemptView> {
        let task = exact_task(&current.tasks, &assertion.task_id, &assertion.task_digest)?;
        require_observation_task(task)?;
        let authority = Authority::open_read_only(self.data_dir)?;
        let registration =
            authority.require_current_workflow_executor(&task.subject, &assertion.executor)?;
        let mut journal = Journal::open(self.data_dir, OpenMode::Create)?;
        let tx = journal
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(view) = operation_retry(
            &tx,
            &assertion.agent,
            &assertion.request_id,
            "result",
            &assertion.request_digest()?,
        )? {
            let view = self.retry_applicability(&tx, context, view)?;
            tx.commit()?;
            return Ok(view);
        }
        let view = read_attempt(&tx, &assertion.attempt_id, self.clock.now_ms()?)?
            .ok_or_else(|| Error::NotFound(format!("task attempt {}", assertion.attempt_id)))?;
        if view.state != AgentAttemptState::Leased {
            return Err(Error::Conflict(
                "result is not for the current live attempt generation".into(),
            ));
        }
        assertion.validate_for_task_attempt(task, &view.attempt)?;
        match assertion.trust_class {
            EvidenceTrustClass::RegisteredExecutorObservation => {
                verify_observation(assertion, &registration.public_key_hex)?;
            }
            EvidenceTrustClass::AgentAssertion => {}
            EvidenceTrustClass::IndependentControllerEvidence => {
                return Err(Error::Invalid(
                    "agent result cannot claim independent controller evidence".into(),
                ));
            }
        }
        let assertion_bytes = assertion.canonical_bytes()?;
        let assertion_digest = assertion.digest()?;
        tx.execute(
            "INSERT INTO agent_workflow_assertions(digest,attempt_id,canonical_json) VALUES(?1,?2,?3)",
            params![assertion_digest, assertion.attempt_id, assertion_bytes],
        )?;
        if tx.execute(
            "UPDATE agent_workflow_attempts SET state='finished',assertion_digest=?1 WHERE attempt_id=?2 AND state='leased' AND fence=?3",
            params![assertion_digest, assertion.attempt_id, to_i64("task fence", assertion.fence)?],
        )? != 1
        {
            return Err(Error::Conflict("task attempt changed before result retention".into()));
        }
        let finished = TaskAttemptView {
            attempt: view.attempt,
            effective_expires_at_ms: view.effective_expires_at_ms,
            state: AgentAttemptState::Finished,
            assertion_digest: Some(assertion_digest),
            superseding_fence: None,
            admission_nonpassing: true,
            currently_applicable: true,
        };
        store_operation(
            &tx,
            &assertion.agent,
            &assertion.request_id,
            "result",
            &assertion.request_digest()?,
            &finished,
        )?;
        self.commit_fenced(tx, context, task, &assertion.executor, false)?;
        Ok(finished)
    }

    /// Exact-attempt read. Missing journals and pre-v3 journals are ordinary not-found results.
    pub fn status(
        &self,
        context: &ReviewContext,
        request: &TaskStatusRequest,
    ) -> Result<Option<TaskAttemptView>> {
        self.authorize
            .authorize(&request.agent, &context.change.repository)?;
        let Some(journal) = open_optional_task_journal(self.data_dir)? else {
            return Ok(None);
        };
        let Some(view) = read_attempt(
            &journal.connection,
            &request.attempt_id,
            self.clock.now_ms()?,
        )?
        else {
            return Ok(None);
        };
        request.validate_for_attempt(&view.attempt)?;
        let task = retained_task(
            &journal.connection,
            context,
            self.profile,
            &request.task_id,
            &request.task_digest,
        )?;
        Ok(Some(self.applicability(context, &task, view)?))
    }

    /// Side-effect-free recovery projection for an exact mutation request.
    pub fn recovery(
        &self,
        context: &ReviewContext,
        request: &TaskMutationRequest,
    ) -> Result<TaskRecoveryEnvelope> {
        self.authorize
            .authorize(&request.agent, &context.change.repository)?;
        let Some(journal) = open_optional_task_journal(self.data_dir)? else {
            return rejected_recovery(request, AttemptObservedState::Unknown);
        };
        let view = read_attempt(
            &journal.connection,
            &request.attempt_id,
            self.clock.now_ms()?,
        )?;
        let Some(view) = view else {
            return rejected_recovery(request, AttemptObservedState::Unknown);
        };
        request.validate_for_attempt(&view.attempt)?;
        retained_task(
            &journal.connection,
            context,
            self.profile,
            &request.task_id,
            &request.task_digest,
        )?;
        if operation_retry(
            &journal.connection,
            &request.agent,
            &request.request_id,
            mutation_kind(&request.action),
            &request.request_digest()?,
        )?
        .is_some()
        {
            return confirmed_recovery(request, &view);
        }
        rejected_recovery(request, observed_state(view.state))
    }

    fn mutate(
        &self,
        context: &ReviewContext,
        request: &TaskMutationRequest,
        mutation: Mutation,
    ) -> Result<TaskAttemptView> {
        self.authorize
            .authorize(&request.agent, &context.change.repository)?;
        if let Some(view) = self.retained_retry(
            context,
            &request.agent,
            &request.request_id,
            mutation_kind(&request.action),
            &request.request_digest()?,
        )? {
            request.validate_for_attempt(&view.attempt)?;
            return Ok(view);
        }
        if matches!(mutation, Mutation::Heartbeat) {
            let current = self.current(context)?;
            exact_task(&current.tasks, &request.task_id, &request.task_digest)?;
        }
        self.with_authorized(&request.agent, context, || {
            self.mutate_authorized(context, request, mutation)
        })
    }

    fn mutate_authorized(
        &self,
        context: &ReviewContext,
        request: &TaskMutationRequest,
        mutation: Mutation,
    ) -> Result<TaskAttemptView> {
        let mut journal = Journal::open(self.data_dir, OpenMode::Create)?;
        let tx = journal
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let kind = mutation_kind(&request.action);
        if let Some(view) = operation_retry(
            &tx,
            &request.agent,
            &request.request_id,
            kind,
            &request.request_digest()?,
        )? {
            let view = self.retry_applicability(&tx, context, view)?;
            tx.commit()?;
            return Ok(view);
        }
        let task = retained_task(
            &tx,
            context,
            self.profile,
            &request.task_id,
            &request.task_digest,
        )?;
        let now = self.clock.now_ms()?;
        let mut view = read_attempt(&tx, &request.attempt_id, now)?
            .ok_or_else(|| Error::NotFound(format!("task attempt {}", request.attempt_id)))?;
        request.validate_for_attempt(&view.attempt)?;
        match mutation {
            Mutation::Heartbeat => {
                if view.state != AgentAttemptState::Leased {
                    return Err(Error::Conflict(
                        "heartbeat requires a live current lease".into(),
                    ));
                }
                let duration = view.attempt.lease.expires_at_ms - view.attempt.lease.issued_at_ms;
                let maximum = view
                    .attempt
                    .lease
                    .issued_at_ms
                    .checked_add(24 * 60 * 60 * 1_000)
                    .ok_or_else(|| Error::Limit("task lease timestamp overflow".into()))?;
                let extended = now
                    .checked_add(duration)
                    .ok_or_else(|| Error::Limit("task lease timestamp overflow".into()))?
                    .min(maximum);
                if extended <= view.effective_expires_at_ms {
                    return Err(Error::Conflict(
                        "task lease cannot be extended further".into(),
                    ));
                }
                view.effective_expires_at_ms = extended;
                tx.execute(
                    "UPDATE agent_workflow_attempts SET lease_expires_at=?1 WHERE attempt_id=?2 AND state='leased' AND fence=?3",
                    params![to_i64("task lease expiry", extended)?, view.attempt.attempt_id, to_i64("task fence", view.attempt.lease.fence)?],
                )?;
            }
            Mutation::Cancel => {
                if view.state != AgentAttemptState::Leased
                    && view.state != AgentAttemptState::Expired
                {
                    return Err(Error::Conflict(
                        "only an active or expired attempt can be cancelled".into(),
                    ));
                }
                if tx.execute(
                    "UPDATE agent_workflow_attempts SET state='cancelled' WHERE attempt_id=?1 AND state='leased' AND fence=?2",
                    params![view.attempt.attempt_id, to_i64("task fence", view.attempt.lease.fence)?],
                )? != 1
                {
                    return Err(Error::Conflict("task attempt changed before cancellation".into()));
                }
                view.state = AgentAttemptState::Cancelled;
            }
            Mutation::Reconcile => {
                if !matches!(
                    view.state,
                    AgentAttemptState::Leased
                        | AgentAttemptState::Expired
                        | AgentAttemptState::Cancelled
                ) {
                    return Err(Error::Conflict(
                        "only an unfinished attempt can be reconciled".into(),
                    ));
                }
                let superseding: i64 = tx.query_row(
                    "SELECT next_fence FROM agent_workflow_tasks WHERE task_id=?1",
                    [&view.attempt.task_id],
                    |row| row.get(0),
                )?;
                if superseding <= to_i64("task fence", view.attempt.lease.fence)? {
                    return Err(Error::Invalid("stored task fence is not monotonic".into()));
                }
                if tx.execute(
                    "UPDATE agent_workflow_attempts SET state='replaced',superseding_fence=?1 WHERE attempt_id=?2 AND state IN ('leased','cancelled') AND fence=?3",
                    params![superseding, view.attempt.attempt_id, to_i64("task fence", view.attempt.lease.fence)?],
                )? != 1
                {
                    return Err(Error::Conflict("task attempt changed before reconciliation".into()));
                }
                view.state = AgentAttemptState::Replaced;
                view.superseding_fence = Some(to_u64("task fence", superseding)?);
            }
        }
        view.admission_nonpassing = true;
        view = self.applicability(context, &task, view)?;
        store_operation(
            &tx,
            &request.agent,
            &request.request_id,
            kind,
            &request.request_digest()?,
            &view,
        )?;
        self.commit_fenced(
            tx,
            context,
            &task,
            &request.executor,
            !matches!(mutation, Mutation::Heartbeat),
        )?;
        Ok(view)
    }

    fn current(&self, context: &ReviewContext) -> Result<CurrentTasks> {
        context.validate()?;
        let context_digest = context.digest()?;
        let authority = Authority::open_read_only(self.data_dir)?;
        if authority.repository_id()? != context.change.repository
            || authority.policy_epoch()? != context.policy_epoch
        {
            return Err(Error::Conflict(
                "workflow task subject is stale under current authority".into(),
            ));
        }
        let change = authority.change(&context.change)?;
        if change.version != context.change_version || change.context_digest != context_digest {
            return Err(Error::Conflict(
                "workflow task review context is stale".into(),
            ));
        }
        if authority.review_context(&context_digest)? != *context {
            return Err(Error::Invalid(
                "current review context bytes are inconsistent".into(),
            ));
        }
        validate_target(authority.target(&context.target)?, context)?;
        let requirement = authority
            .workflow_requirement(&context.target)?
            .ok_or_else(|| {
                Error::Conflict("target has no installed workflow requirement".into())
            })?;
        let requirement_digest = requirement.digest()?;
        let definition = requirement.instantiate(context)?;
        let plan = compile(self.data_dir, &definition)?;
        if plan.skill_digest() != requirement.skill_digest {
            return Err(Error::Conflict("installed workflow skill is stale".into()));
        }
        let tasks = derive_tasks(self.profile, context, &requirement_digest, &plan)?;
        Ok(CurrentTasks { plan, tasks })
    }

    fn with_authorized<T>(
        &self,
        actor: &AuthenticatedAgentIdentity,
        context: &ReviewContext,
        transition: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut transition = Some(transition);
        let mut output = None;
        self.authorize.with_current_authorization(
            actor,
            &context.change.repository,
            &mut || {
                let run = transition
                    .take()
                    .ok_or_else(|| Error::Invalid("authorization callback invoked twice".into()))?;
                output = Some(run()?);
                Ok(())
            },
        )?;
        output.ok_or_else(|| Error::Invalid("authorization callback was not invoked".into()))
    }

    fn commit_fenced(
        &self,
        tx: Transaction<'_>,
        context: &ReviewContext,
        task: &WorkflowTask,
        binding: &ExecutorBinding,
        historical_cleanup: bool,
    ) -> Result<()> {
        // Caller holds actor/workspace authorization, then journal. Native writers never
        // acquire the journal; no compilation or external execution is allowed in this fence.
        let authority = Authority::open(self.data_dir)?;
        authority.with_task_writer_fence(|held| {
            held.check_task_subject(context, &task.subject, binding, historical_cleanup)?;
            tx.commit()?;
            Ok(())
        })
    }

    fn applicability(
        &self,
        context: &ReviewContext,
        task: &WorkflowTask,
        mut view: TaskAttemptView,
    ) -> Result<TaskAttemptView> {
        view.currently_applicable = match Authority::open_read_only(self.data_dir)?
            .check_task_subject(context, &task.subject, &view.attempt.executor, false)
        {
            Ok(()) => true,
            Err(Error::Conflict(_) | Error::NotFound(_)) => false,
            Err(error) => return Err(error),
        };
        Ok(view)
    }

    fn retained_retry(
        &self,
        context: &ReviewContext,
        actor: &AuthenticatedAgentIdentity,
        request_id: &str,
        kind: &str,
        digest: &str,
    ) -> Result<Option<TaskAttemptView>> {
        let Some(journal) = open_optional_task_journal(self.data_dir)? else {
            return Ok(None);
        };
        let Some(view) = operation_retry(&journal.connection, actor, request_id, kind, digest)?
        else {
            return Ok(None);
        };
        Ok(Some(self.retry_applicability(
            &journal.connection,
            context,
            view,
        )?))
    }

    fn retry_applicability(
        &self,
        connection: &Connection,
        context: &ReviewContext,
        view: TaskAttemptView,
    ) -> Result<TaskAttemptView> {
        let task = retained_task(
            connection,
            context,
            self.profile,
            &view.attempt.task_id,
            &view.attempt.task_digest,
        )?;
        let now = read_attempt(connection, &view.attempt.attempt_id, self.clock.now_ms()?)?
            .ok_or_else(|| Error::Invalid("retained attempt disappeared".into()))?;
        let mut view = self.applicability(context, &task, view)?;
        view.currently_applicable &= view.state == now.state
            && view.effective_expires_at_ms == now.effective_expires_at_ms
            && view.assertion_digest == now.assertion_digest
            && view.superseding_fence == now.superseding_fence;
        Ok(view)
    }

    fn read_task_states(&self, tasks: &[WorkflowTask]) -> Result<Vec<LogicalState>> {
        let Some(journal) = open_optional_task_journal(self.data_dir)? else {
            return Ok(vec![LogicalState::Pending; tasks.len()]);
        };
        read_task_states(&journal.connection, tasks)
    }
}

fn read_task_states(connection: &Connection, tasks: &[WorkflowTask]) -> Result<Vec<LogicalState>> {
    tasks.iter().map(|task| {
        let attempt_id: Option<String> = connection.query_row(
            "SELECT attempt_id FROM agent_workflow_attempts WHERE task_id=?1 ORDER BY fence DESC LIMIT 1",
            [&task.task_id], |row| row.get(0)).optional()?;
        let Some(id) = attempt_id else { return Ok(LogicalState::Pending); };
        let view = read_attempt(connection, &id, 0)?.ok_or_else(|| Error::Invalid("task attempt disappeared".into()))?;
        if view.attempt.task_id != task.task_id || view.attempt.task_digest != task.digest()? {
            return Err(Error::Invalid("retained attempt does not bind queried task".into()));
        }
        if view.state == AgentAttemptState::Replaced { return Ok(LogicalState::Pending); }
        if view.state != AgentAttemptState::Finished { return Ok(LogicalState::TerminalNonpassing); }
        let assertion = read_assertion(connection, task, &view)?;
        Ok(if require_observation_task(task).is_ok()
            && assertion.trust_class == EvidenceTrustClass::RegisteredExecutorObservation
            && assertion.outcome == TaskOutcome::Passed {
                LogicalState::CompletedObservation
            } else { LogicalState::TerminalNonpassing })
    }).collect()
}

fn read_assertion(
    connection: &Connection,
    task: &WorkflowTask,
    view: &TaskAttemptView,
) -> Result<TaskResultAssertion> {
    let digest = view
        .assertion_digest
        .as_ref()
        .ok_or_else(|| Error::Invalid("finished attempt missing assertion".into()))?;
    let (attempt_id, bytes): (String, Vec<u8>) = connection.query_row(
        "SELECT attempt_id,canonical_json FROM agent_workflow_assertions WHERE digest=?1",
        [digest],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let assertion = TaskResultAssertion::from_slice(&bytes)?;
    if assertion.digest()? != *digest
        || assertion.canonical_bytes()? != bytes
        || attempt_id != view.attempt.attempt_id
    {
        return Err(Error::Invalid(
            "retained agent assertion failed integrity validation".into(),
        ));
    }
    assertion.validate_for_task_attempt(task, &view.attempt)?;
    Ok(assertion)
}

fn dependencies_ready(current: &CurrentTasks, states: &[LogicalState], index: usize) -> bool {
    current.plan.ordered_steps()[index]
        .needs
        .iter()
        .all(|dependency| {
            current
                .plan
                .ordered_steps()
                .iter()
                .position(|step| &step.id == dependency)
                .is_some_and(|position| states[position] == LogicalState::CompletedObservation)
        })
}

fn require_observation_task(task: &WorkflowTask) -> Result<()> {
    if task
        .entrypoint
        .required_criteria
        .iter()
        .any(|criterion| criterion.evidence_rule == CriterionEvidenceRule::ControllerOnly)
    {
        return Err(Error::Conflict(
            "controller-only tasks cannot use observation-only attempts".into(),
        ));
    }
    Ok(())
}

struct CurrentTasks {
    plan: CompiledPlan,
    tasks: Vec<WorkflowTask>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogicalState {
    Pending,
    CompletedObservation,
    TerminalNonpassing,
}

#[derive(Clone, Copy)]
enum Mutation {
    Heartbeat,
    Cancel,
    Reconcile,
}

pub(super) fn create_schema(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE agent_workflow_tasks(
            task_id TEXT PRIMARY KEY CHECK(length(task_id) BETWEEN 1 AND 128),
            task_digest TEXT NOT NULL UNIQUE CHECK(length(task_digest)=64),
            step_id TEXT NOT NULL CHECK(length(step_id) BETWEEN 1 AND 128),
            next_fence INTEGER NOT NULL CHECK(next_fence>0),
            canonical_json BLOB NOT NULL CHECK(length(canonical_json)<=262144)
         ) STRICT;
         CREATE TABLE agent_workflow_attempts(
            attempt_id TEXT PRIMARY KEY CHECK(length(attempt_id) BETWEEN 1 AND 128),
            execution_operation_id TEXT NOT NULL UNIQUE CHECK(length(execution_operation_id) BETWEEN 1 AND 128),
            task_id TEXT NOT NULL REFERENCES agent_workflow_tasks(task_id),
            fence INTEGER NOT NULL CHECK(fence>0),
            lease_expires_at INTEGER NOT NULL CHECK(lease_expires_at>0),
            state TEXT NOT NULL CHECK(state IN ('leased','finished','cancelled','replaced')),
            superseding_fence INTEGER CHECK(superseding_fence IS NULL OR superseding_fence>fence),
            assertion_digest TEXT UNIQUE CHECK(assertion_digest IS NULL OR length(assertion_digest)=64),
            attempt_json BLOB NOT NULL CHECK(length(attempt_json)<=262144),
            UNIQUE(task_id,fence),
            CHECK((state='replaced')=(superseding_fence IS NOT NULL)),
            CHECK((state='finished')=(assertion_digest IS NOT NULL))
         ) STRICT;
         CREATE TABLE agent_workflow_assertions(
            digest TEXT PRIMARY KEY CHECK(length(digest)=64),
            attempt_id TEXT NOT NULL UNIQUE REFERENCES agent_workflow_attempts(attempt_id),
            canonical_json BLOB NOT NULL CHECK(length(canonical_json)<=262144)
         ) STRICT;
         CREATE TABLE agent_workflow_operations(
            owner_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            request_id TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('claim','heartbeat','cancel','reconcile','result')),
            request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
            outcome_json BLOB NOT NULL CHECK(length(outcome_json)<=524288),
            PRIMARY KEY(owner_id,agent_id,request_id)
         ) STRICT;",
    )?;
    Ok(())
}

pub(super) fn validate_schema(connection: &Connection) -> Result<()> {
    for query in [
        "SELECT task_id,task_digest,step_id,next_fence,canonical_json FROM agent_workflow_tasks LIMIT 0",
        "SELECT attempt_id,task_id,fence,lease_expires_at,state,superseding_fence,assertion_digest,attempt_json,execution_operation_id FROM agent_workflow_attempts LIMIT 0",
        "SELECT digest,attempt_id,canonical_json FROM agent_workflow_assertions LIMIT 0",
        "SELECT owner_id,agent_id,request_id,kind,request_digest,outcome_json FROM agent_workflow_operations LIMIT 0",
    ] {
        connection.prepare(query)?;
    }
    Ok(())
}

fn derive_tasks(
    profile: &str,
    context: &ReviewContext,
    requirement_digest: &str,
    plan: &CompiledPlan,
) -> Result<Vec<WorkflowTask>> {
    let references = instruction_references(plan)?;
    let policy_digest = policy_digest(context, requirement_digest)?;
    plan.ordered_steps()
        .iter()
        .map(|step| {
            let subject = WorkflowTaskSubject {
                repository: context.change.repository.clone(),
                profile: profile.into(),
                target: context.target.clone(),
                revision: context.revision.clone(),
                change: context.change.clone(),
                change_version: context.change_version,
                context_digest: context.digest()?,
                policy_epoch: context.policy_epoch,
                requirement_digest: requirement_digest.into(),
                plan_digest: plan.digest().into(),
                skill_digest: plan.skill_digest().into(),
                step_id: step.id.clone(),
            };
            let entrypoint = InstructionEntrypoint {
                capability: AGENT_TASK_CAPABILITY.into(),
                schema_version: AGENT_TASK_CONTRACT_VERSION,
                workflow_digest: plan.digest().into(),
                policy_digest: policy_digest.clone(),
                required_criteria: criteria(step),
                references: references.clone(),
                permitted_progression: EntrypointProgression::ClaimTask,
            };
            let id_bytes = serde_json::to_vec(&(&subject, step.required, &entrypoint))?;
            let mut hasher = Sha256::new();
            hasher.update(TASK_ID_DOMAIN);
            hasher.update((id_bytes.len() as u64).to_be_bytes());
            hasher.update(id_bytes);
            let task = WorkflowTask {
                version: AGENT_TASK_CONTRACT_VERSION,
                task_id: format!("task-{:x}", hasher.finalize()),
                subject,
                required: step.required,
                entrypoint,
            };
            task.canonical_bytes()?;
            Ok(task)
        })
        .collect()
}

fn criteria(step: &StepDefinition) -> Vec<RequiredCriterion> {
    let (description, kind, evidence_rule) = match &step.action {
        StepAction::ClaudeCodeReview { .. } => (
            "Complete the installed independent controller review",
            CriterionKind::IndependentReview,
            CriterionEvidenceRule::ControllerOnly,
        ),
        StepAction::SourcePaths { .. } => (
            "Evaluate the installed immutable source-path expectations",
            CriterionKind::Check,
            CriterionEvidenceRule::EnrolledLaptopOrController,
        ),
        StepAction::RequireCapability { .. } => (
            "Evaluate the installed capability requirement",
            CriterionKind::Check,
            CriterionEvidenceRule::EnrolledLaptopOrController,
        ),
        StepAction::Command { .. } => (
            "Run the installed pinned command check",
            CriterionKind::Check,
            CriterionEvidenceRule::EnrolledLaptopOrController,
        ),
    };
    vec![RequiredCriterion {
        id: format!("{}-result", step.id),
        description: description.into(),
        kind,
        evidence_rule,
    }]
}

fn instruction_references(plan: &CompiledPlan) -> Result<Vec<NamedInstructionReference>> {
    if plan.instruction_sha256.len() > 32 {
        return Err(Error::Limit(
            "workflow instruction references exceed task contract limit".into(),
        ));
    }
    Ok(plan
        .instruction_sha256
        .iter()
        .enumerate()
        .map(|(index, (path, digest))| NamedInstructionReference {
            name: if *path == plan.definition().skill.entrypoint {
                "entrypoint".into()
            } else {
                format!("resource-{index}")
            },
            sha256: digest.clone(),
            path: path.clone(),
        })
        .collect())
}

fn policy_digest(context: &ReviewContext, requirement_digest: &str) -> Result<String> {
    let bytes = serde_json::to_vec(&(
        &context.change.repository,
        &context.target,
        context.policy_epoch,
        requirement_digest,
    ))?;
    let mut hasher = Sha256::new();
    hasher.update(POLICY_DIGEST_DOMAIN);
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_target(target: Option<TargetState>, context: &ReviewContext) -> Result<()> {
    match (&context.base, target) {
        (Some(base), Some(target))
            if base.revision == target.revision && base.target_version == target.version =>
        {
            Ok(())
        }
        (None, None) => Ok(()),
        _ => Err(Error::Conflict("workflow task target base is stale".into())),
    }
}

fn exact_task<'a>(tasks: &'a [WorkflowTask], id: &str, digest: &str) -> Result<&'a WorkflowTask> {
    let task = tasks
        .iter()
        .find(|task| task.task_id == id)
        .ok_or_else(|| Error::Conflict("workflow task ID is stale or refused".into()))?;
    if task.digest()? != digest {
        return Err(Error::Conflict(
            "workflow task digest is stale or bound to different requirements".into(),
        ));
    }
    Ok(task)
}

fn open_optional_task_journal(data_dir: &Path) -> Result<Option<Journal>> {
    let database = data_dir.join("skill-workflows/state.sqlite");
    match symlink_metadata(database) {
        Ok(_) => {
            let journal = Journal::open(data_dir, OpenMode::Read)?;
            let version: i64 =
                journal
                    .connection
                    .pragma_query_value(None, "user_version", |row| row.get(0))?;
            Ok((version >= 3).then_some(journal))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn store_task(tx: &Transaction<'_>, task: &WorkflowTask) -> Result<()> {
    let canonical = task.canonical_bytes()?;
    let digest = task.digest()?;
    if let Some((stored_digest, stored)) = tx
        .query_row(
            "SELECT task_digest,canonical_json FROM agent_workflow_tasks WHERE task_id=?1",
            [&task.task_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?
    {
        if stored_digest == digest && stored == canonical {
            return Ok(());
        }
        return Err(Error::Conflict(
            "logical task ID is already bound to different requirements".into(),
        ));
    }
    tx.execute(
        "INSERT INTO agent_workflow_tasks(task_id,task_digest,step_id,next_fence,canonical_json) VALUES(?1,?2,?3,1,?4)",
        params![task.task_id, digest, task.subject.step_id, canonical],
    )?;
    Ok(())
}

fn read_task(connection: &Connection, id: &str) -> Result<WorkflowTask> {
    let (digest, step, bytes): (String, String, Vec<u8>) = connection.query_row(
        "SELECT task_digest,step_id,canonical_json FROM agent_workflow_tasks WHERE task_id=?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let task = WorkflowTask::from_slice(&bytes)?;
    if task.task_id != id
        || task.digest()? != digest
        || task.subject.step_id != step
        || task.canonical_bytes()? != bytes
    {
        return Err(Error::Invalid(
            "retained task failed identity validation".into(),
        ));
    }
    Ok(task)
}

fn retained_task(
    connection: &Connection,
    context: &ReviewContext,
    profile: &str,
    id: &str,
    digest: &str,
) -> Result<WorkflowTask> {
    let task = read_task(connection, id)?;
    if task.digest()? != digest
        || task.subject.repository != context.change.repository
        || task.subject.change != context.change
        || task.subject.context_digest != context.digest()?
        || task.subject.revision != context.revision
        || task.subject.target != context.target
        || task.subject.change_version != context.change_version
        || task.subject.policy_epoch != context.policy_epoch
        || task.subject.profile != profile
    {
        return Err(Error::Conflict(
            "retained task is bound to a different subject".into(),
        ));
    }
    Ok(task)
}

fn refuse_unresolved_prior_tasks(
    connection: &Connection,
    task: &WorkflowTask,
    now: u64,
) -> Result<()> {
    // v3 inventory is capped at 4096: scan canonical identities rather than trusting a
    // second mutable scheduling index. Refuse an oversized restored inventory as well.
    let mut statement = connection
        .prepare("SELECT task_id FROM agent_workflow_tasks ORDER BY task_id LIMIT 4097")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if ids.len() > MAX_TASKS as usize {
        return Err(Error::Limit("agent task inventory exceeds limit".into()));
    }
    for id in ids {
        let old = read_task(connection, &id)?;
        if old.task_id == task.task_id
            || old.subject.repository != task.subject.repository
            || old.subject.change != task.subject.change
            || old.subject.profile != task.subject.profile
            || old.subject.step_id != task.subject.step_id
        {
            continue;
        }
        if let Some(state) = latest_attempt_state(connection, &id, now)?
            && !matches!(
                state,
                AgentAttemptState::Finished | AgentAttemptState::Replaced
            )
        {
            return Err(Error::Conflict(
                "prior task subject has an unresolved attempt; explicit reconciliation is required"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn read_attempt(
    connection: &Connection,
    attempt_id: &str,
    now: u64,
) -> Result<Option<TaskAttemptView>> {
    let row = connection
        .query_row(
            "SELECT lease_expires_at,state,superseding_fence,assertion_digest,attempt_json,task_id,fence,execution_operation_id FROM agent_workflow_attempts WHERE attempt_id=?1",
            [attempt_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(
            effective_expires_at,
            state,
            superseding,
            assertion_digest,
            bytes,
            task_id,
            fence,
            execution_operation_id,
        )| {
            let attempt = WorkflowTaskAttempt::from_slice(&bytes)?;
            if attempt.canonical_bytes()? != bytes
                || attempt.attempt_id != attempt_id
                || attempt.task_id != task_id
                || attempt.execution_operation_id != execution_operation_id
                || to_i64("task fence", attempt.lease.fence)? != fence
            {
                return Err(Error::Invalid(
                    "stored task attempt is not canonical".into(),
                ));
            }
            let task = read_task(connection, &task_id)?;
            if attempt.task_digest != task.digest()? {
                return Err(Error::Invalid(
                    "stored attempt task digest is inconsistent".into(),
                ));
            }
            let mut state = parse_state(&state)?;
            let effective_expires_at_ms = to_u64("task lease expiry", effective_expires_at)?;
            if state == AgentAttemptState::Leased && now >= effective_expires_at_ms {
                state = AgentAttemptState::Expired;
            }
            let view = TaskAttemptView {
                attempt,
                effective_expires_at_ms,
                state,
                assertion_digest,
                superseding_fence: superseding
                    .map(|value| to_u64("superseding task fence", value))
                    .transpose()?,
                admission_nonpassing: true,
                currently_applicable: true,
            };
            view.validate()?;
            if view.state == AgentAttemptState::Finished {
                read_assertion(connection, &task, &view)?;
            }
            Ok(view)
        },
    )
    .transpose()
}

fn latest_attempt_state(
    connection: &Connection,
    task_id: &str,
    now: u64,
) -> Result<Option<AgentAttemptState>> {
    let attempt: Option<String> = connection
        .query_row(
            "SELECT attempt_id FROM agent_workflow_attempts WHERE task_id=?1 ORDER BY fence DESC LIMIT 1",
            [task_id],
            |row| row.get(0),
        )
        .optional()?;
    attempt
        .map(|attempt| {
            read_attempt(connection, &attempt, now)?.ok_or_else(|| {
                Error::Invalid("latest task attempt disappeared during transaction".into())
            })
        })
        .transpose()
        .map(|value| value.map(|view| view.state))
}

fn operation_retry(
    connection: &Connection,
    agent: &AuthenticatedAgentIdentity,
    request_id: &str,
    kind: &str,
    digest: &str,
) -> Result<Option<TaskAttemptView>> {
    let Some((stored_kind, stored_digest, outcome)) =
        lookup_operation(connection, agent, request_id)?
    else {
        return Ok(None);
    };
    if stored_kind != kind || stored_digest != digest {
        return Err(Error::Conflict(
            "request ID is already bound to different authenticated bytes".into(),
        ));
    }
    let view = TaskAttemptView::from_slice(&outcome)?;
    let retained = read_attempt(connection, &view.attempt.attempt_id, 0)?
        .ok_or_else(|| Error::Invalid("operation outcome has no retained attempt".into()))?;
    if view.canonical_bytes()? != outcome
        || view.attempt != retained.attempt
        || view.attempt.agent != *agent
    {
        return Err(Error::Invalid(
            "operation outcome failed retained identity validation".into(),
        ));
    }
    Ok(Some(view))
}

fn lookup_operation(
    connection: &Connection,
    agent: &AuthenticatedAgentIdentity,
    request_id: &str,
) -> Result<Option<(String, String, Vec<u8>)>> {
    connection
        .query_row(
            "SELECT kind,request_digest,outcome_json FROM agent_workflow_operations WHERE owner_id=?1 AND agent_id=?2 AND request_id=?3",
            params![agent.owner_id, agent.agent_id, request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(Error::from)
}

fn store_operation(
    tx: &Transaction<'_>,
    agent: &AuthenticatedAgentIdentity,
    request_id: &str,
    kind: &str,
    digest: &str,
    view: &TaskAttemptView,
) -> Result<()> {
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM agent_workflow_operations",
        [],
        |row| row.get(0),
    )?;
    if count >= MAX_OPERATIONS {
        return Err(Error::Limit("agent operation inventory is full".into()));
    }
    tx.execute(
        "INSERT INTO agent_workflow_operations(owner_id,agent_id,request_id,kind,request_digest,outcome_json) VALUES(?1,?2,?3,?4,?5,?6)",
        params![agent.owner_id, agent.agent_id, request_id, kind, digest, view.canonical_bytes()?],
    )?;
    Ok(())
}

fn enforce_inventory(tx: &Transaction<'_>) -> Result<()> {
    for (table, limit, message) in [
        (
            "agent_workflow_tasks",
            MAX_TASKS,
            "agent task inventory is full",
        ),
        (
            "agent_workflow_attempts",
            MAX_ATTEMPTS,
            "agent attempt inventory is full",
        ),
        (
            "agent_workflow_operations",
            MAX_OPERATIONS,
            "agent operation inventory is full",
        ),
    ] {
        let count: i64 = tx.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        if count >= limit {
            return Err(Error::Limit(message.into()));
        }
    }
    Ok(())
}

fn verify_observation(assertion: &TaskResultAssertion, public_key_hex: &str) -> Result<()> {
    let provenance = assertion
        .executor_provenance
        .as_ref()
        .ok_or_else(|| Error::Invalid("registered observation is missing provenance".into()))?;
    let key_bytes = decode_hex::<32>(public_key_hex)?;
    if !crate::evidence::is_canonical_ed25519_encoding(&key_bytes) {
        return Err(Error::Invalid(
            "non-canonical enrolled executor public key".into(),
        ));
    }
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| Error::Invalid("invalid enrolled executor public key".into()))?;
    if key.is_weak() {
        return Err(Error::Invalid("weak enrolled executor public key".into()));
    }
    let digest = decode_hex::<32>(&provenance.observation_digest)?;
    let signature = Signature::from_bytes(&decode_hex::<64>(&provenance.signature_hex)?);
    key.verify_strict(&digest, &signature)
        .map_err(|_| Error::Invalid("invalid executor observation signature".into()))
}

fn confirmed_recovery(
    request: &TaskMutationRequest,
    view: &TaskAttemptView,
) -> Result<TaskRecoveryEnvelope> {
    let (disposition, confirmed_effects, unresolved_effects, next_action) = match request.action {
        TaskMutationAction::Heartbeat {} => (
            RecoveryDisposition::Confirmed,
            vec![ConfirmedEffect::LeaseRecorded {
                fence: request.fence,
            }],
            vec![],
            RecoveryNextAction::InspectStatus,
        ),
        TaskMutationAction::Reconcile {} => (
            RecoveryDisposition::UncertainEffect,
            vec![ConfirmedEffect::FenceSuperseded {
                old_fence: request.fence,
                superseding_fence: view.superseding_fence.ok_or_else(|| {
                    Error::Invalid("reconciled attempt is missing superseding fence".into())
                })?,
            }],
            vec![UnresolvedEffect::ExecutionMayStillRun {}],
            RecoveryNextAction::PrepareFreshAttempt,
        ),
        TaskMutationAction::Cancel { .. } => (
            RecoveryDisposition::UncertainEffect,
            vec![],
            vec![UnresolvedEffect::ExecutionMayStillRun {}],
            RecoveryNextAction::RequestAuthorizedReconciliation,
        ),
    };
    recovery_envelope(
        request,
        disposition,
        observed_state(view.state),
        confirmed_effects,
        unresolved_effects,
        next_action,
    )
}

fn rejected_recovery(
    request: &TaskMutationRequest,
    observed: AttemptObservedState,
) -> Result<TaskRecoveryEnvelope> {
    recovery_envelope(
        request,
        RecoveryDisposition::ConclusiveRejection,
        observed,
        vec![],
        vec![],
        RecoveryNextAction::InspectStatus,
    )
}

fn recovery_envelope(
    request: &TaskMutationRequest,
    disposition: RecoveryDisposition,
    observed_state: AttemptObservedState,
    confirmed_effects: Vec<ConfirmedEffect>,
    unresolved_effects: Vec<UnresolvedEffect>,
    next_action: RecoveryNextAction,
) -> Result<TaskRecoveryEnvelope> {
    let envelope = TaskRecoveryEnvelope {
        version: AGENT_TASK_CONTRACT_VERSION,
        request_id: request.request_id.clone(),
        execution_operation_id: request.execution_operation_id.clone(),
        task_id: request.task_id.clone(),
        task_digest: request.task_digest.clone(),
        attempt_id: request.attempt_id.clone(),
        attempt_digest: request.attempt_digest.clone(),
        fence: request.fence,
        disposition,
        observed_state,
        confirmed_effects,
        unresolved_effects,
        next_action,
    };
    TaskRecoveryEnvelope::from_slice(&serde_json::to_vec(&envelope)?)
}

fn observed_state(state: AgentAttemptState) -> AttemptObservedState {
    match state {
        AgentAttemptState::Leased => AttemptObservedState::Leased,
        AgentAttemptState::Expired => AttemptObservedState::Expired,
        AgentAttemptState::Finished => AttemptObservedState::Finished,
        AgentAttemptState::Cancelled => AttemptObservedState::Cancelled,
        AgentAttemptState::Replaced => AttemptObservedState::Replaced,
    }
}

fn mutation_kind(action: &TaskMutationAction) -> &'static str {
    match action {
        TaskMutationAction::Heartbeat {} => "heartbeat",
        TaskMutationAction::Cancel { .. } => "cancel",
        TaskMutationAction::Reconcile {} => "reconcile",
    }
}

fn parse_state(value: &str) -> Result<AgentAttemptState> {
    match value {
        "leased" => Ok(AgentAttemptState::Leased),
        "finished" => Ok(AgentAttemptState::Finished),
        "cancelled" => Ok(AgentAttemptState::Cancelled),
        "replaced" => Ok(AgentAttemptState::Replaced),
        _ => Err(Error::Invalid(
            "invalid stored agent task attempt state".into(),
        )),
    }
}

fn validate_identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return Err(Error::Invalid(
            "invalid agent task service identifier".into(),
        ));
    }
    Ok(())
}

fn validate_raw_digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid("invalid lowercase SHA-256 digest".into()));
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N]> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid("invalid lowercase hexadecimal bytes".into()));
    }
    let mut output = [0_u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Ok(output)
}

fn nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("hex input was validated"),
    }
}

fn to_i64(label: &str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Limit(format!("{label} does not fit SQLite")))
}

fn to_u64(label: &str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Invalid(format!("stored {label} is negative")))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use ed25519_dalek::{Signer, SigningKey};

    use super::*;
    use crate::authority::workflows::WorkflowRequirement;
    use crate::checks::SourcePathExpectation;
    use crate::context::{ChangeId, InputIdentity, REVIEW_CONTEXT_VERSION};
    use crate::policy::issuer_key_id;
    use crate::skill_workflow::{SkillReference, WorkflowDefinition, WorkflowSource};

    struct Fixture {
        root: tempfile::TempDir,
        context: ReviewContext,
        enrollment: ExecutorEnrollment,
        clock: AtomicU64,
        authorizations: AtomicUsize,
    }

    impl Fixture {
        fn new() -> Self {
            Self::configured(false, 0)
        }

        fn configured(two_steps: bool, resource_bytes: usize) -> Self {
            let root = tempfile::tempdir().unwrap();
            let engine = Engine::open(root.path()).unwrap();
            let revision = engine
                .checkpoint(
                    None,
                    &[
                        crate::model::FileChange {
                            path: "SKILL.md".into(),
                            content: Some(b"# Synthetic task".to_vec()),
                        },
                        crate::model::FileChange {
                            path: "resource.txt".into(),
                            content: Some(vec![b'x'; resource_bytes]),
                        },
                        crate::model::FileChange {
                            path: "src/file".into(),
                            content: Some(b"abc".to_vec()),
                        },
                    ],
                )
                .unwrap();
            let mut authority = Authority::open(root.path()).unwrap();
            let repository = authority.repository_id().unwrap();
            let mut definition = WorkflowDefinition {
                version: 1,
                name: "agent-tasks".into(),
                source: WorkflowSource {
                    repository: repository.clone(),
                    revision: revision.clone(),
                    policy_epoch: 0,
                },
                skill: SkillReference {
                    entrypoint: "SKILL.md".into(),
                    resources: if resource_bytes > 0 {
                        vec!["resource.txt".into()]
                    } else {
                        vec![]
                    },
                },
                steps: vec![StepDefinition {
                    id: "tests".into(),
                    needs: vec![],
                    required: true,
                    action: StepAction::SourcePaths {
                        expectations: vec![SourcePathExpectation {
                            path: "src/file".into(),
                            sha256: crate::store::digest(b"abc"),
                        }],
                    },
                }],
            };
            if two_steps {
                let mut next = definition.steps[0].clone();
                next.id = "dependent".into();
                next.needs = vec!["tests".into()];
                definition.steps.push(next);
            }
            let plan = compile(root.path(), &definition).unwrap();
            let requirement = WorkflowRequirement::from_plan("main", &plan).unwrap();
            authority
                .install_workflow_requirement(&requirement)
                .unwrap();
            let signing = SigningKey::from_bytes(&[7_u8; 32]);
            let public = signing.verifying_key().to_bytes();
            let public_hex = public
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let enrollment = ExecutorEnrollment {
                version: 1,
                registration_id: "registration-1".into(),
                executor_id: "executor-1".into(),
                operator_id: "operator-1".into(),
                repository: repository.clone(),
                step_ids: if two_steps {
                    vec!["dependent".into(), "tests".into()]
                } else {
                    vec!["tests".into()]
                },
                registration_epoch: 1,
                verification_key_id: issuer_key_id(&public_hex).unwrap(),
                verification_key_sha256: sha256(&public),
                signature_algorithm: ObservationSignatureAlgorithm::Ed25519,
                issued_at_ms: 1,
            };
            authority
                .enroll_workflow_executor(&enrollment, &public_hex)
                .unwrap();
            let context = ReviewContext {
                version: REVIEW_CONTEXT_VERSION,
                change: ChangeId {
                    repository,
                    id: "change-1".into(),
                },
                change_version: 1,
                revision,
                base: None,
                target: "main".into(),
                policy_epoch: authority.policy_epoch().unwrap(),
                intent: "exercise agent task state".into(),
                acceptance_criteria: vec!["task state remains fenced".into()],
                scope: vec!["src/file".into()],
                exclusions: vec![],
                omissions: vec![],
                dependencies: vec![],
                recipe: InputIdentity::Digest {
                    sha256: "1".repeat(64),
                },
                toolchain: InputIdentity::Digest {
                    sha256: "2".repeat(64),
                },
                environment: InputIdentity::Digest {
                    sha256: "3".repeat(64),
                },
                required_evidence: vec![],
                required_capabilities: vec!["agentscm.review-context/v1".into()],
            };
            authority.start_change(&context).unwrap();
            drop(authority);
            Self {
                root,
                context,
                enrollment,
                clock: AtomicU64::new(1_000),
                authorizations: AtomicUsize::new(0),
            }
        }

        fn service(&self) -> AgentTaskService<'_> {
            AgentTaskService::new(self.root.path(), "trusted-local", self, self).unwrap()
        }

        fn agent(&self, id: &str) -> AuthenticatedAgentIdentity {
            AuthenticatedAgentIdentity {
                owner_id: "owner-1".into(),
                agent_id: id.into(),
            }
        }

        fn claim(
            &self,
            task: &WorkflowTask,
            agent: AuthenticatedAgentIdentity,
        ) -> TaskClaimRequest {
            TaskClaimRequest {
                version: 1,
                request_id: format!("claim-{}", agent.agent_id),
                execution_operation_id: format!("execute-{}", agent.agent_id),
                task_id: task.task_id.clone(),
                task_digest: task.digest().unwrap(),
                attempt_id: format!("attempt-{}", agent.agent_id),
                agent,
                executor: self.enrollment.binding().unwrap(),
                requested_lease_ms: 100,
            }
        }
    }

    impl TaskClock for Fixture {
        fn now_ms(&self) -> Result<u64> {
            Ok(self.clock.load(Ordering::SeqCst))
        }
    }

    impl TaskAuthorizationGuard for Fixture {
        fn authorize(&self, _: &AuthenticatedAgentIdentity, _: &str) -> Result<()> {
            self.authorizations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn mutation(
        view: &TaskAttemptView,
        request_id: &str,
        action: TaskMutationAction,
    ) -> TaskMutationRequest {
        TaskMutationRequest {
            version: 1,
            request_id: request_id.into(),
            execution_operation_id: view.attempt.execution_operation_id.clone(),
            task_id: view.attempt.task_id.clone(),
            task_digest: view.attempt.task_digest.clone(),
            attempt_id: view.attempt.attempt_id.clone(),
            attempt_digest: view.attempt.digest().unwrap(),
            fence: view.attempt.lease.fence,
            agent: view.attempt.agent.clone(),
            executor: view.attempt.executor.clone(),
            action,
        }
    }

    fn status(view: &TaskAttemptView) -> TaskStatusRequest {
        TaskStatusRequest {
            version: 1,
            task_id: view.attempt.task_id.clone(),
            task_digest: view.attempt.task_digest.clone(),
            attempt_id: view.attempt.attempt_id.clone(),
            attempt_digest: view.attempt.digest().unwrap(),
            fence: view.attempt.lease.fence,
            agent: view.attempt.agent.clone(),
            executor: view.attempt.executor.clone(),
        }
    }

    fn signed_result(task: &WorkflowTask, view: &TaskAttemptView) -> TaskResultAssertion {
        let mut assertion = TaskResultAssertion {
            version: 1,
            task_id: task.task_id.clone(),
            task_digest: task.digest().unwrap(),
            attempt_id: view.attempt.attempt_id.clone(),
            attempt_digest: view.attempt.digest().unwrap(),
            request_id: "result-1".into(),
            execution_operation_id: view.attempt.execution_operation_id.clone(),
            agent: view.attempt.agent.clone(),
            executor: view.attempt.executor.clone(),
            fence: view.attempt.lease.fence,
            trust_class: EvidenceTrustClass::RegisteredExecutorObservation,
            outcome: TaskOutcome::Passed,
            criteria: vec![CriterionResult {
                criterion: task.entrypoint.required_criteria[0].id.clone(),
                outcome: CriterionOutcome::Passed,
                findings: vec![],
                evidence_digests: vec!["a".repeat(64)],
            }],
            executor_provenance: None,
        };
        let observation_digest = assertion.observation_digest().unwrap();
        let signature = SigningKey::from_bytes(&[7_u8; 32])
            .sign(&decode_hex::<32>(&observation_digest).unwrap());
        assertion.executor_provenance = Some(ExecutorObservationProvenance {
            registration_id: assertion.executor.registration_id.clone(),
            registration_epoch: assertion.executor.registration_epoch,
            verification_key_id: assertion.executor.verification_key_id.clone(),
            observation_digest,
            signature_algorithm: ObservationSignatureAlgorithm::Ed25519,
            signature_hex: signature
                .to_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        });
        assertion
    }

    #[test]
    fn status_and_reference_reads_do_not_initialize_the_journal() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .requirements(&fixture.context, &fixture.agent("agent-1"))
            .unwrap()
            .remove(0);
        assert!(!fixture.root.path().join("skill-workflows").exists());
        let request = ReferenceFetchRequest {
            version: 1,
            task_id: task.task_id.clone(),
            task_digest: task.digest().unwrap(),
            reference_name: "entrypoint".into(),
            expected_sha256: task.entrypoint.references[0].sha256.clone(),
            expected_schema_version: 1,
            actor: fixture.agent("agent-1"),
        };
        assert!(matches!(
            service.reference(&fixture.context, &request).unwrap(),
            ReferenceFetchResponse::Available { .. }
        ));
        let absent = TaskStatusRequest {
            version: 1,
            task_id: task.task_id.clone(),
            task_digest: task.digest().unwrap(),
            attempt_id: "absent-attempt".into(),
            attempt_digest: "a".repeat(64),
            fence: 1,
            agent: fixture.agent("agent-1"),
            executor: fixture.enrollment.binding().unwrap(),
        };
        assert_eq!(service.status(&fixture.context, &absent).unwrap(), None);
        assert!(!fixture.root.path().join("skill-workflows").exists());
    }

    #[test]
    fn claims_serialize_per_task_and_idempotency_is_per_exact_agent() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .next_task(&fixture.context, &fixture.agent("agent-1"))
            .unwrap()
            .unwrap();
        let claim = fixture.claim(&task, fixture.agent("agent-1"));
        let first = service.claim(&fixture.context, &claim).unwrap();
        assert_eq!(service.claim(&fixture.context, &claim).unwrap(), first);
        let competing = fixture.claim(&task, fixture.agent("agent-2"));
        assert!(matches!(
            service.claim(&fixture.context, &competing),
            Err(Error::Conflict(_))
        ));
        assert_eq!(
            service.status(&fixture.context, &status(&first)).unwrap(),
            Some(first)
        );
    }

    #[test]
    fn heartbeat_extends_effective_lease_without_changing_attempt_identity() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .next_task(&fixture.context, &fixture.agent("agent-1"))
            .unwrap()
            .unwrap();
        let claimed = service
            .claim(
                &fixture.context,
                &fixture.claim(&task, fixture.agent("agent-1")),
            )
            .unwrap();
        fixture.clock.store(1_050, Ordering::SeqCst);
        let heartbeat = mutation(&claimed, "heartbeat-1", TaskMutationAction::Heartbeat {});
        let extended = service.heartbeat(&fixture.context, &heartbeat).unwrap();
        assert_eq!(extended.attempt, claimed.attempt);
        assert!(extended.effective_expires_at_ms > claimed.effective_expires_at_ms);
        assert_eq!(
            service.heartbeat(&fixture.context, &heartbeat).unwrap(),
            extended
        );
        assert_eq!(
            service.status(&fixture.context, &status(&claimed)).unwrap(),
            Some(extended)
        );
        let recovery = service.recovery(&fixture.context, &heartbeat).unwrap();
        assert_eq!(recovery.disposition, RecoveryDisposition::Confirmed);
        assert_eq!(recovery.observed_state, AttemptObservedState::Leased);
    }

    #[test]
    fn changed_action_same_request_conflicts_and_reconciliation_fences_late_work() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .next_task(&fixture.context, &fixture.agent("agent-1"))
            .unwrap()
            .unwrap();
        let claimed = service
            .claim(
                &fixture.context,
                &fixture.claim(&task, fixture.agent("agent-1")),
            )
            .unwrap();
        let cancel = mutation(
            &claimed,
            "mutation-1",
            TaskMutationAction::Cancel {
                reason: "operator stop".into(),
            },
        );
        let cancelled = service.cancel_attempt(&fixture.context, &cancel).unwrap();
        assert_eq!(cancelled.state, AgentAttemptState::Cancelled);
        assert_eq!(
            service.cancel_attempt(&fixture.context, &cancel).unwrap(),
            cancelled
        );
        let changed_action = mutation(&claimed, "mutation-1", TaskMutationAction::Reconcile {});
        assert!(matches!(
            service.reconcile_attempt(&fixture.context, &changed_action),
            Err(Error::Conflict(_))
        ));
        let reconcile = mutation(&claimed, "reconcile-1", TaskMutationAction::Reconcile {});
        let replaced = service
            .reconcile_attempt(&fixture.context, &reconcile)
            .unwrap();
        assert_eq!(replaced.state, AgentAttemptState::Replaced);
        assert!(replaced.superseding_fence > Some(claimed.attempt.lease.fence));
        let late = mutation(&claimed, "late-heartbeat", TaskMutationAction::Heartbeat {});
        assert!(matches!(
            service.heartbeat(&fixture.context, &late),
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            service.submit_result(&fixture.context, &signed_result(&task, &claimed)),
            Err(Error::Conflict(_))
        ));
        let restarted = fixture.service();
        assert_eq!(
            restarted
                .status(&fixture.context, &status(&claimed))
                .unwrap(),
            Some(replaced)
        );
        let count: i64 = Journal::open(fixture.root.path(), OpenMode::Read)
            .unwrap()
            .connection
            .query_row("SELECT count(*) FROM agent_workflow_attempts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1, "restart must not replay uncertain execution");
    }

    #[test]
    fn expiry_is_read_only_and_requires_reconciliation_before_replacement() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .next_task(&fixture.context, &fixture.agent("agent-1"))
            .unwrap()
            .unwrap();
        let claim = fixture.claim(&task, fixture.agent("agent-1"));
        let view = service.claim(&fixture.context, &claim).unwrap();
        fixture.clock.store(1_101, Ordering::SeqCst);
        let expired = service
            .status(&fixture.context, &status(&view))
            .unwrap()
            .unwrap();
        assert_eq!(expired.state, AgentAttemptState::Expired);
        let retry = service.claim(&fixture.context, &claim).unwrap();
        assert_eq!(retry.state, AgentAttemptState::Leased);
        assert!(
            !retry.currently_applicable,
            "retained lease must not be mistaken for live authority"
        );
        assert!(matches!(
            service.claim(
                &fixture.context,
                &fixture.claim(&task, fixture.agent("agent-2"))
            ),
            Err(Error::Conflict(_))
        ));
        let stored: String = Journal::open(fixture.root.path(), OpenMode::Read)
            .unwrap()
            .connection
            .query_row(
                "SELECT state FROM agent_workflow_attempts WHERE attempt_id=?1",
                [&view.attempt.attempt_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "leased", "status must not mark remote work dead");
    }

    #[test]
    fn signed_observation_is_bound_and_retained_without_minting_a_receipt() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .next_task(&fixture.context, &fixture.agent("agent-1"))
            .unwrap()
            .unwrap();
        let claimed = service
            .claim(
                &fixture.context,
                &fixture.claim(&task, fixture.agent("agent-1")),
            )
            .unwrap();
        let mut forged = signed_result(&task, &claimed);
        forged.executor_provenance.as_mut().unwrap().signature_hex = "0".repeat(128);
        assert!(matches!(
            service.submit_result(&fixture.context, &forged),
            Err(Error::Invalid(_))
        ));

        let assertion = signed_result(&task, &claimed);
        let finished = service.submit_result(&fixture.context, &assertion).unwrap();
        assert_eq!(finished.state, AgentAttemptState::Finished);
        assert!(finished.admission_nonpassing);
        assert_eq!(
            service.submit_result(&fixture.context, &assertion).unwrap(),
            finished
        );
        let authority_db = Connection::open(fixture.root.path().join("state.sqlite")).unwrap();
        let receipts: i64 = authority_db
            .query_row("SELECT count(*) FROM workflow_receipts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(receipts, 0);
    }

    #[test]
    fn agent_assertion_never_completes_a_logical_requirement() {
        let fixture = Fixture::configured(true, 0);
        let service = fixture.service();
        let task = service
            .next_task(&fixture.context, &fixture.agent("agent-1"))
            .unwrap()
            .unwrap();
        let claimed = service
            .claim(
                &fixture.context,
                &fixture.claim(&task, fixture.agent("agent-1")),
            )
            .unwrap();
        let mut assertion = signed_result(&task, &claimed);
        assertion.trust_class = EvidenceTrustClass::AgentAssertion;
        assertion.executor_provenance = None;
        service.submit_result(&fixture.context, &assertion).unwrap();
        assert_eq!(
            service.read_task_states(&[task]).unwrap(),
            vec![LogicalState::TerminalNonpassing]
        );
        let tasks = service
            .requirements(&fixture.context, &fixture.agent("agent-2"))
            .unwrap();
        assert!(matches!(
            service.claim(
                &fixture.context,
                &fixture.claim(&tasks[1], fixture.agent("agent-2"))
            ),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn current_policy_drift_refuses_an_old_logical_task() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .next_task(&fixture.context, &fixture.agent("agent-1"))
            .unwrap()
            .unwrap();
        let mut authority = Authority::open(fixture.root.path()).unwrap();
        authority
            .advance_policy(fixture.context.policy_epoch)
            .unwrap();
        drop(authority);
        assert!(matches!(
            service.claim(
                &fixture.context,
                &fixture.claim(&task, fixture.agent("agent-1"))
            ),
            Err(Error::Conflict(message)) if message.contains("stale")
        ));
        assert!(!fixture.root.path().join("skill-workflows").exists());
    }
    #[test]
    fn dependencies_are_checked_inside_claim_and_controller_steps_are_refused() {
        let fixture = Fixture::configured(true, 0);
        let service = fixture.service();
        let tasks = service
            .requirements(&fixture.context, &fixture.agent("a"))
            .unwrap();
        let first = &tasks[0];
        let second = &tasks[1];
        assert!(matches!(
            service.claim(&fixture.context, &fixture.claim(second, fixture.agent("b"))),
            Err(Error::Conflict(_))
        ));
        let journal = Journal::open(fixture.root.path(), OpenMode::Read).unwrap();
        for table in [
            "agent_workflow_tasks",
            "agent_workflow_attempts",
            "agent_workflow_operations",
        ] {
            assert_eq!(
                journal
                    .connection
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        drop(journal);
        let claim = fixture.claim(first, fixture.agent("a"));
        let view = service.claim(&fixture.context, &claim).unwrap();
        service
            .submit_result(&fixture.context, &signed_result(first, &view))
            .unwrap();
        assert_eq!(
            service.claim(&fixture.context, &claim).unwrap().attempt,
            view.attempt
        );
        std::thread::scope(|scope| {
            let handles: Vec<_> = ["b", "c"]
                .into_iter()
                .map(|id| {
                    let fixture = &fixture;
                    scope.spawn(move || {
                        fixture
                            .service()
                            .claim(&fixture.context, &fixture.claim(second, fixture.agent(id)))
                    })
                })
                .collect();
            let results: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect();
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            assert_eq!(
                results
                    .iter()
                    .filter(|result| matches!(result, Err(Error::Conflict(_))))
                    .count(),
                1
            );
        });

        let fixture = Fixture::configured(true, 0);
        let service = fixture.service();
        let mut current = service.current(&fixture.context).unwrap();
        let controller = StepDefinition {
            id: "tests".into(),
            needs: vec![],
            required: true,
            action: StepAction::ClaudeCodeReview {
                paths: vec!["src/file".into()],
                model: "sonnet".into(),
            },
        };
        current.tasks[0].entrypoint.required_criteria = criteria(&controller);
        let claim = fixture.claim(&current.tasks[0], fixture.agent("a"));
        assert!(matches!(
            service.claim_current(&fixture.context, &claim, &current),
            Err(Error::Conflict(_))
        ));
        // Existing task-aware validation already rejects signed Passed for an all-controller
        // task. Keep this regression anchored in that behavior, not a purported bypass.
        let attempt = WorkflowTaskAttempt {
            version: 1,
            claim_request_id: claim.request_id.clone(),
            task_id: claim.task_id.clone(),
            task_digest: claim.task_digest.clone(),
            attempt_id: claim.attempt_id.clone(),
            execution_operation_id: claim.execution_operation_id.clone(),
            agent: claim.agent.clone(),
            executor: claim.executor.clone(),
            lease: AttemptLease {
                fence: 1,
                issued_at_ms: 1000,
                expires_at_ms: 1100,
            },
        };
        let view = TaskAttemptView {
            attempt,
            effective_expires_at_ms: 1100,
            state: AgentAttemptState::Leased,
            assertion_digest: None,
            superseding_fence: None,
            admission_nonpassing: true,
            currently_applicable: true,
        };
        let assertion = signed_result(&current.tasks[0], &view);
        assert!(
            assertion
                .validate_for_task_attempt(&current.tasks[0], &view.attempt)
                .is_err()
        );
        assert!(matches!(
            service.submit_current_result(&fixture.context, &assertion, &current),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn stale_attempts_remain_inspectable_and_must_be_reconciled_before_new_subject() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .requirements(&fixture.context, &fixture.agent("a"))
            .unwrap()
            .remove(0);
        let claim = fixture.claim(&task, fixture.agent("a"));
        let view = service.claim(&fixture.context, &claim).unwrap();
        fixture.clock.store(1050, Ordering::SeqCst);
        let heartbeat = mutation(&view, "heartbeat", TaskMutationAction::Heartbeat {});
        let extended = service.heartbeat(&fixture.context, &heartbeat).unwrap();
        let mut authority = Authority::open(fixture.root.path()).unwrap();
        let mut next_context = fixture.context.clone();
        next_context.change_version += 1;
        next_context.revision = Engine::open(fixture.root.path())
            .unwrap()
            .checkpoint(
                Some(&fixture.context.revision),
                &[crate::model::FileChange {
                    path: "new.txt".into(),
                    content: Some(b"new".to_vec()),
                }],
            )
            .unwrap();
        authority.update_change_context(1, &next_context).unwrap();
        let observed = service
            .status(&fixture.context, &status(&view))
            .unwrap()
            .unwrap();
        assert!(!observed.currently_applicable);
        assert_eq!(observed.attempt, view.attempt);
        let retry = service.heartbeat(&fixture.context, &heartbeat).unwrap();
        assert!(!retry.currently_applicable);
        assert_eq!(
            retry.effective_expires_at_ms,
            extended.effective_expires_at_ms
        );
        assert!(
            !service
                .claim(&fixture.context, &claim)
                .unwrap()
                .currently_applicable
        );
        assert_eq!(
            service
                .recovery(&fixture.context, &heartbeat)
                .unwrap()
                .disposition,
            RecoveryDisposition::Confirmed
        );
        assert!(matches!(
            service.heartbeat(
                &fixture.context,
                &mutation(&view, "late", TaskMutationAction::Heartbeat {})
            ),
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            service.submit_result(&fixture.context, &signed_result(&task, &view)),
            Err(Error::Conflict(_))
        ));
        let next_task = service
            .requirements(&next_context, &fixture.agent("b"))
            .unwrap()
            .remove(0);
        let next_claim = fixture.claim(&next_task, fixture.agent("b"));
        assert_ne!(task.task_id, next_task.task_id);
        assert!(matches!(
            service.claim(&next_context, &next_claim),
            Err(Error::Conflict(_))
        ));
        let reconcile = mutation(&view, "reconcile", TaskMutationAction::Reconcile {});
        let replaced = service
            .reconcile_attempt(&fixture.context, &reconcile)
            .unwrap();
        assert!(!replaced.currently_applicable);
        assert_eq!(
            service
                .recovery(&fixture.context, &reconcile)
                .unwrap()
                .unresolved_effects,
            vec![UnresolvedEffect::ExecutionMayStillRun {}]
        );
        let mut reused = next_claim.clone();
        reused.execution_operation_id = view.attempt.execution_operation_id.clone();
        assert!(matches!(
            service.claim(&next_context, &reused),
            Err(Error::Conflict(_))
        ));
        service.claim(&next_context, &next_claim).unwrap();
    }

    #[test]
    fn revoked_executor_allows_history_and_cleanup_but_current_actor_revocation_denies_history() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .requirements(&fixture.context, &fixture.agent("a"))
            .unwrap()
            .remove(0);
        let claim = fixture.claim(&task, fixture.agent("a"));
        let view = service.claim(&fixture.context, &claim).unwrap();
        let assertion = signed_result(&task, &view);
        service.submit_result(&fixture.context, &assertion).unwrap();
        let mut authority = Authority::open(fixture.root.path()).unwrap();
        authority
            .revoke_workflow_executor(&ExecutorRevocation {
                version: 1,
                repository: fixture.context.change.repository.clone(),
                registration_id: fixture.enrollment.registration_id.clone(),
                registration_epoch: 1,
                revoked_at_policy_epoch: fixture.context.policy_epoch + 1,
                reason: "synthetic revocation".into(),
            })
            .unwrap();
        assert!(
            !service
                .submit_result(&fixture.context, &assertion)
                .unwrap()
                .currently_applicable
        );
        assert!(
            !service
                .status(&fixture.context, &status(&view))
                .unwrap()
                .unwrap()
                .currently_applicable
        );
        struct Denied;
        impl TaskAuthorizationGuard for Denied {
            fn authorize(&self, _: &AuthenticatedAgentIdentity, _: &str) -> Result<()> {
                Err(Error::Conflict("actor revoked".into()))
            }
        }
        let denied =
            AgentTaskService::new(fixture.root.path(), "trusted-local", &fixture, &Denied).unwrap();
        assert!(denied.status(&fixture.context, &status(&view)).is_err());
        assert!(denied.claim(&fixture.context, &claim).is_err());
    }

    #[test]
    fn reference_limit_is_typed_at_the_exact_boundary() {
        for length in [MAX_REFERENCE_BYTES, MAX_REFERENCE_BYTES + 1] {
            let fixture = Fixture::configured(false, length);
            let service = fixture.service();
            let task = service
                .requirements(&fixture.context, &fixture.agent("a"))
                .unwrap()
                .remove(0);
            let reference = task
                .entrypoint
                .references
                .iter()
                .find(|r| r.path == "resource.txt")
                .unwrap();
            let request = ReferenceFetchRequest {
                version: 1,
                task_id: task.task_id.clone(),
                task_digest: task.digest().unwrap(),
                reference_name: reference.name.clone(),
                expected_sha256: reference.sha256.clone(),
                expected_schema_version: 1,
                actor: fixture.agent("a"),
            };
            let response = service.reference(&fixture.context, &request).unwrap();
            if length > MAX_REFERENCE_BYTES {
                let engine = Engine::open(fixture.root.path()).unwrap();
                let (bounded, budget) = engine.with_read_budget(64 * 1024);
                assert_eq!(
                    bounded
                        .read_regular_file_bounded(
                            &task.subject.revision,
                            "resource.txt",
                            MAX_REFERENCE_BYTES
                        )
                        .unwrap(),
                    None
                );
                budget.finish(Ok(())).unwrap();
                assert!(matches!(response, ReferenceFetchResponse::Limit { .. }));
            } else {
                assert!(
                    matches!(response, ReferenceFetchResponse::Available { content, .. } if content.len() == length)
                );
            }
        }
    }

    #[test]
    fn retained_assertions_cannot_be_spliced_between_attempts() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .requirements(&fixture.context, &fixture.agent("a"))
            .unwrap()
            .remove(0);
        let view = service
            .claim(&fixture.context, &fixture.claim(&task, fixture.agent("a")))
            .unwrap();
        let mut assertion = signed_result(&task, &view);
        // Well-formed canonical bytes and digest, but a different task identity.
        assertion.task_id = "different-task".into();
        assertion.executor_provenance = None;
        assertion.trust_class = EvidenceTrustClass::AgentAssertion;
        let digest = assertion.digest().unwrap();
        let journal = Journal::open(fixture.root.path(), OpenMode::Create).unwrap();
        journal
            .connection
            .execute(
                "INSERT INTO agent_workflow_assertions VALUES(?1,?2,?3)",
                params![
                    digest,
                    view.attempt.attempt_id,
                    assertion.canonical_bytes().unwrap()
                ],
            )
            .unwrap();
        journal
            .connection
            .execute(
                "UPDATE agent_workflow_attempts SET state='finished',assertion_digest=?1",
                [&digest],
            )
            .unwrap();
        drop(journal);
        assert!(matches!(
            service.read_task_states(&[task]),
            Err(Error::Invalid(_))
        ));
    }
    #[test]
    fn immediately_previous_schemas_preserve_existing_workflow_and_authority_records() {
        let fixture = Fixture::new();
        let plan = fixture.service().current(&fixture.context).unwrap().plan;
        let before = super::super::start(fixture.root.path(), "prior-run", &plan).unwrap();
        let journal = Journal::open(fixture.root.path(), OpenMode::Create).unwrap();
        journal.connection.execute_batch("DROP TABLE agent_workflow_operations; DROP TABLE agent_workflow_assertions; DROP TABLE agent_workflow_attempts; DROP TABLE agent_workflow_tasks; PRAGMA user_version=2;").unwrap();
        drop(journal);
        let journal = Journal::open(fixture.root.path(), OpenMode::Create).unwrap();
        assert_eq!(
            journal
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(journal.view("prior-run").unwrap(), before);
        drop(journal);
        let authority = Authority::open_read_only(fixture.root.path()).unwrap();
        let requirement = authority.workflow_requirement("main").unwrap();
        let change = authority.change(&fixture.context.change).unwrap();
        drop(authority);
        let db = Connection::open(fixture.root.path().join("state.sqlite")).unwrap();
        db.execute_batch("DROP TABLE workflow_executor_revocations; DROP TABLE workflow_executor_enrollments; PRAGMA user_version=7;").unwrap();
        drop(db);
        let authority = Authority::open(fixture.root.path()).unwrap();
        assert_eq!(authority.workflow_requirement("main").unwrap(), requirement);
        assert_eq!(authority.change(&fixture.context.change).unwrap(), change);
        assert_eq!(
            authority
                .review_context(&fixture.context.digest().unwrap())
                .unwrap(),
            fixture.context
        );
        assert_eq!(
            authority.policy_epoch().unwrap(),
            fixture.context.policy_epoch
        );
        let db = Connection::open(fixture.root.path().join("state.sqlite")).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            8
        );
    }

    #[test]
    fn policy_or_revocation_between_derivation_and_fence_rolls_back_claim() {
        struct DriftClock<'a> {
            fixture: &'a Fixture,
            revoke: bool,
        }
        impl TaskClock for DriftClock<'_> {
            fn now_ms(&self) -> Result<u64> {
                let mut authority = Authority::open(self.fixture.root.path())?;
                let epoch = authority.policy_epoch()?;
                if self.revoke {
                    authority.revoke_workflow_executor(&ExecutorRevocation {
                        version: 1,
                        repository: self.fixture.context.change.repository.clone(),
                        registration_id: self.fixture.enrollment.registration_id.clone(),
                        registration_epoch: 1,
                        revoked_at_policy_epoch: epoch + 1,
                        reason: "synthetic race".into(),
                    })?;
                } else {
                    authority.advance_policy(epoch)?;
                }
                Ok(1000)
            }
        }
        for revoke in [false, true] {
            let fixture = Fixture::new();
            let task = fixture
                .service()
                .requirements(&fixture.context, &fixture.agent("a"))
                .unwrap()
                .remove(0);
            let clock = DriftClock {
                fixture: &fixture,
                revoke,
            };
            let service =
                AgentTaskService::new(fixture.root.path(), "trusted-local", &clock, &fixture)
                    .unwrap();
            assert!(matches!(
                service.claim(&fixture.context, &fixture.claim(&task, fixture.agent("a"))),
                Err(Error::Conflict(_))
            ));
            let journal = Journal::open(fixture.root.path(), OpenMode::Read).unwrap();
            assert_eq!(
                journal
                    .connection
                    .query_row("SELECT count(*) FROM agent_workflow_attempts", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap(),
                0
            );
            assert_eq!(
                journal
                    .connection
                    .query_row(
                        "SELECT count(*) FROM agent_workflow_operations",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn authorization_callback_is_entered_before_the_journal_lease() {
        struct Guard<'a>(&'a Fixture);
        impl TaskAuthorizationGuard for Guard<'_> {
            fn authorize(&self, _: &AuthenticatedAgentIdentity, _: &str) -> Result<()> {
                Ok(())
            }
            fn with_current_authorization(
                &self,
                _: &AuthenticatedAgentIdentity,
                _: &str,
                transition: &mut dyn FnMut() -> Result<()>,
            ) -> Result<()> {
                // This would fail if the service already held the journal lease while
                // acquiring workspace authorization, the inverse of workspace finalization.
                drop(Journal::open(self.0.root.path(), OpenMode::Create)?);
                transition()
            }
        }
        let fixture = Fixture::new();
        let task = fixture
            .service()
            .requirements(&fixture.context, &fixture.agent("a"))
            .unwrap()
            .remove(0);
        let guard = Guard(&fixture);
        let service =
            AgentTaskService::new(fixture.root.path(), "trusted-local", &fixture, &guard).unwrap();
        service
            .claim(&fixture.context, &fixture.claim(&task, fixture.agent("a")))
            .unwrap();
    }

    #[test]
    fn revoked_executor_does_not_prevent_unfinished_attempt_cleanup() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let task = service
            .requirements(&fixture.context, &fixture.agent("a"))
            .unwrap()
            .remove(0);
        let view = service
            .claim(&fixture.context, &fixture.claim(&task, fixture.agent("a")))
            .unwrap();
        let mut authority = Authority::open(fixture.root.path()).unwrap();
        authority
            .revoke_workflow_executor(&ExecutorRevocation {
                version: 1,
                repository: fixture.context.change.repository.clone(),
                registration_id: fixture.enrollment.registration_id.clone(),
                registration_epoch: 1,
                revoked_at_policy_epoch: fixture.context.policy_epoch + 1,
                reason: "synthetic revocation".into(),
            })
            .unwrap();
        let cancel = mutation(
            &view,
            "cancel",
            TaskMutationAction::Cancel {
                reason: "stop".into(),
            },
        );
        assert!(
            !service
                .cancel_attempt(&fixture.context, &cancel)
                .unwrap()
                .currently_applicable
        );
        let reconcile = mutation(&view, "reconcile", TaskMutationAction::Reconcile {});
        assert_eq!(
            service
                .reconcile_attempt(&fixture.context, &reconcile)
                .unwrap()
                .state,
            AgentAttemptState::Replaced
        );
    }
    #[test]
    fn native_writer_remains_fenced_through_actual_journal_commit() {
        let fixture = Fixture::new();
        let task = fixture
            .service()
            .requirements(&fixture.context, &fixture.agent("a"))
            .unwrap()
            .remove(0);
        let authority = Authority::open(fixture.root.path()).unwrap();
        let mut journal = Journal::open(fixture.root.path(), OpenMode::Create).unwrap();
        let tx = journal
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        store_task(&tx, &task).unwrap();
        let competing = Connection::open(fixture.root.path().join("state.sqlite")).unwrap();
        competing.busy_timeout(std::time::Duration::ZERO).unwrap();
        authority.with_task_writer_fence(|held| {
            held.check_task_subject(&fixture.context, &task.subject, &fixture.enrollment.binding()?, false)?;
            tx.commit()?;
            // A separate native connection still cannot mutate policy after the journal
            // commit until this fence releases; it is not merely a final unheld read.
            let error = std::thread::spawn(move || competing.execute("UPDATE authority_state SET policy_epoch=policy_epoch+1 WHERE singleton=1", []).unwrap_err()).join().unwrap();
            assert!(matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::DatabaseBusy));
            Ok(())
        }).unwrap();
        assert_eq!(read_task(&journal.connection, &task.task_id).unwrap(), task);
        drop(journal);
        let mut writer = Authority::open(fixture.root.path()).unwrap();
        writer.advance_policy(fixture.context.policy_epoch).unwrap();
    }
}
