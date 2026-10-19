//! Versioned wire contracts for agent-owned execution of existing workflow steps.
//!
//! These types add no endpoint, scheduler, enrollment store, signature verifier or receipt path.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::context::ChangeId;
use crate::{Error, Result};

pub const AGENT_TASK_CONTRACT_VERSION: u32 = 1;
pub const AGENT_TASK_CAPABILITY: &str = "agentscm.agent-workflow-tasks/v1";

const MAX_WIRE_BYTES: usize = 256 * 1024;
const MAX_ID_BYTES: usize = 128;
const MAX_TEXT_BYTES: usize = 2 * 1024;
const MAX_REFERENCE_BYTES: usize = 128 * 1024;
const MAX_REFERENCE_ENVELOPE_BYTES: usize = MAX_REFERENCE_BYTES + 16 * 1024;
const MAX_LEASE_MS: u64 = 24 * 60 * 60 * 1_000;
const TASK_DOMAIN: &[u8] = b"agentscm.agent-workflow-task.v1\0";
const ATTEMPT_DOMAIN: &[u8] = b"agentscm.agent-workflow-attempt.v1\0";
const ASSERTION_DOMAIN: &[u8] = b"agentscm.agent-workflow-assertion.v1\0";
const OBSERVATION_DOMAIN: &[u8] = b"agentscm.agent-workflow-observation.v1\0";
const ENROLLMENT_DOMAIN: &[u8] = b"agentscm.agent-workflow-enrollment.v1\0";
const REVOCATION_DOMAIN: &[u8] = b"agentscm.agent-workflow-revocation.v1\0";
const CLAIM_DOMAIN: &[u8] = b"agentscm.agent-workflow-claim.v1\0";
const HEARTBEAT_DOMAIN: &[u8] = b"agentscm.agent-workflow-heartbeat.v1\0";
const CANCEL_DOMAIN: &[u8] = b"agentscm.agent-workflow-cancel.v1\0";
const RECONCILE_DOMAIN: &[u8] = b"agentscm.agent-workflow-reconcile.v1\0";
const RESULT_DOMAIN: &[u8] = b"agentscm.agent-workflow-result.v1\0";
const REFERENCE_FETCH_DOMAIN: &[u8] = b"agentscm.agent-workflow-reference-fetch.v1\0";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTaskSubject {
    pub repository: String,
    pub profile: String,
    pub target: String,
    pub revision: String,
    pub change: ChangeId,
    pub change_version: u64,
    pub context_digest: String,
    pub policy_epoch: u64,
    pub requirement_digest: String,
    pub plan_digest: String,
    pub skill_digest: String,
    pub step_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTask {
    pub version: u32,
    pub task_id: String,
    pub subject: WorkflowTaskSubject,
    pub required: bool,
    pub entrypoint: InstructionEntrypoint,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstructionEntrypoint {
    pub capability: String,
    pub schema_version: u32,
    pub workflow_digest: String,
    pub policy_digest: String,
    pub required_criteria: Vec<RequiredCriterion>,
    pub references: Vec<NamedInstructionReference>,
    pub permitted_progression: EntrypointProgression,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequiredCriterion {
    pub id: String,
    pub description: String,
    pub kind: CriterionKind,
    pub evidence_rule: CriterionEvidenceRule,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CriterionKind {
    Check,
    IndependentReview,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CriterionEvidenceRule {
    EnrolledLaptopOrController,
    ControllerOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NamedInstructionReference {
    pub name: String,
    pub sha256: String,
    pub path: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntrypointProgression {
    InspectTask,
    ClaimTask,
    InspectStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionEntrypointResponse {
    Available {
        entrypoint: InstructionEntrypoint,
    },
    SchemaMismatch {
        capability: String,
        required_schema_version: u32,
        supported_schema_versions: Vec<u32>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedAgentIdentity {
    pub owner_id: String,
    pub agent_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutorBinding {
    pub registration_id: String,
    pub executor_id: String,
    pub registration_epoch: u64,
    pub verification_key_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutorEnrollment {
    pub version: u32,
    pub registration_id: String,
    pub executor_id: String,
    pub operator_id: String,
    pub repository: String,
    pub step_ids: Vec<String>,
    pub registration_epoch: u64,
    pub verification_key_id: String,
    pub verification_key_sha256: String,
    pub signature_algorithm: ObservationSignatureAlgorithm,
    pub issued_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutorRevocation {
    pub version: u32,
    pub registration_id: String,
    pub repository: String,
    pub registration_epoch: u64,
    pub revoked_at_policy_epoch: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttemptLease {
    pub fence: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTaskAttempt {
    pub version: u32,
    pub claim_request_id: String,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub execution_operation_id: String,
    pub agent: AuthenticatedAgentIdentity,
    pub executor: ExecutorBinding,
    pub lease: AttemptLease,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskClaimRequest {
    pub version: u32,
    pub request_id: String,
    pub execution_operation_id: String,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub agent: AuthenticatedAgentIdentity,
    pub executor: ExecutorBinding,
    pub requested_lease_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskMutationRequest {
    pub version: u32,
    pub request_id: String,
    pub execution_operation_id: String,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub attempt_digest: String,
    pub fence: u64,
    pub agent: AuthenticatedAgentIdentity,
    pub executor: ExecutorBinding,
    pub action: TaskMutationAction,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskMutationAction {
    Heartbeat {},
    Cancel { reason: String },
    Reconcile {},
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskStatusRequest {
    pub version: u32,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub attempt_digest: String,
    pub fence: u64,
    pub agent: AuthenticatedAgentIdentity,
    pub executor: ExecutorBinding,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReferenceFetchRequest {
    pub version: u32,
    pub task_id: String,
    pub task_digest: String,
    pub reference_name: String,
    pub expected_sha256: String,
    pub expected_schema_version: u32,
    pub actor: AuthenticatedAgentIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReferenceFetchResponse {
    Available {
        task_id: String,
        task_digest: String,
        reference_name: String,
        sha256: String,
        schema_version: u32,
        content: String,
    },
    Missing {
        task_id: String,
        task_digest: String,
        reference_name: String,
    },
    Denied {
        task_id: String,
        task_digest: String,
        reference_name: String,
    },
    Uncertain {
        task_id: String,
        task_digest: String,
        reference_name: String,
    },
    Limit {
        task_id: String,
        task_digest: String,
        reference_name: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceTrustClass {
    AgentAssertion,
    RegisteredExecutorObservation,
    IndependentControllerEvidence,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Passed,
    Failed,
    Incomplete,
    Cancelled,
    Uncertain,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CriterionResult {
    pub criterion: String,
    pub outcome: CriterionOutcome,
    pub findings: Vec<String>,
    pub evidence_digests: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CriterionOutcome {
    Passed,
    Failed,
    Incomplete,
    NotClaimed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutorObservationProvenance {
    pub registration_id: String,
    pub registration_epoch: u64,
    pub verification_key_id: String,
    pub observation_digest: String,
    pub signature_algorithm: ObservationSignatureAlgorithm,
    pub signature_hex: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSignatureAlgorithm {
    Ed25519,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskResultAssertion {
    pub version: u32,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub attempt_digest: String,
    pub request_id: String,
    pub execution_operation_id: String,
    pub agent: AuthenticatedAgentIdentity,
    pub executor: ExecutorBinding,
    pub fence: u64,
    pub trust_class: EvidenceTrustClass,
    pub outcome: TaskOutcome,
    pub criteria: Vec<CriterionResult>,
    pub executor_provenance: Option<ExecutorObservationProvenance>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDisposition {
    Confirmed,
    ConclusiveRejection,
    UncertainEffect,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryNextAction {
    InspectStatus,
    RetryIdenticalSafeRequest,
    RequestAuthorizedReconciliation,
    PrepareFreshAttempt,
    Stop,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptObservedState {
    Pending,
    Leased,
    Finished,
    Replaced,
    Expired,
    Cancelled,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "effect", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfirmedEffect {
    LeaseRecorded {
        fence: u64,
    },
    AssertionRetained {
        assertion_digest: String,
    },
    FenceSuperseded {
        old_fence: u64,
        superseding_fence: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "effect", rename_all = "snake_case", deny_unknown_fields)]
pub enum UnresolvedEffect {
    ExecutionMayStillRun {},
    ResultAcceptanceUnknown {},
    ResourceCustody { custody_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskRecoveryEnvelope {
    pub version: u32,
    pub request_id: String,
    pub execution_operation_id: String,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub attempt_digest: String,
    pub fence: u64,
    pub disposition: RecoveryDisposition,
    pub observed_state: AttemptObservedState,
    pub confirmed_effects: Vec<ConfirmedEffect>,
    pub unresolved_effects: Vec<UnresolvedEffect>,
    pub next_action: RecoveryNextAction,
}

impl WorkflowTaskSubject {
    fn validate(&self) -> Result<()> {
        repository(&self.repository)?;
        id(&self.profile)?;
        id(&self.target)?;
        raw_digest(&self.revision)?;
        self.change.validate()?;
        if self.change.repository != self.repository {
            return invalid("task repository does not match its Change");
        }
        positive(self.change_version)?;
        raw_digest(&self.context_digest)?;
        positive(self.policy_epoch)?;
        raw_digest(&self.requirement_digest)?;
        prefixed_digest(&self.plan_digest)?;
        prefixed_digest(&self.skill_digest)?;
        id(&self.step_id)
    }
}

impl InstructionEntrypoint {
    fn validate(&self) -> Result<()> {
        if self.capability != AGENT_TASK_CAPABILITY
            || self.schema_version != AGENT_TASK_CONTRACT_VERSION
            || self.required_criteria.is_empty()
            || self.required_criteria.len() > 64
            || self.references.len() > 32
        {
            return invalid("unsupported or incomplete instruction entrypoint");
        }
        prefixed_digest(&self.workflow_digest)?;
        raw_digest(&self.policy_digest)?;
        let mut criteria = HashSet::new();
        for criterion in &self.required_criteria {
            id(&criterion.id)?;
            text(&criterion.description)?;
            if !criteria.insert(&criterion.id)
                || (criterion.kind == CriterionKind::IndependentReview
                    && criterion.evidence_rule != CriterionEvidenceRule::ControllerOnly)
            {
                return invalid("invalid or duplicate required criterion");
            }
        }
        let mut references = HashSet::new();
        for reference in &self.references {
            id(&reference.name)?;
            raw_digest(&reference.sha256)?;
            path(&reference.path)?;
            if !references.insert(&reference.name) {
                return invalid("duplicate instruction reference");
            }
        }
        Ok(())
    }
}

impl WorkflowTask {
    fn validate(&self) -> Result<()> {
        version(self.version)?;
        id(&self.task_id)?;
        self.subject.validate()?;
        self.entrypoint.validate()?;
        if self.subject.plan_digest != self.entrypoint.workflow_digest {
            return invalid("entrypoint does not match task plan");
        }
        Ok(())
    }
}

impl ExecutorEnrollment {
    fn validate(&self) -> Result<()> {
        version(self.version)?;
        id(&self.registration_id)?;
        id(&self.executor_id)?;
        id(&self.operator_id)?;
        repository(&self.repository)?;
        positive(self.registration_epoch)?;
        id(&self.verification_key_id)?;
        raw_digest(&self.verification_key_sha256)?;
        positive(self.issued_at_ms)?;
        if self.step_ids.is_empty() || self.step_ids.len() > 64 {
            return invalid("enrollment must contain 1 to 64 steps");
        }
        let mut unique = HashSet::new();
        for step in &self.step_ids {
            id(step)?;
            if !unique.insert(step) {
                return invalid("duplicate enrollment step");
            }
        }
        Ok(())
    }

    pub fn binding(&self) -> Result<ExecutorBinding> {
        self.validate()?;
        Ok(ExecutorBinding {
            registration_id: self.registration_id.clone(),
            executor_id: self.executor_id.clone(),
            registration_epoch: self.registration_epoch,
            verification_key_id: self.verification_key_id.clone(),
        })
    }

    pub fn validate_scope_and_binding(
        &self,
        subject: &WorkflowTaskSubject,
        binding: &ExecutorBinding,
    ) -> Result<()> {
        subject.validate()?;
        if binding != &self.binding()?
            || self.repository != subject.repository
            || !self.step_ids.contains(&subject.step_id)
        {
            return invalid("enrollment does not permit this repository and step");
        }
        Ok(())
    }
}

impl ExecutorRevocation {
    fn validate(&self) -> Result<()> {
        version(self.version)?;
        id(&self.registration_id)?;
        repository(&self.repository)?;
        positive(self.registration_epoch)?;
        positive(self.revoked_at_policy_epoch)?;
        text(&self.reason)
    }

    pub fn matches_binding(&self, repository_id: &str, binding: &ExecutorBinding) -> Result<bool> {
        self.validate()?;
        binding.validate()?;
        repository(repository_id)?;
        Ok(self.registration_id == binding.registration_id
            && self.repository == repository_id
            && self.registration_epoch == binding.registration_epoch)
    }
}

impl WorkflowTaskAttempt {
    fn validate(&self) -> Result<()> {
        version(self.version)?;
        id(&self.claim_request_id)?;
        id(&self.task_id)?;
        raw_digest(&self.task_digest)?;
        id(&self.attempt_id)?;
        id(&self.execution_operation_id)?;
        self.agent.validate()?;
        self.executor.validate()?;
        positive(self.lease.fence)?;
        positive(self.lease.issued_at_ms)?;
        positive(self.lease.expires_at_ms)?;
        if self.lease.expires_at_ms <= self.lease.issued_at_ms
            || self.lease.expires_at_ms - self.lease.issued_at_ms > MAX_LEASE_MS
        {
            return invalid("invalid attempt lease interval");
        }
        Ok(())
    }

    pub fn validate_for_task(
        &self,
        task: &WorkflowTask,
        enrollment: &ExecutorEnrollment,
    ) -> Result<()> {
        self.validate()?;
        task.validate()?;
        if self.task_id != task.task_id
            || self.task_digest != task.digest()?
            || self.executor != enrollment.binding()?
            || self.executor.registration_epoch != enrollment.registration_epoch
            || self.executor.executor_id != enrollment.executor_id
        {
            return invalid("attempt does not bind the exact task and enrollment");
        }
        enrollment.validate_scope_and_binding(&task.subject, &self.executor)
    }

    pub fn validate_for_claim(&self, claim: &TaskClaimRequest) -> Result<()> {
        self.validate()?;
        claim.validate()?;
        if self.claim_request_id != claim.request_id
            || self.lease.expires_at_ms - self.lease.issued_at_ms > claim.requested_lease_ms
            || self.task_id != claim.task_id
            || self.task_digest != claim.task_digest
            || self.attempt_id != claim.attempt_id
            || self.execution_operation_id != claim.execution_operation_id
            || self.agent != claim.agent
            || self.executor != claim.executor
        {
            return invalid("attempt does not bind the exact claim request");
        }
        Ok(())
    }
}

impl TaskClaimRequest {
    fn validate(&self) -> Result<()> {
        version(self.version)?;
        id(&self.request_id)?;
        id(&self.execution_operation_id)?;
        id(&self.task_id)?;
        raw_digest(&self.task_digest)?;
        id(&self.attempt_id)?;
        self.agent.validate()?;
        self.executor.validate()?;
        positive(self.requested_lease_ms)?;
        if self.requested_lease_ms > MAX_LEASE_MS {
            return invalid("requested lease exceeds maximum");
        }
        Ok(())
    }

    pub fn validate_for_task(
        &self,
        task: &WorkflowTask,
        enrollment: &ExecutorEnrollment,
    ) -> Result<()> {
        self.validate()?;
        task.validate()?;
        if self.task_id != task.task_id
            || self.task_digest != task.digest()?
            || self.executor != enrollment.binding()?
        {
            return invalid("claim does not bind the exact task and enrollment");
        }
        enrollment.validate_scope_and_binding(&task.subject, &self.executor)
    }
}

impl TaskMutationRequest {
    fn validate(&self) -> Result<()> {
        version(self.version)?;
        id(&self.request_id)?;
        id(&self.execution_operation_id)?;
        id(&self.task_id)?;
        raw_digest(&self.task_digest)?;
        id(&self.attempt_id)?;
        raw_digest(&self.attempt_digest)?;
        positive(self.fence)?;
        self.agent.validate()?;
        self.executor.validate()?;
        if let TaskMutationAction::Cancel { reason } = &self.action {
            text(reason)?;
        }
        Ok(())
    }

    pub fn validate_for_attempt(&self, attempt: &WorkflowTaskAttempt) -> Result<()> {
        self.validate()?;
        attempt.validate()?;
        if self.execution_operation_id != attempt.execution_operation_id
            || self.task_id != attempt.task_id
            || self.task_digest != attempt.task_digest
            || self.attempt_id != attempt.attempt_id
            || self.attempt_digest != attempt.digest()?
            || self.fence != attempt.lease.fence
            || self.agent != attempt.agent
            || self.executor != attempt.executor
        {
            return invalid("mutation does not bind the exact current attempt");
        }
        Ok(())
    }

    pub fn request_digest(&self) -> Result<String> {
        self.validate()?;
        let domain = match self.action {
            TaskMutationAction::Heartbeat {} => HEARTBEAT_DOMAIN,
            TaskMutationAction::Cancel { .. } => CANCEL_DOMAIN,
            TaskMutationAction::Reconcile {} => RECONCILE_DOMAIN,
        };
        domain_digest(domain, &serde_json::to_vec(self)?)
    }
}

impl TaskStatusRequest {
    fn validate(&self) -> Result<()> {
        version(self.version)?;
        id(&self.task_id)?;
        raw_digest(&self.task_digest)?;
        id(&self.attempt_id)?;
        raw_digest(&self.attempt_digest)?;
        positive(self.fence)?;
        self.agent.validate()?;
        self.executor.validate()
    }

    pub fn validate_for_attempt(&self, attempt: &WorkflowTaskAttempt) -> Result<()> {
        self.validate()?;
        attempt.validate()?;
        if self.task_id != attempt.task_id
            || self.task_digest != attempt.task_digest
            || self.attempt_id != attempt.attempt_id
            || self.attempt_digest != attempt.digest()?
            || self.fence != attempt.lease.fence
            || self.agent != attempt.agent
            || self.executor != attempt.executor
        {
            return invalid("status read does not bind the exact current attempt");
        }
        Ok(())
    }
}

impl AuthenticatedAgentIdentity {
    fn validate(&self) -> Result<()> {
        id(&self.owner_id)?;
        id(&self.agent_id)
    }
}

impl ExecutorBinding {
    fn validate(&self) -> Result<()> {
        id(&self.registration_id)?;
        id(&self.executor_id)?;
        positive(self.registration_epoch)?;
        id(&self.verification_key_id)
    }
}

impl TaskResultAssertion {
    fn validate(&self) -> Result<()> {
        version(self.version)?;
        id(&self.task_id)?;
        raw_digest(&self.task_digest)?;
        id(&self.attempt_id)?;
        raw_digest(&self.attempt_digest)?;
        id(&self.request_id)?;
        id(&self.execution_operation_id)?;
        self.agent.validate()?;
        self.executor.validate()?;
        positive(self.fence)?;
        if self.trust_class == EvidenceTrustClass::IndependentControllerEvidence {
            return invalid("an external assertion cannot claim controller evidence");
        }
        if self.criteria.is_empty() || self.criteria.len() > 64 {
            return invalid("result must contain 1 to 64 criterion rows");
        }
        let mut criteria = HashSet::new();
        for row in &self.criteria {
            id(&row.criterion)?;
            if !criteria.insert(&row.criterion)
                || row.findings.len() > 16
                || row.evidence_digests.len() > 16
                || (row.outcome == CriterionOutcome::NotClaimed
                    && (!row.findings.is_empty() || !row.evidence_digests.is_empty()))
            {
                return invalid("invalid or duplicate criterion result");
            }
            for finding in &row.findings {
                text(finding)?;
            }
            let mut evidence = HashSet::new();
            for digest in &row.evidence_digests {
                raw_digest(digest)?;
                if !evidence.insert(digest) {
                    return invalid("duplicate criterion evidence digest");
                }
            }
        }
        match (self.trust_class, &self.executor_provenance) {
            (EvidenceTrustClass::AgentAssertion, None) => Ok(()),
            (EvidenceTrustClass::RegisteredExecutorObservation, Some(provenance)) => {
                provenance.validate(&self.executor)?;
                if provenance.observation_digest != self.observation_digest()? {
                    return invalid("signature does not bind the exact observation");
                }
                Ok(())
            }
            _ => invalid("assertion trust class and provenance disagree"),
        }
    }

    /// Hashes compact `serde_json` bytes in declared field order with provenance as `null`.
    /// Exact escaping and integer encoding are specified in
    /// `docs/advanced/agent-workflow-tasks.md` (Observation signing bytes).
    /// The digest input is the observation domain, its NUL terminator, the JSON length as
    /// big-endian u64, and the exact JSON bytes.
    pub fn observation_digest(&self) -> Result<String> {
        let mut unsigned = self.clone();
        unsigned.executor_provenance = None;
        domain_digest(OBSERVATION_DOMAIN, &serde_json::to_vec(&unsigned)?)
    }

    pub fn request_digest(&self) -> Result<String> {
        self.validate()?;
        domain_digest(RESULT_DOMAIN, &serde_json::to_vec(self)?)
    }

    pub fn validate_for_task_attempt(
        &self,
        task: &WorkflowTask,
        attempt: &WorkflowTaskAttempt,
    ) -> Result<()> {
        self.validate()?;
        task.validate()?;
        attempt.validate()?;
        if self.task_id != attempt.task_id
            || self.task_digest != attempt.task_digest
            || self.task_id != task.task_id
            || self.task_digest != task.digest()?
            || self.attempt_id != attempt.attempt_id
            || self.attempt_digest != attempt.digest()?
            || self.execution_operation_id != attempt.execution_operation_id
            || self.agent != attempt.agent
            || self.executor != attempt.executor
            || self.fence != attempt.lease.fence
        {
            return invalid("result does not bind the exact current attempt");
        }

        let required = task
            .entrypoint
            .required_criteria
            .iter()
            .map(|criterion| (criterion.id.as_str(), criterion))
            .collect::<std::collections::HashMap<_, _>>();
        if self.criteria.len() != required.len() {
            return invalid("result omits a task criterion");
        }

        let mut eligible = 0_usize;
        let mut eligible_passed = 0_usize;
        let mut eligible_failed = false;
        for row in &self.criteria {
            let criterion = required
                .get(row.criterion.as_str())
                .ok_or_else(|| Error::Invalid("result names an unknown criterion".into()))?;
            let laptop_eligible =
                criterion.evidence_rule == CriterionEvidenceRule::EnrolledLaptopOrController;
            if self.trust_class == EvidenceTrustClass::RegisteredExecutorObservation
                && !laptop_eligible
                && row.outcome != CriterionOutcome::NotClaimed
            {
                return invalid("registered executor cannot claim controller-only criterion");
            }
            if self.trust_class != EvidenceTrustClass::RegisteredExecutorObservation
                || laptop_eligible
            {
                eligible += 1;
                eligible_passed += usize::from(row.outcome == CriterionOutcome::Passed);
                eligible_failed |= row.outcome == CriterionOutcome::Failed;
            }
        }
        let complete = eligible > 0 && eligible == eligible_passed;
        match self.outcome {
            TaskOutcome::Passed if !complete => {
                return invalid("passed result does not cover an eligible required criterion");
            }
            TaskOutcome::Failed if !eligible_failed => {
                return invalid("failed result contains no failed eligible criterion");
            }
            TaskOutcome::Incomplete | TaskOutcome::Cancelled | TaskOutcome::Uncertain
                if complete || eligible_failed =>
            {
                return invalid("incomplete result contradicts criterion outcomes");
            }
            _ => {}
        }
        Ok(())
    }
}

impl ReferenceFetchRequest {
    fn validate(&self) -> Result<()> {
        version(self.version)?;
        id(&self.task_id)?;
        raw_digest(&self.task_digest)?;
        id(&self.reference_name)?;
        raw_digest(&self.expected_sha256)?;
        if self.expected_schema_version != AGENT_TASK_CONTRACT_VERSION {
            return invalid("unsupported reference schema version");
        }
        self.actor.validate()
    }

    pub fn validate_for_task(&self, task: &WorkflowTask) -> Result<()> {
        self.validate()?;
        task.validate()?;
        if self.task_id != task.task_id || self.task_digest != task.digest()? {
            return invalid("reference fetch does not bind the exact task");
        }
        let reference = task
            .entrypoint
            .references
            .iter()
            .find(|reference| reference.name == self.reference_name)
            .ok_or_else(|| Error::Invalid("unknown instruction reference".into()))?;
        if reference.sha256 != self.expected_sha256 {
            return invalid("reference fetch digest does not match stored task");
        }
        Ok(())
    }
}

impl ReferenceFetchResponse {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        if input.len() > MAX_REFERENCE_ENVELOPE_BYTES {
            return Err(Error::Limit("reference response exceeds limit".into()));
        }
        let value: Self = serde_json::from_slice(input)?;
        value.validate()?;
        Ok(value)
    }

    fn identity(&self) -> (&str, &str, &str) {
        match self {
            Self::Available {
                task_id,
                task_digest,
                reference_name,
                ..
            }
            | Self::Missing {
                task_id,
                task_digest,
                reference_name,
            }
            | Self::Denied {
                task_id,
                task_digest,
                reference_name,
            }
            | Self::Uncertain {
                task_id,
                task_digest,
                reference_name,
            }
            | Self::Limit {
                task_id,
                task_digest,
                reference_name,
            } => (task_id, task_digest, reference_name),
        }
    }

    fn validate(&self) -> Result<()> {
        let (task_id, task_digest, reference_name) = self.identity();
        id(task_id)?;
        raw_digest(task_digest)?;
        id(reference_name)?;
        if let Self::Available {
            sha256,
            schema_version,
            content,
            ..
        } = self
        {
            raw_digest(sha256)?;
            if *schema_version != AGENT_TASK_CONTRACT_VERSION
                || content.len() > MAX_REFERENCE_BYTES
                || sha256_bytes(content.as_bytes()) != *sha256
            {
                return invalid("invalid available reference content");
            }
        }
        Ok(())
    }

    pub fn validate_for_request(&self, request: &ReferenceFetchRequest) -> Result<()> {
        self.validate()?;
        request.validate()?;
        let (task_id, task_digest, reference_name) = self.identity();
        if task_id != request.task_id
            || task_digest != request.task_digest
            || reference_name != request.reference_name
        {
            return invalid("reference response does not bind the exact request");
        }
        if let Self::Available {
            sha256,
            schema_version,
            ..
        } = self
            && (sha256 != &request.expected_sha256
                || schema_version != &request.expected_schema_version)
        {
            return invalid("reference response changed expected identity");
        }
        Ok(())
    }
}

impl ExecutorObservationProvenance {
    fn validate(&self, executor: &ExecutorBinding) -> Result<()> {
        raw_digest(&self.observation_digest)?;
        if self.registration_id != executor.registration_id
            || self.registration_epoch != executor.registration_epoch
            || self.verification_key_id != executor.verification_key_id
            || self.signature_hex.len() != 128
            || !lower_hex(&self.signature_hex)
        {
            return invalid("invalid executor observation provenance");
        }
        Ok(())
    }
}

impl TaskRecoveryEnvelope {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        let value: Self = decode(input)?;
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<()> {
        version(self.version)?;
        id(&self.request_id)?;
        id(&self.execution_operation_id)?;
        id(&self.task_id)?;
        raw_digest(&self.task_digest)?;
        id(&self.attempt_id)?;
        raw_digest(&self.attempt_digest)?;
        positive(self.fence)?;
        if self.confirmed_effects.len() > 16 || self.unresolved_effects.len() > 16 {
            return invalid("recovery envelope exceeds effect limits");
        }
        let mut assertion_retained = false;
        let mut superseding_fence = None;
        let mut confirmed_kinds = HashSet::new();
        for effect in &self.confirmed_effects {
            match effect {
                ConfirmedEffect::LeaseRecorded { fence } => {
                    if !confirmed_kinds.insert("lease") {
                        return invalid("duplicate confirmed recovery effect");
                    }
                    positive(*fence)?;
                    if *fence != self.fence {
                        return invalid("confirmed lease uses the wrong fence");
                    }
                }
                ConfirmedEffect::AssertionRetained { assertion_digest } => {
                    if !confirmed_kinds.insert("assertion") {
                        return invalid("duplicate confirmed recovery effect");
                    }
                    raw_digest(assertion_digest)?;
                    assertion_retained = true;
                }
                ConfirmedEffect::FenceSuperseded {
                    old_fence,
                    superseding_fence: new_fence,
                } => {
                    if !confirmed_kinds.insert("supersession") {
                        return invalid("duplicate confirmed recovery effect");
                    }
                    positive(*old_fence)?;
                    positive(*new_fence)?;
                    if *old_fence != self.fence || *new_fence <= *old_fence {
                        return invalid("fence supersession is not monotonic");
                    }
                    superseding_fence = Some(*new_fence);
                }
            }
        }
        let mut execution_may_run = false;
        let mut hard_block = false;
        let mut unresolved_kinds = HashSet::new();
        for effect in &self.unresolved_effects {
            match effect {
                UnresolvedEffect::ExecutionMayStillRun {} => {
                    if !unresolved_kinds.insert("execution".to_owned()) {
                        return invalid("duplicate unresolved recovery effect");
                    }
                    execution_may_run = true;
                }
                UnresolvedEffect::ResultAcceptanceUnknown {} => {
                    if !unresolved_kinds.insert("result".to_owned()) {
                        return invalid("duplicate unresolved recovery effect");
                    }
                    hard_block = true;
                }
                UnresolvedEffect::ResourceCustody { custody_id } => {
                    id(custody_id)?;
                    if !unresolved_kinds.insert(format!("custody:{custody_id}")) {
                        return invalid("duplicate unresolved recovery effect");
                    }
                    hard_block = true;
                }
            }
        }
        if superseding_fence.is_some() && self.observed_state != AttemptObservedState::Replaced {
            return invalid("fence supersession requires a replaced attempt");
        }

        match self.disposition {
            RecoveryDisposition::Confirmed => {
                if self.confirmed_effects.is_empty()
                    || !self.unresolved_effects.is_empty()
                    || !matches!(
                        self.next_action,
                        RecoveryNextAction::InspectStatus | RecoveryNextAction::Stop
                    )
                    || (assertion_retained && self.observed_state != AttemptObservedState::Finished)
                {
                    return invalid("confirmed recovery state, effects and action disagree");
                }
            }
            RecoveryDisposition::ConclusiveRejection => {
                if !self.confirmed_effects.is_empty()
                    || !self.unresolved_effects.is_empty()
                    || !matches!(
                        self.next_action,
                        RecoveryNextAction::InspectStatus
                            | RecoveryNextAction::RetryIdenticalSafeRequest
                            | RecoveryNextAction::Stop
                    )
                {
                    return invalid("rejected recovery must have no effect");
                }
                if self.next_action == RecoveryNextAction::RetryIdenticalSafeRequest
                    && self.observed_state != AttemptObservedState::Pending
                {
                    return invalid("identical retry requires a pending attempt");
                }
            }
            RecoveryDisposition::UncertainEffect => {
                if self.unresolved_effects.is_empty()
                    || self.next_action == RecoveryNextAction::RetryIdenticalSafeRequest
                    || assertion_retained
                {
                    return invalid("uncertain recovery state, effects and action disagree");
                }
                if self.next_action == RecoveryNextAction::PrepareFreshAttempt
                    && (self.observed_state != AttemptObservedState::Replaced
                        || superseding_fence.is_none()
                        || !execution_may_run
                        || hard_block
                        || self.unresolved_effects.len() != 1)
                {
                    return invalid("fresh attempt requires explicit safe fence supersession");
                }
            }
        }
        Ok(())
    }

    pub fn validate_for_attempt(&self, attempt: &WorkflowTaskAttempt) -> Result<()> {
        self.validate()?;
        attempt.validate()?;
        if self.execution_operation_id != attempt.execution_operation_id
            || self.task_id != attempt.task_id
            || self.task_digest != attempt.task_digest
            || self.attempt_id != attempt.attempt_id
            || self.attempt_digest != attempt.digest()?
            || self.fence != attempt.lease.fence
        {
            return invalid("recovery does not bind the exact attempt");
        }
        Ok(())
    }
}

impl InstructionEntrypointResponse {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        let value: Self = decode(input)?;
        match &value {
            Self::Available { entrypoint } => entrypoint.validate()?,
            Self::SchemaMismatch {
                capability,
                required_schema_version,
                supported_schema_versions,
            } if capability == AGENT_TASK_CAPABILITY
                && *required_schema_version > 0
                && !supported_schema_versions.is_empty()
                && supported_schema_versions.len() <= 16
                && supported_schema_versions.iter().all(|version| *version > 0)
                && supported_schema_versions
                    .iter()
                    .collect::<HashSet<_>>()
                    .len()
                    == supported_schema_versions.len()
                && !supported_schema_versions.contains(required_schema_version) => {}
            Self::SchemaMismatch { .. } => return invalid("invalid schema mismatch response"),
        }
        Ok(value)
    }
}

macro_rules! wire_contract {
    ($type:ty, $domain:expr) => {
        impl $type {
            pub fn from_slice(input: &[u8]) -> Result<Self> {
                let value: Self = decode(input)?;
                value.validate()?;
                Ok(value)
            }

            pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
                self.validate()?;
                let bytes = serde_json::to_vec(self)?;
                if bytes.len() > MAX_WIRE_BYTES {
                    return Err(Error::Limit("agent task contract exceeds 256 KiB".into()));
                }
                Ok(bytes)
            }

            pub fn digest(&self) -> Result<String> {
                domain_digest($domain, &self.canonical_bytes()?)
            }
        }
    };
}

wire_contract!(WorkflowTask, TASK_DOMAIN);
wire_contract!(WorkflowTaskAttempt, ATTEMPT_DOMAIN);
wire_contract!(TaskResultAssertion, ASSERTION_DOMAIN);
wire_contract!(ExecutorEnrollment, ENROLLMENT_DOMAIN);
wire_contract!(ExecutorRevocation, REVOCATION_DOMAIN);
wire_contract!(TaskClaimRequest, CLAIM_DOMAIN);
wire_contract!(ReferenceFetchRequest, REFERENCE_FETCH_DOMAIN);

impl TaskMutationRequest {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        let value: Self = decode(input)?;
        value.validate()?;
        Ok(value)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_WIRE_BYTES {
            return Err(Error::Limit("agent task contract exceeds 256 KiB".into()));
        }
        Ok(bytes)
    }
}

impl TaskStatusRequest {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        let value: Self = decode(input)?;
        value.validate()?;
        Ok(value)
    }
}

fn decode<T: serde::de::DeserializeOwned>(input: &[u8]) -> Result<T> {
    if input.len() > MAX_WIRE_BYTES {
        return Err(Error::Limit("agent task input exceeds 256 KiB".into()));
    }
    Ok(serde_json::from_slice(input)?)
}

fn version(value: u32) -> Result<()> {
    if value != AGENT_TASK_CONTRACT_VERSION {
        return invalid("unsupported agent task contract version");
    }
    Ok(())
}

fn id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return invalid("invalid canonical identifier");
    }
    Ok(())
}

fn text(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || value.contains('\0') {
        return invalid("invalid bounded text");
    }
    Ok(())
}

fn repository(value: &str) -> Result<()> {
    if value.len() != 32 || !lower_hex(value) {
        return invalid("repository must be 32 lowercase hexadecimal characters");
    }
    Ok(())
}

fn raw_digest(value: &str) -> Result<()> {
    if value.len() != 64 || !lower_hex(value) {
        return invalid("invalid lowercase SHA-256 digest");
    }
    Ok(())
}

fn prefixed_digest(value: &str) -> Result<()> {
    raw_digest(
        value
            .strip_prefix("sha256:")
            .ok_or_else(|| Error::Invalid("digest must use sha256".into()))?,
    )
}

fn lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn positive(value: u64) -> Result<()> {
    if value == 0 || value > i64::MAX as u64 {
        return invalid("value must be a positive signed 64-bit integer");
    }
    Ok(())
}

fn path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 512
        || value.starts_with('/')
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'\\' | b':'))
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return invalid("invalid canonical relative source path");
    }
    Ok(())
}

fn domain_digest(domain: &[u8], bytes: &[u8]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::Invalid(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> WorkflowTask {
        WorkflowTask {
            version: 1,
            task_id: "task-1".into(),
            subject: WorkflowTaskSubject {
                repository: "a".repeat(32),
                profile: "laptop".into(),
                target: "main".into(),
                revision: "2".repeat(64),
                change: ChangeId {
                    repository: "a".repeat(32),
                    id: "change-1".into(),
                },
                change_version: 3,
                context_digest: "3".repeat(64),
                policy_epoch: 4,
                requirement_digest: "4".repeat(64),
                plan_digest: format!("sha256:{}", "5".repeat(64)),
                skill_digest: format!("sha256:{}", "6".repeat(64)),
                step_id: "tests".into(),
            },
            required: true,
            entrypoint: InstructionEntrypoint {
                capability: AGENT_TASK_CAPABILITY.into(),
                schema_version: 1,
                workflow_digest: format!("sha256:{}", "5".repeat(64)),
                policy_digest: "7".repeat(64),
                required_criteria: vec![
                    RequiredCriterion {
                        id: "pinned-tests".into(),
                        description: "Run the pinned tests".into(),
                        kind: CriterionKind::Check,
                        evidence_rule: CriterionEvidenceRule::EnrolledLaptopOrController,
                    },
                    RequiredCriterion {
                        id: "independent-review".into(),
                        description: "Run the pinned controller review".into(),
                        kind: CriterionKind::IndependentReview,
                        evidence_rule: CriterionEvidenceRule::ControllerOnly,
                    },
                ],
                references: vec![NamedInstructionReference {
                    name: "skill".into(),
                    sha256: "8".repeat(64),
                    path: "skills/test/SKILL.md".into(),
                }],
                permitted_progression: EntrypointProgression::ClaimTask,
            },
        }
    }

    fn enrollment(task: &WorkflowTask) -> ExecutorEnrollment {
        ExecutorEnrollment {
            version: 1,
            registration_id: "registration-1".into(),
            executor_id: "executor-1".into(),
            operator_id: "operator-1".into(),
            repository: task.subject.repository.clone(),
            step_ids: vec![task.subject.step_id.clone()],
            registration_epoch: 2,
            verification_key_id: "key-1".into(),
            verification_key_sha256: "9".repeat(64),
            signature_algorithm: ObservationSignatureAlgorithm::Ed25519,
            issued_at_ms: 1,
        }
    }

    fn attempt(task: &WorkflowTask, enrollment: &ExecutorEnrollment) -> WorkflowTaskAttempt {
        WorkflowTaskAttempt {
            version: 1,
            claim_request_id: "claim-request-1".into(),
            task_id: task.task_id.clone(),
            task_digest: task.digest().unwrap(),
            attempt_id: "attempt-1".into(),
            execution_operation_id: "execution-1".into(),
            agent: AuthenticatedAgentIdentity {
                owner_id: "owner-1".into(),
                agent_id: "agent-1".into(),
            },
            executor: enrollment.binding().unwrap(),
            lease: AttemptLease {
                fence: 3,
                issued_at_ms: 10,
                expires_at_ms: 20,
            },
        }
    }

    fn result(task: &WorkflowTask, attempt: &WorkflowTaskAttempt) -> TaskResultAssertion {
        let mut result = TaskResultAssertion {
            version: 1,
            task_id: task.task_id.clone(),
            task_digest: task.digest().unwrap(),
            attempt_id: attempt.attempt_id.clone(),
            attempt_digest: attempt.digest().unwrap(),
            request_id: "result-request-1".into(),
            execution_operation_id: attempt.execution_operation_id.clone(),
            agent: attempt.agent.clone(),
            executor: attempt.executor.clone(),
            fence: attempt.lease.fence,
            trust_class: EvidenceTrustClass::RegisteredExecutorObservation,
            outcome: TaskOutcome::Passed,
            criteria: vec![
                CriterionResult {
                    criterion: "pinned-tests".into(),
                    outcome: CriterionOutcome::Passed,
                    findings: vec![],
                    evidence_digests: vec!["a".repeat(64)],
                },
                CriterionResult {
                    criterion: "independent-review".into(),
                    outcome: CriterionOutcome::NotClaimed,
                    findings: vec![],
                    evidence_digests: vec![],
                },
            ],
            executor_provenance: None,
        };
        result.executor_provenance = Some(ExecutorObservationProvenance {
            registration_id: result.executor.registration_id.clone(),
            registration_epoch: result.executor.registration_epoch,
            verification_key_id: result.executor.verification_key_id.clone(),
            observation_digest: result.observation_digest().unwrap(),
            signature_algorithm: ObservationSignatureAlgorithm::Ed25519,
            signature_hex: "b".repeat(128),
        });
        result
    }

    fn unsigned(mut result: TaskResultAssertion) -> TaskResultAssertion {
        result.trust_class = EvidenceTrustClass::AgentAssertion;
        result.executor_provenance = None;
        result
    }

    fn resign(result: &mut TaskResultAssertion) {
        result.executor_provenance = None;
        result.executor_provenance = Some(ExecutorObservationProvenance {
            registration_id: result.executor.registration_id.clone(),
            registration_epoch: result.executor.registration_epoch,
            verification_key_id: result.executor.verification_key_id.clone(),
            observation_digest: result.observation_digest().unwrap(),
            signature_algorithm: ObservationSignatureAlgorithm::Ed25519,
            signature_hex: "b".repeat(128),
        });
    }

    fn recovery() -> TaskRecoveryEnvelope {
        TaskRecoveryEnvelope {
            version: 1,
            request_id: "request-1".into(),
            execution_operation_id: "execution-1".into(),
            task_id: "task-1".into(),
            task_digest: "1".repeat(64),
            attempt_id: "attempt-1".into(),
            attempt_digest: "2".repeat(64),
            fence: 3,
            disposition: RecoveryDisposition::UncertainEffect,
            observed_state: AttemptObservedState::Expired,
            confirmed_effects: vec![],
            unresolved_effects: vec![UnresolvedEffect::ExecutionMayStillRun {}],
            next_action: RecoveryNextAction::RequestAuthorizedReconciliation,
        }
    }

    #[test]
    fn strict_decode_and_digests_bind_task_and_attempt() {
        let task = task();
        assert_eq!(
            WorkflowTask::from_slice(&task.canonical_bytes().unwrap()).unwrap(),
            task
        );
        let enrollment = enrollment(&task);
        let attempt = attempt(&task, &enrollment);
        attempt.validate_for_task(&task, &enrollment).unwrap();

        let mut unknown = serde_json::to_value(&task).unwrap();
        unknown["unknown"] = true.into();
        assert!(WorkflowTask::from_slice(&serde_json::to_vec(&unknown).unwrap()).is_err());
        assert!(matches!(
            WorkflowTask::from_slice(&vec![b' '; MAX_WIRE_BYTES + 1]),
            Err(Error::Limit(_))
        ));
    }

    #[test]
    fn independent_review_remains_controller_only() {
        let mut task = task();
        task.entrypoint.required_criteria[1].evidence_rule =
            CriterionEvidenceRule::EnrolledLaptopOrController;
        assert!(task.validate().is_err());
    }

    #[test]
    fn enrollment_and_revocation_helpers_are_structural_only() {
        let task = task();
        let enrollment = enrollment(&task);
        enrollment
            .validate_scope_and_binding(&task.subject, &enrollment.binding().unwrap())
            .unwrap();
        let revocation = ExecutorRevocation {
            version: 1,
            registration_id: enrollment.registration_id.clone(),
            repository: enrollment.repository.clone(),
            registration_epoch: enrollment.registration_epoch,
            revoked_at_policy_epoch: 5,
            reason: "operator revoked laptop".into(),
        };
        assert!(
            revocation
                .matches_binding(&task.subject.repository, &enrollment.binding().unwrap())
                .unwrap()
        );
        let mut unrelated = enrollment.binding().unwrap();
        unrelated.registration_epoch += 1;
        assert!(
            !revocation
                .matches_binding(&task.subject.repository, &unrelated)
                .unwrap()
        );
    }

    #[test]
    fn task_aware_results_enforce_criterion_identity_and_trust() {
        let task = task();
        let enrollment = enrollment(&task);
        let attempt = attempt(&task, &enrollment);
        result(&task, &attempt)
            .validate_for_task_attempt(&task, &attempt)
            .unwrap();

        let mut controller_claim = result(&task, &attempt);
        controller_claim.criteria[1].outcome = CriterionOutcome::Passed;
        resign(&mut controller_claim);
        assert!(
            controller_claim
                .validate_for_task_attempt(&task, &attempt)
                .is_err()
        );

        let mut duplicate = unsigned(result(&task, &attempt));
        duplicate.criteria[1].criterion = "pinned-tests".into();
        assert!(duplicate.validate().is_err());

        let mut false_nonclaim = unsigned(result(&task, &attempt));
        false_nonclaim.criteria[1]
            .evidence_digests
            .push("c".repeat(64));
        assert!(false_nonclaim.validate().is_err());

        let mut unknown = unsigned(result(&task, &attempt));
        unknown.criteria[1].criterion = "unknown".into();
        assert!(unknown.validate_for_task_attempt(&task, &attempt).is_err());

        let mut omitted = unsigned(result(&task, &attempt));
        omitted.criteria.pop();
        assert!(omitted.validate_for_task_attempt(&task, &attempt).is_err());

        let mut zero_eligible_pass = task.clone();
        zero_eligible_pass.entrypoint.required_criteria.remove(0);
        let zero_attempt = WorkflowTaskAttempt {
            task_digest: zero_eligible_pass.digest().unwrap(),
            ..attempt.clone()
        };
        let mut zero_result = result(&task, &attempt);
        zero_result.task_digest = zero_eligible_pass.digest().unwrap();
        zero_result.attempt_digest = zero_attempt.digest().unwrap();
        zero_result.criteria.remove(0);
        resign(&mut zero_result);
        assert!(
            zero_result
                .validate_for_task_attempt(&zero_eligible_pass, &zero_attempt)
                .is_err()
        );
    }

    #[test]
    fn observation_serialization_golden_vector() {
        let mut value = TaskResultAssertion {
            version: 1,
            task_id: "task-1".into(),
            task_digest: "1".repeat(64),
            attempt_id: "attempt-1".into(),
            attempt_digest: "2".repeat(64),
            request_id: "result-1".into(),
            execution_operation_id: "execution-1".into(),
            agent: AuthenticatedAgentIdentity {
                owner_id: "owner-1".into(),
                agent_id: "agent-1".into(),
            },
            executor: ExecutorBinding {
                registration_id: "registration-1".into(),
                executor_id: "executor-1".into(),
                registration_epoch: i64::MAX as u64,
                verification_key_id: "key-1".into(),
            },
            fence: i64::MAX as u64,
            trust_class: EvidenceTrustClass::RegisteredExecutorObservation,
            outcome: TaskOutcome::Passed,
            criteria: vec![
                CriterionResult {
                    criterion: "check".into(),
                    outcome: CriterionOutcome::Passed,
                    findings: vec![
                        "quote \" slash / backslash \\\n\u{8}\t\u{c}\r\u{1}\u{1f} é e\u{301} 😀\u{2028}\u{2029}".into(),
                        "second".into(),
                    ],
                    evidence_digests: vec!["a".repeat(64), "b".repeat(64)],
                },
                CriterionResult {
                    criterion: "review".into(),
                    outcome: CriterionOutcome::NotClaimed,
                    findings: vec![],
                    evidence_digests: vec![],
                },
            ],
            executor_provenance: None,
        };
        let expected = concat!(
            r#"{"version":1,"task_id":"task-1","task_digest":"1111111111111111111111111111111111111111111111111111111111111111","attempt_id":"attempt-1","attempt_digest":"2222222222222222222222222222222222222222222222222222222222222222","request_id":"result-1","execution_operation_id":"execution-1","agent":{"owner_id":"owner-1","agent_id":"agent-1"},"executor":{"registration_id":"registration-1","executor_id":"executor-1","registration_epoch":9223372036854775807,"verification_key_id":"key-1"},"fence":9223372036854775807,"trust_class":"registered_executor_observation","outcome":"passed","criteria":"#,
            r#"[{"criterion":"check","outcome":"passed","findings":["quote \" slash / backslash \\\n\b\t\f\r\u0001\u001f é é 😀"#,
            "\u{2028}\u{2029}",
            r#"","second"],"evidence_digests":["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]},{"criterion":"review","outcome":"not_claimed","findings":[],"evidence_digests":[]}],"executor_provenance":null}"#,
        );
        assert_eq!(serde_json::to_vec(&value).unwrap(), expected.as_bytes());
        assert_eq!(expected.len(), 991);
        let digest = "53d42bccedb1a288a5527823348a35b818dfdfc778ab2c345e5dfa5ce6a2e7f7";
        assert_eq!(value.observation_digest().unwrap(), digest);
        resign(&mut value);
        value.validate().unwrap();
        assert_eq!(value.observation_digest().unwrap(), digest);
    }

    #[test]
    fn signed_observation_binds_null_provenance_and_exact_fields() {
        let task = task();
        let enrollment = enrollment(&task);
        let attempt = attempt(&task, &enrollment);
        let mut value = result(&task, &attempt);
        let expected = value
            .executor_provenance
            .as_ref()
            .unwrap()
            .observation_digest
            .clone();
        value.executor_provenance = None;
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .ends_with("\"executor_provenance\":null}")
        );
        assert_eq!(value.observation_digest().unwrap(), expected);

        let mut changed = result(&task, &attempt);
        changed.criteria[0].findings.push("changed".into());
        assert!(changed.validate().is_err());
    }

    #[test]
    fn claims_mutations_and_status_bind_exact_attempt_with_separate_identities() {
        let task = task();
        let enrollment = enrollment(&task);
        let attempt = attempt(&task, &enrollment);
        let claim = TaskClaimRequest {
            version: 1,
            request_id: "claim-request-1".into(),
            execution_operation_id: attempt.execution_operation_id.clone(),
            task_id: task.task_id.clone(),
            task_digest: task.digest().unwrap(),
            attempt_id: attempt.attempt_id.clone(),
            agent: attempt.agent.clone(),
            executor: attempt.executor.clone(),
            requested_lease_ms: 1_000,
        };
        claim.validate_for_task(&task, &enrollment).unwrap();
        attempt.validate_for_claim(&claim).unwrap();
        let mut other_claim = claim.clone();
        other_claim.request_id = "other-claim".into();
        assert!(attempt.validate_for_claim(&other_claim).is_err());
        let mut bounded_claim = claim.clone();
        bounded_claim.requested_lease_ms = attempt.lease.expires_at_ms - attempt.lease.issued_at_ms;
        attempt.validate_for_claim(&bounded_claim).unwrap();
        bounded_claim.requested_lease_ms -= 1;
        assert!(attempt.validate_for_claim(&bounded_claim).is_err());
        let mut changed_attempt = attempt.clone();
        changed_attempt.claim_request_id = "other-claim".into();
        assert_ne!(attempt.digest().unwrap(), changed_attempt.digest().unwrap());
        let mut changed_claim = claim.clone();
        changed_claim.requested_lease_ms += 1;
        assert_ne!(claim.digest().unwrap(), changed_claim.digest().unwrap());

        let mutation = TaskMutationRequest {
            version: 1,
            request_id: "mutation-request-1".into(),
            execution_operation_id: attempt.execution_operation_id.clone(),
            task_id: attempt.task_id.clone(),
            task_digest: attempt.task_digest.clone(),
            attempt_id: attempt.attempt_id.clone(),
            attempt_digest: attempt.digest().unwrap(),
            fence: attempt.lease.fence,
            agent: attempt.agent.clone(),
            executor: attempt.executor.clone(),
            action: TaskMutationAction::Heartbeat {},
        };
        mutation.validate_for_attempt(&attempt).unwrap();
        let identical = mutation.clone();
        assert_eq!(
            mutation.request_digest().unwrap(),
            identical.request_digest().unwrap()
        );
        let mut changed = mutation.clone();
        changed.action = TaskMutationAction::Cancel {
            reason: "operator request".into(),
        };
        assert_ne!(
            mutation.request_digest().unwrap(),
            changed.request_digest().unwrap()
        );
        let mut stale = mutation.clone();
        stale.fence += 1;
        assert!(stale.validate_for_attempt(&attempt).is_err());

        let original_result = unsigned(result(&task, &attempt));
        let mut changed_result = original_result.clone();
        changed_result.criteria[0].findings.push("changed".into());
        assert_eq!(original_result.request_id, changed_result.request_id);
        assert_ne!(
            original_result.request_digest().unwrap(),
            changed_result.request_digest().unwrap()
        );

        let status = TaskStatusRequest {
            version: 1,
            task_id: attempt.task_id.clone(),
            task_digest: attempt.task_digest.clone(),
            attempt_id: attempt.attempt_id.clone(),
            attempt_digest: attempt.digest().unwrap(),
            fence: attempt.lease.fence,
            agent: attempt.agent.clone(),
            executor: attempt.executor.clone(),
        };
        status.validate_for_attempt(&attempt).unwrap();
        assert!(
            !serde_json::to_value(status)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("request_id")
        );
    }

    #[test]
    fn reference_paths_are_platform_independent() {
        let mut task = task();
        task.entrypoint.references[0].path = "skills/check/SKILL.md".into();
        task.validate().unwrap();
        for invalid in [
            r"..\secret",
            r"C:\source",
            "C:source",
            "a:b",
            "a\0b",
            "a\nb",
            "a\u{7f}b",
        ] {
            task.entrypoint.references[0].path = invalid.into();
            assert!(task.validate().is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn reference_fetch_is_authority_derived_bounded_and_identity_checked() {
        let task = task();
        let content = "pinned instructions";
        let mut task_with_content_digest = task.clone();
        task_with_content_digest.entrypoint.references[0].sha256 = sha256_bytes(content.as_bytes());
        let request = ReferenceFetchRequest {
            version: 1,
            task_id: task_with_content_digest.task_id.clone(),
            task_digest: task_with_content_digest.digest().unwrap(),
            reference_name: "skill".into(),
            expected_sha256: sha256_bytes(content.as_bytes()),
            expected_schema_version: 1,
            actor: AuthenticatedAgentIdentity {
                owner_id: "owner-1".into(),
                agent_id: "agent-1".into(),
            },
        };
        request
            .validate_for_task(&task_with_content_digest)
            .unwrap();
        let available = ReferenceFetchResponse::Available {
            task_id: request.task_id.clone(),
            task_digest: request.task_digest.clone(),
            reference_name: request.reference_name.clone(),
            sha256: request.expected_sha256.clone(),
            schema_version: 1,
            content: content.into(),
        };
        available.validate_for_request(&request).unwrap();
        for response in [
            ReferenceFetchResponse::Missing {
                task_id: request.task_id.clone(),
                task_digest: request.task_digest.clone(),
                reference_name: request.reference_name.clone(),
            },
            ReferenceFetchResponse::Denied {
                task_id: request.task_id.clone(),
                task_digest: request.task_digest.clone(),
                reference_name: request.reference_name.clone(),
            },
            ReferenceFetchResponse::Uncertain {
                task_id: request.task_id.clone(),
                task_digest: request.task_digest.clone(),
                reference_name: request.reference_name.clone(),
            },
            ReferenceFetchResponse::Limit {
                task_id: request.task_id.clone(),
                task_digest: request.task_digest.clone(),
                reference_name: request.reference_name.clone(),
            },
        ] {
            response.validate_for_request(&request).unwrap();
        }
        let mut cross_task = request.clone();
        cross_task.task_digest = "f".repeat(64);
        assert!(
            cross_task
                .validate_for_task(&task_with_content_digest)
                .is_err()
        );
        let mut unknown = request.clone();
        unknown.reference_name = "unknown".into();
        assert!(
            unknown
                .validate_for_task(&task_with_content_digest)
                .is_err()
        );
        let mut stale_schema = request.clone();
        stale_schema.expected_schema_version = 2;
        assert!(stale_schema.validate().is_err());
        let mut wrong_content = available;
        if let ReferenceFetchResponse::Available { content, .. } = &mut wrong_content {
            *content = "changed".into();
        }
        assert!(wrong_content.validate_for_request(&request).is_err());
        let cross_response = ReferenceFetchResponse::Denied {
            task_id: "other-task".into(),
            task_digest: request.task_digest.clone(),
            reference_name: request.reference_name.clone(),
        };
        assert!(cross_response.validate_for_request(&request).is_err());
        assert!(matches!(
            ReferenceFetchResponse::from_slice(&vec![b' '; MAX_REFERENCE_ENVELOPE_BYTES + 1]),
            Err(Error::Limit(_))
        ));
    }

    #[test]
    fn recovery_matrix_requires_confirmed_effects_or_safe_supersession() {
        let mut confirmed = recovery();
        confirmed.disposition = RecoveryDisposition::Confirmed;
        confirmed.observed_state = AttemptObservedState::Finished;
        confirmed.confirmed_effects = vec![ConfirmedEffect::AssertionRetained {
            assertion_digest: "3".repeat(64),
        }];
        confirmed.unresolved_effects.clear();
        confirmed.next_action = RecoveryNextAction::Stop;
        confirmed.validate().unwrap();

        let mut rejected = recovery();
        rejected.disposition = RecoveryDisposition::ConclusiveRejection;
        rejected.confirmed_effects.clear();
        rejected.unresolved_effects.clear();
        rejected.observed_state = AttemptObservedState::Pending;
        rejected.next_action = RecoveryNextAction::RetryIdenticalSafeRequest;
        rejected.validate().unwrap();
        for state in [
            AttemptObservedState::Finished,
            AttemptObservedState::Replaced,
            AttemptObservedState::Cancelled,
            AttemptObservedState::Expired,
            AttemptObservedState::Leased,
            AttemptObservedState::Unknown,
        ] {
            let mut existing = rejected.clone();
            existing.observed_state = state;
            assert!(existing.validate().is_err());
            for action in [RecoveryNextAction::InspectStatus, RecoveryNextAction::Stop] {
                existing.next_action = action;
                existing.validate().unwrap();
            }
        }
        rejected.confirmed_effects = confirmed.confirmed_effects.clone();
        assert!(rejected.validate().is_err());

        let mut replaced = recovery();
        replaced.observed_state = AttemptObservedState::Replaced;
        replaced.confirmed_effects = vec![ConfirmedEffect::FenceSuperseded {
            old_fence: 3,
            superseding_fence: 4,
        }];
        replaced.next_action = RecoveryNextAction::PrepareFreshAttempt;
        replaced.validate().unwrap();
        let mut unreplaced = replaced.clone();
        unreplaced.observed_state = AttemptObservedState::Expired;
        assert!(unreplaced.validate().is_err());
        let mut custody = replaced;
        custody
            .unresolved_effects
            .push(UnresolvedEffect::ResourceCustody {
                custody_id: "workspace-1".into(),
            });
        assert!(custody.validate().is_err());
        let mut retry_uncertain = recovery();
        retry_uncertain.next_action = RecoveryNextAction::RetryIdenticalSafeRequest;
        assert!(retry_uncertain.validate().is_err());

        let task = task();
        let enrollment = enrollment(&task);
        let attempt = attempt(&task, &enrollment);
        let mut exact = recovery();
        exact.execution_operation_id = attempt.execution_operation_id.clone();
        exact.task_id = attempt.task_id.clone();
        exact.task_digest = attempt.task_digest.clone();
        exact.attempt_id = attempt.attempt_id.clone();
        exact.attempt_digest = attempt.digest().unwrap();
        exact.fence = attempt.lease.fence;
        exact.validate_for_attempt(&attempt).unwrap();
        exact.task_digest = "f".repeat(64);
        assert!(exact.validate_for_attempt(&attempt).is_err());
    }

    #[test]
    fn timestamps_and_schema_mismatch_are_strict() {
        let task = task();
        let enrollment = enrollment(&task);
        let mut attempt = attempt(&task, &enrollment);
        attempt.lease.issued_at_ms = 0;
        assert!(attempt.validate().is_err());

        let response = InstructionEntrypointResponse::SchemaMismatch {
            capability: AGENT_TASK_CAPABILITY.into(),
            required_schema_version: 2,
            supported_schema_versions: vec![1],
        };
        assert!(
            InstructionEntrypointResponse::from_slice(&serde_json::to_vec(&response).unwrap())
                .is_ok()
        );
    }
}
