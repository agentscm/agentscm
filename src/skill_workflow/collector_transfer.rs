//! Strict records for operator-enrolled collector execution.
//!
//! These wire records are distinct from agent assertions. A signature attributes the
//! original typed collector records to an enrolled key; only the native authority may
//! validate them and assign an admission trust class.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use ed25519_dalek::{Signature, VerifyingKey};
#[cfg(unix)]
use ed25519_dalek::{Signer, SigningKey};
#[cfg(unix)]
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checks::CheckSubject;
use crate::execution::{CommandRecipe, ExecutionOutcome, ExecutionRecord, ExecutionRequest};
use crate::reviewer::{
    MAX_ROW_BYTES, ReviewLimits, ReviewRecord, ReviewRequest, ReviewSource, ReviewSourceBinding,
    ReviewSourceObservation, ReviewSourceSelector, SourceReadFailure, SourceRendering,
    TransportBasis,
};
use crate::store::Engine;
use crate::{Error, Result};

use super::{
    AuthenticatedAgentIdentity, CompiledPlan, ExecutorBinding, StepAction, TaskClaimRequest,
    WorkflowTask, WorkflowTaskAttempt,
};

pub const COLLECTOR_TRANSFER_CAPABILITY: &str = "agentscm.collector-record-transfer/v1";
pub const COLLECTOR_TRANSFER_VERSION: u32 = 1;
pub const MAX_COLLECTOR_TRANSFER_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROFILE_BYTES: usize = 512 * 1024;
const TRANSFER_DOMAIN: &[u8] = b"agentscm.collector-record-transfer.v1\0";
const PROFILE_DOMAIN: &[u8] = b"agentscm.collector-profile.v1\0";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollectorActionKind {
    Command,
    FiniteReview,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CollectorProfileAction {
    Command {
        recipe: CommandRecipe,
        runtime_scope_sha256: String,
    },
    FiniteReview {
        executable_sha256: String,
        provider_scope_sha256: String,
        transport_basis: TransportBasis,
        model: String,
        installed_policy_sha256: String,
        required_paths: Vec<String>,
        limits: ReviewLimits,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollectorProfile {
    pub version: u32,
    pub profile_id: String,
    pub authority_audience: String,
    pub repository: String,
    pub workflow_profile: String,
    pub step_id: String,
    pub registration_id: String,
    pub registration_epoch: u64,
    pub verification_key_id: String,
    pub action: CollectorProfileAction,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OriginGrantBinding {
    pub actor: AuthenticatedAgentIdentity,
    pub grant_id: String,
    pub grant_generation: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollectorClaimRequest {
    pub claim: TaskClaimRequest,
    pub collector_profile_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollectorWorkDescriptor {
    pub version: u32,
    pub capability: String,
    pub profile: CollectorProfile,
    pub task: WorkflowTask,
    pub attempt: WorkflowTaskAttempt,
    pub origin: OriginGrantBinding,
}

impl CollectorWorkDescriptor {
    pub fn validate(&self) -> Result<()> {
        if self.version != COLLECTOR_TRANSFER_VERSION
            || self.capability != COLLECTOR_TRANSFER_CAPABILITY
        {
            return Err(Error::Invalid(
                "unsupported collector descriptor contract".into(),
            ));
        }
        self.profile.canonical_bytes()?;
        self.task.canonical_bytes()?;
        self.attempt.digest()?;
        identifier(&self.origin.grant_id)?;
        if self.origin.grant_generation == 0 || self.origin.actor != self.attempt.agent {
            return Err(Error::Invalid(
                "collector descriptor origin does not bind the attempt".into(),
            ));
        }
        Ok(())
    }
}

impl CollectorClaimRequest {
    pub fn digest(&self) -> Result<String> {
        identifier(&self.collector_profile_id)?;
        domain_digest(b"agentscm.collector-claim.v1\0", &serde_json::to_vec(self)?)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollectorRecordKind {
    ExecutionRequest,
    ExecutionRecord,
    ReviewRequest,
    ReviewRecord,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollectorRecordCommitment {
    pub kind: CollectorRecordKind,
    pub length: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CollectorPayload {
    Command {
        request: ExecutionRequest,
        record: Box<ExecutionRecord>,
    },
    Review {
        request: ReviewRequest,
        record: Box<ReviewRecord>,
    },
}

/// Observations made by the concrete local adapter, covered by the transfer signature.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CollectorIdentity {
    Command {
        runtime_identity: String,
    },
    Review {
        executable_sha256: String,
        provider_scope_sha256: String,
    },
}

pub fn runtime_scope_digest(identity: &str) -> String {
    format!("{:x}", Sha256::digest(identity.as_bytes()))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollectorTransferHeader {
    pub observed_identity: CollectorIdentity,
    pub version: u32,
    pub capability: String,
    pub authority_audience: String,
    pub repository: String,
    pub workflow_profile: String,
    pub collector_profile_id: String,
    pub collector_profile_digest: String,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub attempt_digest: String,
    pub execution_operation_id: String,
    pub fence: u64,
    pub origin: OriginGrantBinding,
    pub executor: ExecutorBinding,
    pub immutable_request_digest: String,
    pub transfer_request_id: String,
    pub records: Vec<CollectorRecordCommitment>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollectorTransferSignature {
    pub algorithm: String,
    pub verification_key_id: String,
    pub transfer_digest: String,
    pub signature_hex: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImportedStepDependency {
    pub transfer_digest: String,
    pub task_id: String,
    pub task_digest: String,
    pub step_id: String,
    pub attempt_id: String,
    pub attempt_digest: String,
    pub fence: u64,
    pub registration_id: String,
    pub registration_epoch: u64,
    pub verification_key_id: String,
    pub collector_profile_id: String,
    pub collector_profile_digest: String,
    pub origin: OriginGrantBinding,
    pub trust_class: ImportedTrustClass,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportedTrustClass {
    EnrolledCommandCollector,
    EnrolledFiniteReviewController,
}

impl ImportedStepDependency {
    pub fn from_transfer(transfer: &CollectorTransfer, task: &WorkflowTask) -> Result<Self> {
        transfer.validate_shape()?;
        if transfer.header.task_id != task.task_id
            || transfer.header.task_digest != task.digest()?
        {
            return Err(Error::Invalid(
                "imported dependency task differs from its transfer".into(),
            ));
        }
        Ok(Self {
            transfer_digest: transfer.digest()?,
            task_id: transfer.header.task_id.clone(),
            task_digest: transfer.header.task_digest.clone(),
            step_id: task.subject.step_id.clone(),
            attempt_id: transfer.header.attempt_id.clone(),
            attempt_digest: transfer.header.attempt_digest.clone(),
            fence: transfer.header.fence,
            registration_id: transfer.header.executor.registration_id.clone(),
            registration_epoch: transfer.header.executor.registration_epoch,
            verification_key_id: transfer.header.executor.verification_key_id.clone(),
            collector_profile_id: transfer.header.collector_profile_id.clone(),
            collector_profile_digest: transfer.header.collector_profile_digest.clone(),
            origin: transfer.header.origin.clone(),
            trust_class: match &transfer.payload {
                CollectorPayload::Command { .. } => ImportedTrustClass::EnrolledCommandCollector,
                CollectorPayload::Review { .. } => {
                    ImportedTrustClass::EnrolledFiniteReviewController
                }
            },
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollectorTransfer {
    pub header: CollectorTransferHeader,
    pub payload: CollectorPayload,
    pub signature: Option<CollectorTransferSignature>,
}

impl CollectorProfile {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_PROFILE_BYTES {
            return Err(Error::Limit("collector profile exceeds 512 KiB".into()));
        }
        Ok(bytes)
    }

    pub fn digest(&self) -> Result<String> {
        domain_digest(PROFILE_DOMAIN, &self.canonical_bytes()?)
    }

    pub fn action_kind(&self) -> CollectorActionKind {
        match &self.action {
            CollectorProfileAction::Command { .. } => CollectorActionKind::Command,
            CollectorProfileAction::FiniteReview { .. } => CollectorActionKind::FiniteReview,
        }
    }

    pub fn validate_for(
        &self,
        task: &WorkflowTask,
        executor: &ExecutorBinding,
        audience: &str,
    ) -> Result<()> {
        self.validate()?;
        if self.authority_audience != audience
            || self.repository != task.subject.repository
            || self.workflow_profile != task.subject.profile
            || self.step_id != task.subject.step_id
            || self.registration_id != executor.registration_id
            || self.registration_epoch != executor.registration_epoch
            || self.verification_key_id != executor.verification_key_id
        {
            return Err(Error::Conflict(
                "collector profile does not bind this authority, task, and registration".into(),
            ));
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.version != COLLECTOR_TRANSFER_VERSION {
            return Err(Error::Invalid(
                "unsupported collector profile version".into(),
            ));
        }
        for value in [
            &self.profile_id,
            &self.authority_audience,
            &self.workflow_profile,
            &self.step_id,
            &self.registration_id,
            &self.verification_key_id,
        ] {
            identifier(value)?;
        }
        repository(&self.repository)?;
        if self.registration_epoch == 0 {
            return Err(Error::Invalid(
                "collector registration epoch must be positive".into(),
            ));
        }
        match &self.action {
            CollectorProfileAction::Command {
                recipe,
                runtime_scope_sha256,
            } => {
                recipe.validate()?;
                digest(runtime_scope_sha256)?;
            }
            CollectorProfileAction::FiniteReview {
                executable_sha256,
                provider_scope_sha256,
                model,
                installed_policy_sha256,
                required_paths,
                limits,
                transport_basis,
            } => {
                if *transport_basis != TransportBasis::ClaudeCode {
                    return Err(Error::Invalid(
                        "admission review requires qualified Claude Code transport".into(),
                    ));
                }
                for value in [
                    executable_sha256,
                    provider_scope_sha256,
                    installed_policy_sha256,
                ] {
                    digest(value)?;
                }
                identifier(model)?;
                limits.validate()?;
                if required_paths.is_empty() || required_paths.len() > 128 {
                    return Err(Error::Limit(
                        "collector review profile requires 1 to 128 paths".into(),
                    ));
                }
                let mut seen = HashSet::new();
                for path in required_paths {
                    portable_path(path)?;
                    if !seen.insert(path) {
                        return Err(Error::Invalid("duplicate collector review path".into()));
                    }
                }
            }
        }
        Ok(())
    }
}

impl CollectorTransfer {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_COLLECTOR_TRANSFER_BYTES {
            return Err(Error::Limit("collector transfer exceeds 16 MiB".into()));
        }
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate_shape()?;
        if value.canonical_bytes()? != bytes {
            return Err(Error::Invalid("collector transfer is not canonical".into()));
        }
        Ok(value)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate_shape()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_COLLECTOR_TRANSFER_BYTES {
            return Err(Error::Limit("collector transfer exceeds 16 MiB".into()));
        }
        Ok(bytes)
    }

    pub fn unsigned_digest(&self) -> Result<String> {
        let mut unsigned = self.clone();
        unsigned.signature = None;
        domain_digest(TRANSFER_DOMAIN, &serde_json::to_vec(&unsigned)?)
    }

    pub fn digest(&self) -> Result<String> {
        domain_digest(TRANSFER_DOMAIN, &self.canonical_bytes()?)
    }

    pub fn verify_signature(&self, public_key_hex: &str) -> Result<()> {
        let signature = self
            .signature
            .as_ref()
            .ok_or_else(|| Error::Invalid("collector transfer is unsigned".into()))?;
        if signature.algorithm != "ed25519"
            || signature.verification_key_id != self.header.executor.verification_key_id
            || signature.transfer_digest != self.unsigned_digest()?
        {
            return Err(Error::Invalid(
                "collector transfer signature metadata is inconsistent".into(),
            ));
        }
        let key_bytes = decode_hex::<32>(public_key_hex, "collector public key")?;
        let key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|_| Error::Invalid("invalid collector public key".into()))?;
        if key.is_weak() {
            return Err(Error::Invalid("weak collector public key".into()));
        }
        let signed = decode_hex::<32>(&signature.transfer_digest, "collector transfer digest")?;
        let signature_bytes = decode_hex::<64>(&signature.signature_hex, "collector signature")?;
        let signature = Signature::from_bytes(&signature_bytes);
        key.verify_strict(&signed, &signature)
            .map_err(|_| Error::Invalid("collector transfer signature verification failed".into()))
    }

    pub fn validate_for(
        &self,
        task: &WorkflowTask,
        attempt: &WorkflowTaskAttempt,
        profile: &CollectorProfile,
        authority_audience: &str,
        origin: &OriginGrantBinding,
    ) -> Result<()> {
        self.validate_shape()?;
        self.validate_profile_identity(profile)?;
        profile.validate_for(task, &attempt.executor, authority_audience)?;
        if self.header.authority_audience != authority_audience
            || self.header.repository != task.subject.repository
            || self.header.workflow_profile != task.subject.profile
            || self.header.collector_profile_id != profile.profile_id
            || self.header.collector_profile_digest != profile.digest()?
            || self.header.task_id != task.task_id
            || self.header.task_digest != task.digest()?
            || self.header.attempt_id != attempt.attempt_id
            || self.header.attempt_digest != attempt.digest()?
            || self.header.execution_operation_id != attempt.execution_operation_id
            || self.header.fence != attempt.lease.fence
            || self.header.origin != *origin
            || self.header.origin.actor != attempt.agent
            || self.header.executor != attempt.executor
        {
            return Err(Error::Conflict(
                "collector transfer does not bind the exact current task attempt".into(),
            ));
        }
        let request_digest = match &self.payload {
            CollectorPayload::Command { request, .. } => request.digest()?,
            CollectorPayload::Review { request, .. } => request.digest()?,
        };
        if self.header.immutable_request_digest != request_digest {
            return Err(Error::Invalid(
                "collector transfer request commitment is inconsistent".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_profile_identity(&self, profile: &CollectorProfile) -> Result<()> {
        profile.validate()?;
        let matches = match (
            &profile.action,
            &self.header.observed_identity,
            &self.payload,
        ) {
            (
                CollectorProfileAction::Command {
                    runtime_scope_sha256,
                    ..
                },
                CollectorIdentity::Command { runtime_identity },
                CollectorPayload::Command { record, .. },
            ) => {
                runtime_scope_digest(runtime_identity) == *runtime_scope_sha256
                    && record.runtime_identity.as_ref() == Some(runtime_identity)
            }
            (
                CollectorProfileAction::FiniteReview {
                    executable_sha256,
                    provider_scope_sha256,
                    transport_basis,
                    ..
                },
                CollectorIdentity::Review {
                    executable_sha256: observed,
                    provider_scope_sha256: scope,
                },
                CollectorPayload::Review { record, .. },
            ) => {
                observed == executable_sha256
                    && scope == provider_scope_sha256
                    && record.calls.iter().all(|call| {
                        call.reservation.transport_basis == *transport_basis
                            && call.observation.basis == *transport_basis
                    })
                    && record
                        .unconfirmed_reservation
                        .as_ref()
                        .is_none_or(|r| r.transport_basis == *transport_basis)
            }
            _ => false,
        };
        if !matches {
            return Err(Error::Conflict(
                "observed collector identity differs from installed profile".into(),
            ));
        }
        Ok(())
    }

    fn validate_shape(&self) -> Result<()> {
        let header = &self.header;
        if header.version != COLLECTOR_TRANSFER_VERSION
            || header.capability != COLLECTOR_TRANSFER_CAPABILITY
            || header.fence == 0
        {
            return Err(Error::Invalid(
                "unsupported collector transfer header".into(),
            ));
        }
        for value in [
            &header.authority_audience,
            &header.workflow_profile,
            &header.collector_profile_id,
            &header.task_id,
            &header.attempt_id,
            &header.execution_operation_id,
            &header.origin.actor.owner_id,
            &header.origin.actor.agent_id,
            &header.origin.grant_id,
            &header.executor.registration_id,
            &header.executor.executor_id,
            &header.executor.verification_key_id,
            &header.transfer_request_id,
        ] {
            identifier(value)?;
        }
        repository(&header.repository)?;
        if header.origin.grant_generation == 0
            || header.executor.registration_epoch == 0
            || header.records.len() != 2
        {
            return Err(Error::Invalid(
                "collector transfer has invalid generations or record count".into(),
            ));
        }
        for value in [
            &header.collector_profile_digest,
            &header.task_digest,
            &header.attempt_digest,
            &header.immutable_request_digest,
        ] {
            digest(value)?;
        }
        let expected = payload_commitments(&self.payload)?;
        if header.records != expected {
            return Err(Error::Invalid(
                "collector transfer manifest does not match its typed payload".into(),
            ));
        }
        match &self.payload {
            CollectorPayload::Command { request, record } => {
                request.validate()?;
                record.validate()?;
                if record.request != *request {
                    return Err(Error::Invalid(
                        "execution record does not contain the transferred request".into(),
                    ));
                }
            }
            CollectorPayload::Review { request, record } => {
                request.validate()?;
                if record.version != 1
                    || record.request != *request
                    || record.request_sha256 != request.digest()?
                {
                    return Err(Error::Invalid(
                        "review record does not contain the transferred request".into(),
                    ));
                }
            }
        }
        if let Some(signature) = &self.signature {
            if signature.algorithm != "ed25519" {
                return Err(Error::Invalid(
                    "unsupported collector signature algorithm".into(),
                ));
            }
            identifier(&signature.verification_key_id)?;
            digest(&signature.transfer_digest)?;
            decode_hex::<64>(&signature.signature_hex, "collector signature")?;
        }
        Ok(())
    }
}

/// Authority-derived immutable command descriptor for a claimed collector attempt.
pub fn expected_command_request(
    plan: &CompiledPlan,
    task: &WorkflowTask,
    attempt: &WorkflowTaskAttempt,
    profile: &CollectorProfile,
) -> Result<ExecutionRequest> {
    let step = exact_step(plan, task)?;
    let StepAction::Command {
        change,
        change_version,
        context_digest,
        ..
    } = &step.action
    else {
        return Err(Error::Invalid(
            "command collector profile is attached to a non-command step".into(),
        ));
    };
    let CollectorProfileAction::Command { recipe, .. } = &profile.action else {
        return Err(Error::Conflict(
            "collector profile action does not match command task".into(),
        ));
    };
    let compiled = plan
        .command_recipe(&step.id)
        .ok_or_else(|| Error::Invalid("compiled command recipe is missing".into()))?;
    if compiled != recipe {
        return Err(Error::Conflict(
            "collector profile command recipe differs from installed workflow".into(),
        ));
    }
    let request = ExecutionRequest {
        attempt_id: attempt.attempt_id.clone(),
        subject: CheckSubject {
            repository: task.subject.repository.clone(),
            change: change.clone(),
            change_version: *change_version,
            context_digest: context_digest.clone(),
            revision: task.subject.revision.clone(),
            recipe_sha256: recipe.digest()?,
            toolchain_sha256: recipe.toolchain_sha256.clone(),
            environment_sha256: recipe.environment_sha256.clone(),
        },
        run_generation: attempt.lease.fence,
        recipe: recipe.clone(),
    };
    request.validate()?;
    Ok(request)
}

pub fn validate_command_record(
    expected: &ExecutionRequest,
    record: &ExecutionRecord,
) -> Result<()> {
    record.validate()?;
    if record.request != *expected || record.request_sha256 != expected.digest()? {
        return Err(Error::Conflict(
            "collector command record differs from the authority-derived request".into(),
        ));
    }
    Ok(())
}

pub fn command_record_succeeded(record: &ExecutionRecord) -> bool {
    record.outcome == ExecutionOutcome::Success
        && record.exit_code == Some(0)
        && !record.output_truncated
        && record.cleanup_confirmed
        && record.cleanup_handle.is_none()
        && record.isolation_observed
        && record.runtime_identity.is_some()
}

pub struct CollectorReviewDescriptor {
    pub request: ReviewRequest,
    source: BTreeMap<String, ReviewSourceObservation>,
}

impl ReviewSource for CollectorReviewDescriptor {
    fn read(
        &mut self,
        request: &ReviewRequest,
        selector: &ReviewSourceSelector,
        max_bytes: usize,
    ) -> std::result::Result<ReviewSourceObservation, SourceReadFailure> {
        if request != &self.request {
            return Err(SourceReadFailure::Denied);
        }
        let row = self
            .source
            .get(&selector.path)
            .ok_or(SourceReadFailure::Missing)?;
        if row.raw.len() > max_bytes {
            return Err(SourceReadFailure::Limit);
        }
        Ok(row.clone())
    }
}

pub fn expected_review_descriptor(
    data_dir: &Path,
    plan: &CompiledPlan,
    task: &WorkflowTask,
    attempt: &WorkflowTaskAttempt,
    profile: &CollectorProfile,
) -> Result<CollectorReviewDescriptor> {
    let step = exact_step(plan, task)?;
    let StepAction::ClaudeCodeReview { paths, model } = &step.action else {
        return Err(Error::Invalid(
            "finite-review collector profile is attached to a non-review step".into(),
        ));
    };
    let CollectorProfileAction::FiniteReview {
        provider_scope_sha256,
        model: installed_model,
        installed_policy_sha256,
        required_paths,
        limits,
        ..
    } = &profile.action
    else {
        return Err(Error::Conflict(
            "collector profile action does not match review task".into(),
        ));
    };
    let expected_profile_policy = plan
        .skill_digest()
        .strip_prefix("sha256:")
        .unwrap_or(plan.skill_digest());
    if installed_model != model
        || required_paths != paths
        || installed_policy_sha256 != expected_profile_policy
    {
        return Err(Error::Conflict(
            "collector review profile differs from installed workflow".into(),
        ));
    }
    let (change, change_version, context_digest) = plan
        .ordered_steps()
        .iter()
        .find_map(|candidate| match &candidate.action {
            StepAction::Command {
                change,
                change_version,
                context_digest,
                ..
            } => Some((change, *change_version, context_digest)),
            _ => None,
        })
        .ok_or_else(|| Error::Invalid("review has no exact command context".into()))?;
    let engine = Engine::open(data_dir)?;
    let (source, budget) = engine.with_read_budget(4 * 1024 * 1024);
    let (objective, rows, selectors) = budget.finish((|| {
        let read = |path: &str, max: usize| -> Result<Vec<u8>> {
            source
                .read_regular_file_bounded(&task.subject.revision, path, max)?
                .ok_or_else(|| Error::Limit("collector review source exceeds its limit".into()))
        };
        let objective = String::from_utf8(read(&plan.definition().skill.entrypoint, 4096)?)
            .map_err(|_| Error::Invalid("collector review skill must be UTF-8".into()))?;
        let permit_bytes = serde_json::to_vec(&(
            &task.subject,
            task.digest()?,
            attempt.digest()?,
            profile.digest()?,
        ))?;
        let binding = ReviewSourceBinding {
            profile: "enrolled-collector/v1".into(),
            permit_sha256: format!("{:x}", Sha256::digest(permit_bytes)),
        };
        let mut rows = BTreeMap::new();
        let mut selectors = Vec::new();
        let mut total = 0usize;
        for path in required_paths {
            let bytes = read(path, MAX_ROW_BYTES)?;
            total = total
                .checked_add(bytes.len())
                .ok_or_else(|| Error::Limit("collector source byte count overflow".into()))?;
            if total > 64 * 1024 {
                return Err(Error::Limit(
                    "collector review source exceeds 64 KiB profile".into(),
                ));
            }
            let text = String::from_utf8(bytes.clone())
                .map_err(|_| Error::Invalid("collector review source must be UTF-8".into()))?;
            let lines = text.lines().count();
            if lines == 0 || lines > 16_384 {
                return Err(Error::Limit(
                    "collector review source has unsupported line count".into(),
                ));
            }
            let raw_sha256 = format!("{:x}", Sha256::digest(&bytes));
            selectors.push(ReviewSourceSelector {
                source: binding.clone(),
                path: path.clone(),
                start_line: 1,
                end_line: lines as u32,
                raw_sha256: raw_sha256.clone(),
                rendered_sha256: raw_sha256,
                required: true,
            });
            rows.insert(
                path.clone(),
                ReviewSourceObservation {
                    source: binding.clone(),
                    raw: bytes,
                    rendered: text,
                    rendering: SourceRendering::ExactUtf8,
                },
            );
        }
        Ok((objective, rows, selectors))
    })())?;
    let recipe = crate::reviewer::ReviewRecipe {
        version: 1,
        model: installed_model.clone(),
        objective,
        provider_scope_sha256: provider_scope_sha256.clone(),
        installed_policy_sha256: plan
            .digest()
            .strip_prefix("sha256:")
            .unwrap_or(plan.digest())
            .into(),
        policy_epoch: task.subject.policy_epoch,
        source: selectors,
        limits: limits.clone(),
    };
    let request = ReviewRequest {
        attempt_id: attempt.attempt_id.clone(),
        run_generation: attempt.lease.fence,
        subject: CheckSubject {
            repository: task.subject.repository.clone(),
            change: change.clone(),
            change_version,
            context_digest: context_digest.clone(),
            revision: task.subject.revision.clone(),
            recipe_sha256: recipe.digest()?,
            toolchain_sha256: format!(
                "{:x}",
                Sha256::digest(b"agentscm.claude-code.tools-disabled/v1")
            ),
            environment_sha256: provider_scope_sha256.clone(),
        },
        recipe,
    };
    request.validate()?;
    Ok(CollectorReviewDescriptor {
        request,
        source: rows,
    })
}

fn exact_step<'a>(
    plan: &'a CompiledPlan,
    task: &WorkflowTask,
) -> Result<&'a super::StepDefinition> {
    if task.subject.plan_digest != plan.digest()
        || task.subject.skill_digest != plan.skill_digest()
        || task.subject.revision != plan.definition().source.revision
        || task.subject.policy_epoch != plan.definition().source.policy_epoch
    {
        return Err(Error::Conflict(
            "collector task does not bind the compiled installed plan".into(),
        ));
    }
    plan.ordered_steps()
        .iter()
        .find(|step| step.id == task.subject.step_id)
        .ok_or_else(|| Error::Invalid("collector task step is not in the plan".into()))
}

fn payload_commitments(payload: &CollectorPayload) -> Result<Vec<CollectorRecordCommitment>> {
    let values = match payload {
        CollectorPayload::Command { request, record } => vec![
            (
                CollectorRecordKind::ExecutionRequest,
                request.canonical_bytes()?,
            ),
            (
                CollectorRecordKind::ExecutionRecord,
                record.canonical_bytes()?,
            ),
        ],
        CollectorPayload::Review { request, record } => vec![
            (
                CollectorRecordKind::ReviewRequest,
                serde_json::to_vec(request)?,
            ),
            (
                CollectorRecordKind::ReviewRecord,
                serde_json::to_vec(record)?,
            ),
        ],
    };
    values
        .into_iter()
        .map(|(kind, bytes)| {
            Ok(CollectorRecordCommitment {
                kind,
                length: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
            })
        })
        .collect()
}

fn domain_digest(domain: &[u8], bytes: &[u8]) -> Result<String> {
    let length = u64::try_from(bytes.len())
        .map_err(|_| Error::Limit("collector canonical byte length overflow".into()))?;
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(length.to_be_bytes());
    hash.update(bytes);
    Ok(format!("{:x}", hash.finalize()))
}

fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
        })
    {
        return Err(Error::Invalid("invalid collector identifier".into()));
    }
    Ok(())
}

fn repository(value: &str) -> Result<()> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(
            "invalid collector repository identity".into(),
        ));
    }
    Ok(())
}

fn digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid("invalid lowercase collector digest".into()));
    }
    Ok(())
}

fn portable_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 1024
        || !value.is_ascii()
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'\\' | b':'))
    {
        return Err(Error::Invalid("invalid collector source path".into()));
    }
    Ok(())
}

fn decode_hex<const N: usize>(value: &str, kind: &str) -> Result<[u8; N]> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(format!("invalid {kind}")));
    }
    let mut output = [0; N];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| Error::Invalid(format!("invalid {kind}")))?;
    }
    Ok(output)
}

/// Operator-controlled collector process core. It accepts only authority-derived task
/// descriptors, executes the pinned collector, durably records possible effects, and returns
/// an already-persisted signed transfer. It exposes no generic JSON-signing operation.
#[cfg(unix)]
pub struct TrustedLocalCollector {
    profile: CollectorProfile,
    signing_key: SigningKey,
    state_dir: std::path::PathBuf,
}

#[cfg(unix)]
impl TrustedLocalCollector {
    pub fn new(
        state_dir: &Path,
        profile: CollectorProfile,
        signing_key: SigningKey,
    ) -> Result<Self> {
        profile.canonical_bytes()?;
        crate::local_recovery::create_private_directory(state_dir)?;
        require_private_path(state_dir, true)?;
        let state_dir = std::fs::canonicalize(state_dir)?;
        let snapshots = state_dir.join("snapshots");
        crate::local_recovery::create_private_directory(&snapshots)?;
        require_private_path(&snapshots, true)?;
        let public_hex = signing_key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if crate::policy::issuer_key_id(&public_hex)? != profile.verification_key_id {
            return Err(Error::Conflict(
                "collector signing key does not match operator profile".into(),
            ));
        }
        let collector = Self {
            profile,
            signing_key,
            state_dir: state_dir.to_owned(),
        };
        collector.open_journal()?;
        Ok(collector)
    }

    /// Returns the exact signed transfer retained before upload. It never executes work.
    pub fn retained_transfer(&self, transfer_request_id: &str) -> Result<CollectorTransfer> {
        identifier(transfer_request_id)?;
        let mut journal = self.open_journal()?;
        let transaction = journal.transaction()?;
        let stored_length: i64 = transaction
            .query_row(
                "SELECT length(canonical_json) FROM collector_transfers WHERE transfer_request_id=?1",
                [transfer_request_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("collector transfer {transfer_request_id}")))?;
        if stored_length < 0
            || usize::try_from(stored_length)
                .map_or(true, |length| length > MAX_COLLECTOR_TRANSFER_BYTES)
        {
            return Err(Error::Limit(
                "retained collector transfer exceeds 16 MiB".into(),
            ));
        }
        let bytes: Vec<u8> = transaction.query_row(
            "SELECT canonical_json FROM collector_transfers WHERE transfer_request_id=?1",
            [transfer_request_id],
            |row| row.get(0),
        )?;
        let transfer = CollectorTransfer::from_slice(&bytes)?;
        transaction.commit()?;
        Ok(transfer)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn collect_command(
        &self,
        data_dir: &Path,
        plan: &CompiledPlan,
        task: &WorkflowTask,
        attempt: &WorkflowTaskAttempt,
        origin: OriginGrantBinding,
        transfer_request_id: &str,
        backend: &crate::execution::OciBackend,
        cancellation: &crate::execution::Cancellation,
        dispatch_guard: &(dyn Fn() -> Result<()> + Send + Sync),
    ) -> Result<CollectorTransfer> {
        self.profile
            .validate_for(task, &attempt.executor, &task.subject.repository)?;
        let CollectorProfileAction::Command {
            runtime_scope_sha256: pinned_runtime,
            ..
        } = &self.profile.action
        else {
            return Err(Error::Conflict(
                "collector profile is not a command profile".into(),
            ));
        };
        let probe = backend.probe();
        let runtime_identity = probe
            .identity
            .filter(|_| probe.available)
            .ok_or_else(|| Error::Conflict(format!("OCI runtime unavailable: {}", probe.detail)))?;
        if runtime_scope_digest(&runtime_identity) != *pinned_runtime {
            return Err(Error::Conflict(
                "selected OCI runtime scope differs from collector profile".into(),
            ));
        }
        let request = expected_command_request(plan, task, attempt, &self.profile)?;
        dispatch_guard()?;
        let journal = self.open_journal()?;
        reserve_execution(&journal, attempt, &request.digest()?)?;
        let engine = Engine::open(data_dir)?;
        let snapshot_path = self.snapshot_path(attempt)?;
        let snapshot = crate::execution::Snapshot::materialize(
            &engine,
            &task.subject.revision,
            &snapshot_path,
            crate::execution::MAX_SNAPSHOT_FILES,
            crate::execution::MAX_SNAPSHOT_BYTES,
        )?;
        let mut persist = |handle: &crate::execution::CleanupHandle| {
            journal.execute(
                "UPDATE collector_executions SET custody_json=?1 WHERE execution_operation_id=?2 AND custody_json IS NULL",
                params![serde_json::to_vec(handle)?, attempt.execution_operation_id],
            )?;
            Ok(())
        };
        let record = backend.collect_with(&request, &snapshot, cancellation, &mut persist)?;
        validate_command_record(&request, &record)?;
        if record.runtime_identity.as_ref() != Some(&runtime_identity) {
            return Err(Error::Conflict(
                "OCI runtime changed during collection".into(),
            ));
        }
        dispatch_guard()?;
        let transfer = self.sign_transfer(
            task,
            attempt,
            origin,
            transfer_request_id,
            CollectorPayload::Command {
                request,
                record: Box::new(record),
            },
        )?;
        persist_local_transfer(&journal, &transfer)?;
        journal.execute(
            "UPDATE collector_executions SET state='finished' WHERE execution_operation_id=?1 AND state='reserved'",
            [&attempt.execution_operation_id],
        )?;
        Ok(transfer)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn collect_review(
        &self,
        data_dir: &Path,
        plan: &CompiledPlan,
        task: &WorkflowTask,
        attempt: &WorkflowTaskAttempt,
        origin: OriginGrantBinding,
        transfer_request_id: &str,
        transport: &crate::reviewer::ClaudeCodeTransport,
        cancellation: &crate::reviewer::ReviewCancellation,
        dispatch_guard: &(dyn Fn() -> Result<()> + Send + Sync),
    ) -> Result<CollectorTransfer> {
        let executable = transport.verified_executable_sha256()?;
        let guard = || {
            transport.verified_executable_sha256()?;
            dispatch_guard()
        };
        self.collect_review_inner(
            data_dir,
            plan,
            task,
            attempt,
            origin,
            transfer_request_id,
            &executable,
            transport,
            cancellation,
            &guard,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn collect_review_inner<T: crate::reviewer::ModelTransport + Sync>(
        &self,
        data_dir: &Path,
        plan: &CompiledPlan,
        task: &WorkflowTask,
        attempt: &WorkflowTaskAttempt,
        origin: OriginGrantBinding,
        transfer_request_id: &str,
        executable_sha256: &str,
        transport: &T,
        cancellation: &crate::reviewer::ReviewCancellation,
        dispatch_guard: &(dyn Fn() -> Result<()> + Send + Sync),
    ) -> Result<CollectorTransfer> {
        self.profile
            .validate_for(task, &attempt.executor, &task.subject.repository)?;
        let CollectorProfileAction::FiniteReview {
            executable_sha256: pinned_executable,
            provider_scope_sha256,
            transport_basis,
            ..
        } = &self.profile.action
        else {
            return Err(Error::Conflict(
                "collector profile is not a review profile".into(),
            ));
        };
        if executable_sha256 != pinned_executable
            || transport.scope_sha256() != *provider_scope_sha256
            || transport.basis() != *transport_basis
        {
            return Err(Error::Conflict(
                "selected review transport differs from collector profile".into(),
            ));
        }
        let mut descriptor =
            expected_review_descriptor(data_dir, plan, task, attempt, &self.profile)?;
        let request = descriptor.request.clone();
        let connection = self.open_journal()?;
        let mut journal = LocalReviewJournal {
            connection: &connection,
            attempt,
            dispatch_guard,
        };
        let guarded = LocalGuardedTransport {
            inner: transport,
            dispatch_guard,
        };
        let record = crate::reviewer::ReviewController::run(
            request.clone(),
            &mut descriptor,
            &mut journal,
            &guarded,
            cancellation,
        )
        .await?;
        dispatch_guard()?;
        let transfer = self.sign_transfer(
            task,
            attempt,
            origin,
            transfer_request_id,
            CollectorPayload::Review {
                request,
                record: Box::new(record),
            },
        )?;
        persist_local_transfer(&connection, &transfer)?;
        Ok(transfer)
    }

    fn sign_transfer(
        &self,
        task: &WorkflowTask,
        attempt: &WorkflowTaskAttempt,
        origin: OriginGrantBinding,
        transfer_request_id: &str,
        payload: CollectorPayload,
    ) -> Result<CollectorTransfer> {
        identifier(transfer_request_id)?;
        if origin.actor != attempt.agent {
            return Err(Error::Conflict(
                "collector origin actor differs from claimed attempt".into(),
            ));
        }
        let immutable_request_digest = match &payload {
            CollectorPayload::Command { request, .. } => request.digest()?,
            CollectorPayload::Review { request, .. } => request.digest()?,
        };
        let mut transfer = CollectorTransfer {
            header: CollectorTransferHeader {
                observed_identity: match (&self.profile.action, &payload) {
                    (
                        CollectorProfileAction::Command { .. },
                        CollectorPayload::Command { record, .. },
                    ) => CollectorIdentity::Command {
                        runtime_identity: record
                            .runtime_identity
                            .clone()
                            .ok_or_else(|| Error::Invalid("missing observed runtime".into()))?,
                    },
                    (
                        CollectorProfileAction::FiniteReview {
                            executable_sha256,
                            provider_scope_sha256,
                            ..
                        },
                        CollectorPayload::Review { .. },
                    ) => CollectorIdentity::Review {
                        executable_sha256: executable_sha256.clone(),
                        provider_scope_sha256: provider_scope_sha256.clone(),
                    },
                    _ => return Err(Error::Invalid("collector action mismatch".into())),
                },
                version: COLLECTOR_TRANSFER_VERSION,
                capability: COLLECTOR_TRANSFER_CAPABILITY.into(),
                authority_audience: self.profile.authority_audience.clone(),
                repository: task.subject.repository.clone(),
                workflow_profile: task.subject.profile.clone(),
                collector_profile_id: self.profile.profile_id.clone(),
                collector_profile_digest: self.profile.digest()?,
                task_id: task.task_id.clone(),
                task_digest: task.digest()?,
                attempt_id: attempt.attempt_id.clone(),
                attempt_digest: attempt.digest()?,
                execution_operation_id: attempt.execution_operation_id.clone(),
                fence: attempt.lease.fence,
                origin,
                executor: attempt.executor.clone(),
                immutable_request_digest,
                transfer_request_id: transfer_request_id.into(),
                records: payload_commitments(&payload)?,
            },
            payload,
            signature: None,
        };
        let transfer_digest = transfer.unsigned_digest()?;
        let raw = decode_hex::<32>(&transfer_digest, "collector transfer digest")?;
        transfer.signature = Some(CollectorTransferSignature {
            algorithm: "ed25519".into(),
            verification_key_id: self.profile.verification_key_id.clone(),
            transfer_digest,
            signature_hex: self
                .signing_key
                .sign(&raw)
                .to_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        });
        transfer.canonical_bytes()?;
        Ok(transfer)
    }

    fn snapshot_path(&self, attempt: &WorkflowTaskAttempt) -> Result<std::path::PathBuf> {
        require_private_path(&self.state_dir, true)?;
        let root = self.state_dir.join("snapshots");
        require_private_path(&root, true)?;
        Ok(root.join(attempt.digest()?))
    }

    fn open_journal(&self) -> Result<Connection> {
        require_private_path(&self.state_dir, true)?;
        require_private_path(&self.state_dir.join("snapshots"), true)?;
        for name in [
            "collector.sqlite",
            "collector.sqlite-wal",
            "collector.sqlite-shm",
            "collector.sqlite-journal",
        ] {
            let path = self.state_dir.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => require_private_path(&path, false)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let database = self.state_dir.join("collector.sqlite");
        let file = super::runtime::journal_open_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&database)?;
        require_private_path(&database, false)?;
        file.sync_all()?;
        let connection = Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS collector_executions(
                execution_operation_id TEXT PRIMARY KEY,
                request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
                state TEXT NOT NULL CHECK(state IN ('reserved','finished')),
                custody_json BLOB
             ) STRICT;
             CREATE TABLE IF NOT EXISTS collector_review_calls(
                attempt_id TEXT NOT NULL,
                call_index INTEGER NOT NULL CHECK(call_index>=0 AND call_index<16),
                reservation_digest TEXT NOT NULL CHECK(length(reservation_digest)=64),
                reservation_json BLOB NOT NULL,
                observation_json BLOB,
                PRIMARY KEY(attempt_id,call_index)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS collector_transfers(
                transfer_request_id TEXT PRIMARY KEY,
                digest TEXT NOT NULL UNIQUE CHECK(length(digest)=64),
                canonical_json BLOB NOT NULL CHECK(length(canonical_json)<=16777216)
             ) STRICT;",
        )?;
        Ok(connection)
    }
}

#[cfg(unix)]
fn require_private_path(path: &Path, directory: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    // The selected root may have a platform alias above it; no child component may be a link.
    // SAFETY: geteuid only reads the effective process identity and has no preconditions.
    let owner = unsafe { libc::geteuid() };
    if metadata.file_type().is_symlink()
        || metadata.is_dir() != directory
        || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != owner
    {
        return Err(Error::Invalid(
            "collector state must be owner-private and non-symlink".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn reserve_execution(
    connection: &Connection,
    attempt: &WorkflowTaskAttempt,
    request_digest: &str,
) -> Result<()> {
    let existing: Option<(String, String)> = connection
        .query_row(
            "SELECT request_digest,state FROM collector_executions WHERE execution_operation_id=?1",
            [&attempt.execution_operation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((digest, _)) = existing {
        if digest != request_digest {
            return Err(Error::Conflict(
                "collector execution identity is bound to different request bytes".into(),
            ));
        }
        return Err(Error::Conflict(
            "collector execution is already reserved; inspect retained state instead of rerunning"
                .into(),
        ));
    }
    connection.execute(
        "INSERT INTO collector_executions(execution_operation_id,request_digest,state) VALUES(?1,?2,'reserved')",
        params![attempt.execution_operation_id, request_digest],
    )?;
    Ok(())
}

#[cfg(unix)]
fn persist_local_transfer(connection: &Connection, transfer: &CollectorTransfer) -> Result<()> {
    let bytes = transfer.canonical_bytes()?;
    let digest = transfer.digest()?;
    connection.execute(
        "INSERT INTO collector_transfers(transfer_request_id,digest,canonical_json) VALUES(?1,?2,?3) ON CONFLICT(transfer_request_id) DO NOTHING",
        params![transfer.header.transfer_request_id, digest, bytes],
    )?;
    let stored: (String, Vec<u8>) = connection.query_row(
        "SELECT digest,canonical_json FROM collector_transfers WHERE transfer_request_id=?1",
        [&transfer.header.transfer_request_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if stored.0 != transfer.digest()? || stored.1 != transfer.canonical_bytes()? {
        return Err(Error::Conflict(
            "collector upload identity is bound to different signed bytes".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
struct LocalReviewJournal<'a> {
    connection: &'a Connection,
    attempt: &'a WorkflowTaskAttempt,
    dispatch_guard: &'a (dyn Fn() -> Result<()> + Send + Sync),
}

#[cfg(unix)]
impl crate::reviewer::ReviewJournal for LocalReviewJournal<'_> {
    fn reserve_call(
        &mut self,
        reservation: &crate::reviewer::CallReservation,
    ) -> Result<crate::reviewer::ReservationOutcome> {
        (self.dispatch_guard)()?;
        if reservation.attempt_id != self.attempt.attempt_id
            || reservation.run_generation != self.attempt.lease.fence
        {
            return Err(Error::Conflict(
                "review reservation lost its collector attempt".into(),
            ));
        }
        let digest = reservation.digest()?;
        let changed = self.connection.execute(
            "INSERT INTO collector_review_calls(attempt_id,call_index,reservation_digest,reservation_json) VALUES(?1,?2,?3,?4) ON CONFLICT(attempt_id,call_index) DO NOTHING",
            params![
                self.attempt.attempt_id,
                reservation.call_index,
                digest,
                serde_json::to_vec(reservation)?
            ],
        )?;
        if changed == 0 {
            return Ok(crate::reviewer::ReservationOutcome::Existing);
        }
        Ok(crate::reviewer::ReservationOutcome::Reserved(
            crate::reviewer::CallTicket {
                reservation_sha256: digest,
                run_generation: self.attempt.lease.fence,
                durable_version: u64::from(reservation.call_index) + 1,
            },
        ))
    }

    fn record_call(
        &mut self,
        ticket: &crate::reviewer::CallTicket,
        observation: &crate::reviewer::ModelCallObservation,
    ) -> Result<()> {
        if self.connection.execute(
            "UPDATE collector_review_calls SET observation_json=?1 WHERE attempt_id=?2 AND reservation_digest=?3 AND observation_json IS NULL",
            params![serde_json::to_vec(observation)?, self.attempt.attempt_id, ticket.reservation_sha256],
        )? != 1
        {
            return Err(Error::Conflict(
                "review observation was already recorded or fenced".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(unix)]
struct LocalGuardedTransport<'a, T> {
    inner: &'a T,
    dispatch_guard: &'a (dyn Fn() -> Result<()> + Send + Sync),
}

#[cfg(unix)]
impl<T: crate::reviewer::ModelTransport + Sync> crate::reviewer::ModelTransport
    for LocalGuardedTransport<'_, T>
{
    fn scope_sha256(&self) -> String {
        self.inner.scope_sha256()
    }

    fn basis(&self) -> TransportBasis {
        self.inner.basis()
    }

    fn invoke<'a>(
        &'a self,
        call: &'a crate::reviewer::PreparedModelCall,
        cancel: &'a crate::reviewer::ReviewCancellation,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::reviewer::ModelCallObservation> + Send + 'a>,
    > {
        Box::pin(async move {
            if (self.dispatch_guard)().is_err() {
                return crate::reviewer::ModelCallObservation::not_started(
                    self.basis(),
                    crate::reviewer::ModelFailure::Cancelled,
                );
            }
            self.inner.invoke(call, cancel).await
        })
    }
}

// Every test drives the Unix-only `TrustedLocalCollector`.
#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::context::ChangeId;
    use crate::context::{InputIdentity, RequiredEvidence, ReviewContext};
    use crate::execution::{
        COMMAND_CAPABILITY, COMMAND_EVIDENCE, ExecutionLimits, SyscallPolicy, environment_digest,
    };
    use crate::model::{Approval, FileChange};
    use crate::skill_workflow::runtime::{
        AgentTaskService, CollectorOriginAuthorizationGuard, TaskAuthorizationGuard, TaskClock,
    };
    use crate::skill_workflow::{
        AttemptLease, CriterionEvidenceRule, CriterionKind, EntrypointProgression,
        InstructionEntrypoint, RequiredCriterion, SkillReference, StepDefinition,
        WorkflowDefinition, WorkflowSource, WorkflowTaskSubject, compile,
    };

    fn private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn fixture() -> (
        tempfile::TempDir,
        TrustedLocalCollector,
        WorkflowTask,
        WorkflowTaskAttempt,
        ExecutionRequest,
        ExecutionRecord,
        OriginGrantBinding,
    ) {
        let signing = SigningKey::from_bytes(&[11; 32]);
        let public_hex = signing
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let key_id = crate::policy::issuer_key_id(&public_hex).unwrap();
        let environment = BTreeMap::new();
        let recipe = CommandRecipe {
            version: 1,
            image: format!("sha256:{}", "1".repeat(64)),
            argv: vec!["/bin/true".into()],
            environment_sha256: environment_digest(&environment).unwrap(),
            environment,
            toolchain_sha256: "2".repeat(64),
            limits: ExecutionLimits {
                timeout_ms: 1000,
                max_output_bytes: 4096,
                memory_bytes: 64 * 1024 * 1024,
                scratch_bytes: 1024 * 1024,
                pids: 8,
                cpu_millis: 500,
            },
            syscall_policy: SyscallPolicy::DockerBuiltin,
            no_new_privileges: true,
        };
        let repository = "a".repeat(32);
        let profile = CollectorProfile {
            version: 1,
            profile_id: "collector-1".into(),
            authority_audience: repository.clone(),
            repository: repository.clone(),
            workflow_profile: "trusted-local".into(),
            step_id: "tests".into(),
            registration_id: "registration-1".into(),
            registration_epoch: 1,
            verification_key_id: key_id.clone(),
            action: CollectorProfileAction::Command {
                recipe: recipe.clone(),
                runtime_scope_sha256: runtime_scope_digest("synthetic-runtime"),
            },
        };
        let task = WorkflowTask {
            version: 1,
            task_id: "task-1".into(),
            subject: WorkflowTaskSubject {
                repository: repository.clone(),
                profile: "trusted-local".into(),
                target: "main".into(),
                revision: "4".repeat(64),
                change: ChangeId {
                    repository: repository.clone(),
                    id: "change-1".into(),
                },
                change_version: 1,
                context_digest: "5".repeat(64),
                policy_epoch: 1,
                requirement_digest: "6".repeat(64),
                plan_digest: format!("sha256:{}", "7".repeat(64)),
                skill_digest: format!("sha256:{}", "8".repeat(64)),
                step_id: "tests".into(),
            },
            required: true,
            entrypoint: InstructionEntrypoint {
                capability: crate::skill_workflow::AGENT_TASK_CAPABILITY.into(),
                schema_version: 1,
                workflow_digest: format!("sha256:{}", "7".repeat(64)),
                policy_digest: "9".repeat(64),
                required_criteria: vec![RequiredCriterion {
                    id: "tests-result".into(),
                    description: "run tests".into(),
                    kind: CriterionKind::Check,
                    evidence_rule: CriterionEvidenceRule::EnrolledLaptopOrController,
                }],
                references: vec![],
                permitted_progression: EntrypointProgression::ClaimTask,
            },
        };
        let agent = AuthenticatedAgentIdentity {
            owner_id: "owner-1".into(),
            agent_id: "agent-1".into(),
        };
        let attempt = WorkflowTaskAttempt {
            version: 1,
            claim_request_id: "claim-1".into(),
            task_id: task.task_id.clone(),
            task_digest: task.digest().unwrap(),
            attempt_id: "attempt-1".into(),
            execution_operation_id: "execution-1".into(),
            agent: agent.clone(),
            executor: ExecutorBinding {
                registration_id: "registration-1".into(),
                executor_id: "executor-1".into(),
                registration_epoch: 1,
                verification_key_id: key_id,
            },
            lease: AttemptLease {
                fence: 1,
                issued_at_ms: 1,
                expires_at_ms: 1000,
            },
        };
        let request = ExecutionRequest {
            attempt_id: attempt.attempt_id.clone(),
            subject: CheckSubject {
                repository: repository.clone(),
                change: task.subject.change.clone(),
                change_version: 1,
                context_digest: task.subject.context_digest.clone(),
                revision: task.subject.revision.clone(),
                recipe_sha256: recipe.digest().unwrap(),
                toolchain_sha256: recipe.toolchain_sha256.clone(),
                environment_sha256: recipe.environment_sha256.clone(),
            },
            run_generation: 1,
            recipe,
        };
        let record = ExecutionRecord {
            version: 1,
            request: request.clone(),
            request_sha256: request.digest().unwrap(),
            outcome: ExecutionOutcome::Success,
            exit_code: Some(0),
            stdout: vec![],
            stderr: vec![],
            output_truncated: false,
            cleanup_confirmed: true,
            cleanup_handle: None,
            isolation_observed: true,
            runtime_identity: Some("synthetic-runtime".into()),
            detail: "synthetic fixture".into(),
        };
        let origin = OriginGrantBinding {
            actor: agent,
            grant_id: "grant-1".into(),
            grant_generation: 1,
        };
        let root = private_tempdir();
        let collector = TrustedLocalCollector::new(root.path(), profile, signing).unwrap();
        (root, collector, task, attempt, request, record, origin)
    }

    #[test]
    fn signed_transfer_binds_manifest_identity_and_original_records() {
        let (_root, collector, task, attempt, request, record, origin) = fixture();
        let transfer = collector
            .sign_transfer(
                &task,
                &attempt,
                origin.clone(),
                "upload-1",
                CollectorPayload::Command {
                    request,
                    record: Box::new(record),
                },
            )
            .unwrap();
        let public_hex = collector
            .signing_key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        transfer.verify_signature(&public_hex).unwrap();
        transfer
            .validate_for(
                &task,
                &attempt,
                &collector.profile,
                &task.subject.repository,
                &origin,
            )
            .unwrap();
        assert_eq!(
            CollectorTransfer::from_slice(&transfer.canonical_bytes().unwrap()).unwrap(),
            transfer
        );

        let mut altered = transfer.clone();
        let CollectorPayload::Command { record, .. } = &mut altered.payload else {
            unreachable!()
        };
        record.stdout.push(b'x');
        assert!(altered.canonical_bytes().is_err());

        let wrong = SigningKey::from_bytes(&[12; 32]);
        let wrong_hex = wrong
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert!(transfer.verify_signature(&wrong_hex).is_err());
    }

    #[test]
    fn unsigned_and_oversized_transfers_fail_closed() {
        let (_root, collector, task, attempt, request, record, origin) = fixture();
        let mut transfer = collector
            .sign_transfer(
                &task,
                &attempt,
                origin,
                "upload-1",
                CollectorPayload::Command {
                    request,
                    record: Box::new(record),
                },
            )
            .unwrap();
        transfer.signature = None;
        assert!(transfer.verify_signature(&"1".repeat(64)).is_err());
        assert!(
            CollectorTransfer::from_slice(&vec![b' '; MAX_COLLECTOR_TRANSFER_BYTES + 1]).is_err()
        );
    }

    struct RouteAuthorization {
        now: AtomicU64,
        allowed: AtomicBool,
        lock: std::sync::Mutex<()>,
        revoke_finalize: AtomicBool,
        hold_approval: AtomicBool,
        entered_approval: AtomicBool,
        restore_receipt: std::sync::Mutex<Option<std::path::PathBuf>>,
    }

    impl TaskClock for RouteAuthorization {
        fn now_ms(&self) -> Result<u64> {
            Ok(self.now.load(Ordering::SeqCst))
        }
    }

    impl TaskAuthorizationGuard for RouteAuthorization {
        fn authorize(&self, _: &AuthenticatedAgentIdentity, _: &str) -> Result<()> {
            if self.allowed.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(Error::Conflict("synthetic actor revoked".into()))
            }
        }
    }

    impl CollectorOriginAuthorizationGuard for RouteAuthorization {
        fn with_current_collector_authorization(
            &self,
            actor: &AuthenticatedAgentIdentity,
            action: crate::skill_workflow::CollectorAction,
            origins: &[OriginGrantBinding],
            repository: &str,
            transition: &mut dyn FnMut() -> Result<()>,
        ) -> Result<()> {
            let _lock = self
                .lock
                .try_lock()
                .expect("combined authorization must not nest");
            if action == crate::skill_workflow::CollectorAction::Finalize
                && self.revoke_finalize.load(Ordering::SeqCst)
            {
                return Err(Error::Conflict(
                    "synthetic revocation before finalization".into(),
                ));
            }
            if action == crate::skill_workflow::CollectorAction::Approve
                && self.hold_approval.load(Ordering::SeqCst)
            {
                self.entered_approval.store(true, Ordering::SeqCst);
                while self.hold_approval.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
            }
            if action == crate::skill_workflow::CollectorAction::Approve
                && let Some(database) = self.restore_receipt.lock().unwrap().take()
            {
                Connection::open(database)?.execute_batch("INSERT INTO workflow_receipts SELECT * FROM fixture_hidden_receipts; DROP TABLE fixture_hidden_receipts;")?;
            }
            self.authorize(actor, repository)?;
            for origin in origins {
                self.authorize(&origin.actor, repository)?;
            }
            transition()
        }
    }

    struct ReviewScript {
        scope: String,
        read_source: bool,
        calls: AtomicU64,
    }

    impl crate::reviewer::ModelTransport for ReviewScript {
        fn scope_sha256(&self) -> String {
            self.scope.clone()
        }

        fn basis(&self) -> TransportBasis {
            TransportBasis::ClaudeCode
        }

        fn invoke<'a>(
            &'a self,
            call: &'a crate::reviewer::PreparedModelCall,
            _: &'a crate::reviewer::ReviewCancellation,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = crate::reviewer::ModelCallObservation> + Send + 'a,
            >,
        > {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let body: serde_json::Value = serde_json::from_slice(call.body()).unwrap();
                let input: serde_json::Value =
                    serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
                let supplied = input["supplied_rows"].as_array().unwrap();
                let decision = if self.read_source && supplied.is_empty() {
                    crate::reviewer::ReviewDecision::RequestRows {
                        row_ids: input["catalog"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|row| row["row_id"].as_str().unwrap().to_owned())
                            .collect(),
                    }
                } else {
                    crate::reviewer::ReviewDecision::Finish {
                        findings: vec![],
                        unresolved: vec![],
                    }
                };
                let reply = crate::reviewer::ModelReply {
                    response_id: format!("reply-{}", self.calls.load(Ordering::SeqCst)),
                    model: "sonnet".into(),
                    usage: crate::reviewer::ReviewUsage {
                        basis: crate::reviewer::UsageBasis::ClaudeCodeResultV1,
                        input_tokens: 2,
                        output_tokens: 1,
                        total_tokens: 3,
                        prompt_details: Some(crate::reviewer::PromptTokenUsage {
                            cached_tokens: Some(0),
                            audio_tokens: None,
                        }),
                        completion_details: None,
                    },
                    content_sha256: format!(
                        "{:x}",
                        Sha256::digest(serde_json::to_vec(&decision).unwrap())
                    ),
                    decision,
                    system_fingerprint: None,
                    service_tier: None,
                };
                // Synthetic concrete-basis envelope for replay tests only; no provider ran.
                let captured_response = serde_json::to_vec(&serde_json::json!({
                    "type":"result", "subtype":"success", "is_error":false,
                    "session_id":reply.response_id, "num_turns":1, "structured_output":reply.decision,
                    "usage":{"input_tokens":2,"output_tokens":1},
                    "modelUsage":{"claude-sonnet-fixture":{"inputTokens":2,"outputTokens":1}}
                })).unwrap();
                crate::reviewer::ModelCallObservation {
                    state: crate::reviewer::CallState::Acknowledged,
                    basis: TransportBasis::ClaudeCode,
                    reply: Some(reply),
                    failure: None,
                    http_status: None,
                    captured_response_bytes: captured_response.len() as u64,
                    captured_response_sha256: format!("{:x}", Sha256::digest(&captured_response)),
                    captured_response,
                    response_capture_complete: true,
                }
            })
        }
    }

    fn command_context(repository: &str, revision: &str, recipe: &CommandRecipe) -> ReviewContext {
        let identity = |sha256: &str| InputIdentity::Digest {
            sha256: sha256.into(),
        };
        let recipe_digest = recipe.digest().unwrap();
        ReviewContext {
            version: 1,
            change: ChangeId {
                repository: repository.into(),
                id: "collector-change".into(),
            },
            change_version: 1,
            revision: revision.into(),
            base: None,
            target: "main".into(),
            policy_epoch: 0,
            intent: "exercise enrolled collector transfer".into(),
            acceptance_criteria: vec!["the pinned command succeeds".into()],
            scope: vec!["src/lib.rs".into()],
            exclusions: vec![],
            omissions: vec![],
            dependencies: vec![],
            recipe: identity(&recipe_digest),
            toolchain: identity(&recipe.toolchain_sha256),
            environment: identity(&recipe.environment_sha256),
            required_evidence: vec![RequiredEvidence {
                identity: COMMAND_EVIDENCE.into(),
                digest: identity(&recipe_digest),
            }],
            required_capabilities: vec![
                COMMAND_CAPABILITY.into(),
                "agentscm.review-context/v1".into(),
            ],
        }
    }

    fn result_record(request: &ExecutionRequest, success: bool) -> ExecutionRecord {
        ExecutionRecord {
            version: 1,
            request: request.clone(),
            request_sha256: request.digest().unwrap(),
            outcome: if success {
                ExecutionOutcome::Success
            } else {
                ExecutionOutcome::Failed
            },
            exit_code: Some(if success { 0 } else { 1 }),
            stdout: vec![],
            stderr: if success {
                vec![]
            } else {
                b"failed\n".to_vec()
            },
            output_truncated: false,
            cleanup_confirmed: true,
            cleanup_handle: None,
            isolation_observed: true,
            runtime_identity: Some("synthetic-runtime".into()),
            detail: if success { "passed" } else { "failed" }.into(),
        }
    }

    #[tokio::test]
    async fn command_failure_then_new_revision_success_requires_live_origin_admission() {
        run_route(None, false).await;
    }

    #[tokio::test]
    async fn optional_omitted_command_cannot_override_context_evidence() {
        run_route(Some(false), false).await;
    }

    #[tokio::test]
    async fn optional_failed_command_cannot_override_context_evidence() {
        run_route(Some(true), false).await;
    }

    #[tokio::test]
    #[ignore = "requires explicitly configured local Docker and already-present public image; never pulls"]
    async fn collector_real_docker_smoke() {
        run_route(None, true).await;
    }

    async fn run_route(optional: Option<bool>, docker_smoke: bool) {
        let root = private_tempdir();
        let environment = if docker_smoke {
            BTreeMap::from([(
                "PATH".into(),
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
            )])
        } else {
            BTreeMap::new()
        };
        let recipe = CommandRecipe {
            version: 1,
            image: if docker_smoke { std::env::var("AGENTSCM_COLLECTOR_SMOKE_IMAGE").expect("explicit local image digest") } else { format!("sha256:{}", "1".repeat(64)) },
            argv: vec!["/bin/sh".into(), "-c".into(), "test -r /source/src/lib.rs && test ! -e /source/collector.sqlite && printf collector-smoke".into()],
            environment_sha256: environment_digest(&environment).unwrap(),
            environment,
            toolchain_sha256: "2".repeat(64),
            limits: ExecutionLimits {
                timeout_ms: 1000,
                max_output_bytes: 4096,
                memory_bytes: 64 * 1024 * 1024,
                scratch_bytes: 1024 * 1024,
                pids: 8,
                cpu_millis: 500,
            },
            syscall_policy: SyscallPolicy::DockerBuiltin,
            no_new_privileges: true,
        };
        let engine = Engine::open(root.path()).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[
                    FileChange {
                        path: "skills/check/SKILL.md".into(),
                        content: Some(b"# Run the pinned check\n".to_vec()),
                    },
                    FileChange {
                        path: "skills/check/runner.json".into(),
                        content: Some(serde_json::to_vec(&recipe).unwrap()),
                    },
                    FileChange {
                        path: "src/lib.rs".into(),
                        content: Some(b"fn answer() -> u8 { 0 }\n".to_vec()),
                    },
                ],
            )
            .unwrap();
        let mut authority = crate::authority::Authority::open(root.path()).unwrap();
        let repository = authority.repository_id().unwrap();
        let mut context = command_context(&repository, &revision, &recipe);
        authority.start_change(&context).unwrap();
        let definition = WorkflowDefinition {
            version: 1,
            name: "collector-route".into(),
            source: WorkflowSource {
                repository: repository.clone(),
                revision: revision.clone(),
                policy_epoch: 0,
            },
            skill: SkillReference {
                entrypoint: "skills/check/SKILL.md".into(),
                resources: vec!["skills/check/runner.json".into()],
            },
            steps: vec![
                StepDefinition {
                    id: "tests".into(),
                    needs: vec![],
                    required: optional.is_none(),
                    action: StepAction::Command {
                        runner: "runner.json".into(),
                        change: context.change.clone(),
                        change_version: context.change_version,
                        context_digest: context.digest().unwrap(),
                    },
                },
                StepDefinition {
                    id: "review".into(),
                    needs: if optional.is_none() {
                        vec!["tests".into()]
                    } else {
                        vec![]
                    },
                    required: true,
                    action: StepAction::ClaudeCodeReview {
                        paths: vec!["src/lib.rs".into()],
                        model: "sonnet".into(),
                    },
                },
            ],
        };
        let plan = compile(root.path(), &definition).unwrap();
        let requirement =
            crate::authority::workflows::WorkflowRequirement::from_plan("main", &plan).unwrap();
        authority
            .install_workflow_requirement(&requirement)
            .unwrap();
        let signing = SigningKey::from_bytes(&[21; 32]);
        let public_hex = signing
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let enrollment = crate::skill_workflow::ExecutorEnrollment {
            version: 1,
            registration_id: "collector-registration".into(),
            executor_id: "collector-executor".into(),
            operator_id: "operator".into(),
            repository: repository.clone(),
            step_ids: vec!["review".into(), "tests".into()],
            registration_epoch: 1,
            verification_key_id: crate::policy::issuer_key_id(&public_hex).unwrap(),
            verification_key_sha256: format!(
                "{:x}",
                Sha256::digest(signing.verifying_key().to_bytes())
            ),
            signature_algorithm: crate::skill_workflow::ObservationSignatureAlgorithm::Ed25519,
            issued_at_ms: 1,
        };
        authority
            .enroll_workflow_executor(&enrollment, &public_hex)
            .unwrap();
        let stale_context = context.clone();
        let previous = context.change_version;
        context.change_version += 1;
        context.policy_epoch = authority.policy_epoch().unwrap();
        authority.update_change_context(previous, &context).unwrap();
        let plan = compile(root.path(), &requirement.instantiate(&context).unwrap()).unwrap();
        let profile = CollectorProfile {
            version: 1,
            profile_id: "command-profile".into(),
            authority_audience: repository.clone(),
            repository: repository.clone(),
            workflow_profile: "trusted-local".into(),
            step_id: "tests".into(),
            registration_id: enrollment.registration_id.clone(),
            registration_epoch: enrollment.registration_epoch,
            verification_key_id: enrollment.verification_key_id.clone(),
            action: CollectorProfileAction::Command {
                recipe: recipe.clone(),
                runtime_scope_sha256: runtime_scope_digest("synthetic-runtime"),
            },
        };
        authority
            .install_workflow_collector_profile(&profile)
            .unwrap();
        let review_profile = CollectorProfile {
            version: 1,
            profile_id: "review-profile".into(),
            authority_audience: repository.clone(),
            repository: repository.clone(),
            workflow_profile: "trusted-local".into(),
            step_id: "review".into(),
            registration_id: enrollment.registration_id.clone(),
            registration_epoch: enrollment.registration_epoch,
            verification_key_id: enrollment.verification_key_id.clone(),
            action: CollectorProfileAction::FiniteReview {
                executable_sha256: "4".repeat(64),
                provider_scope_sha256: "5".repeat(64),
                transport_basis: TransportBasis::ClaudeCode,
                model: "sonnet".into(),
                installed_policy_sha256: plan
                    .skill_digest()
                    .strip_prefix("sha256:")
                    .unwrap()
                    .into(),
                required_paths: vec!["src/lib.rs".into()],
                limits: ReviewLimits {
                    max_source_bytes: 64 * 1024,
                    max_request_bytes: 256 * 1024,
                    max_total_request_bytes: 1024 * 1024,
                    max_response_bytes: 128 * 1024,
                    max_calls: 4,
                    max_input_tokens: 64_000,
                    max_output_tokens: 4096,
                    max_total_tokens: 80_000,
                    max_cost_micros: Some(1000),
                    timeout_ms: 5000,
                },
            },
        };
        authority
            .install_workflow_collector_profile(&review_profile)
            .unwrap();
        for basis in [
            TransportBasis::Fixture,
            TransportBasis::LocalHttpUnqualified,
        ] {
            let mut rejected = review_profile.clone();
            rejected.profile_id = "unqualified-review".into();
            if let CollectorProfileAction::FiniteReview {
                transport_basis, ..
            } = &mut rejected.action
            {
                *transport_basis = basis;
            }
            assert!(
                authority
                    .install_workflow_collector_profile(&rejected)
                    .is_err()
            );
        }
        drop(authority);
        let collector_state = private_tempdir();
        let collector =
            TrustedLocalCollector::new(collector_state.path(), profile, signing).unwrap();
        let review_state = private_tempdir();
        let review_collector = TrustedLocalCollector::new(
            review_state.path(),
            review_profile,
            SigningKey::from_bytes(&[21; 32]),
        )
        .unwrap();
        let authorization = RouteAuthorization {
            now: AtomicU64::new(100),
            allowed: AtomicBool::new(true),
            lock: std::sync::Mutex::new(()),
            revoke_finalize: AtomicBool::new(false),
            hold_approval: AtomicBool::new(false),
            entered_approval: AtomicBool::new(false),
            restore_receipt: std::sync::Mutex::new(None),
        };
        let service = AgentTaskService::new_with_collector_authorization(
            root.path(),
            "trusted-local",
            &authorization,
            &authorization,
        )
        .unwrap();
        let actor = AuthenticatedAgentIdentity {
            owner_id: "owner".into(),
            agent_id: "agent".into(),
        };
        let origin = OriginGrantBinding {
            actor: actor.clone(),
            grant_id: "grant".into(),
            grant_generation: 1,
        };
        let make_claim = |task: &WorkflowTask, suffix: &str| CollectorClaimRequest {
            claim: TaskClaimRequest {
                version: 1,
                request_id: format!("claim-{suffix}"),
                execution_operation_id: format!("execute-{suffix}"),
                task_id: task.task_id.clone(),
                task_digest: task.digest().unwrap(),
                attempt_id: format!("attempt-{suffix}"),
                agent: actor.clone(),
                executor: enrollment.binding().unwrap(),
                requested_lease_ms: 10_000,
            },
            collector_profile_id: "command-profile".into(),
        };

        if let Some(failed) = optional {
            let tasks = service.requirements(&context, &actor).unwrap();
            if failed {
                let task = tasks.iter().find(|t| t.subject.step_id == "tests").unwrap();
                let attempt = service
                    .claim_collector(&context, &make_claim(task, "optional-failure"))
                    .unwrap();
                let request =
                    expected_command_request(&plan, task, &attempt.attempt, &collector.profile)
                        .unwrap();
                let transfer = collector
                    .sign_transfer(
                        task,
                        &attempt.attempt,
                        origin.clone(),
                        "optional-failed",
                        CollectorPayload::Command {
                            record: Box::new(result_record(&request, false)),
                            request,
                        },
                    )
                    .unwrap();
                service
                    .submit_collector_transfer(&context, &transfer)
                    .await
                    .unwrap();
            }
            let task = tasks
                .iter()
                .find(|t| t.subject.step_id == "review")
                .unwrap();
            let mut claim = make_claim(task, "optional-review");
            claim.collector_profile_id = "review-profile".into();
            let attempt = service.claim_collector(&context, &claim).unwrap();
            let transport = ReviewScript {
                scope: "5".repeat(64),
                read_source: true,
                calls: AtomicU64::new(0),
            };
            let transfer = review_collector
                .collect_review_inner(
                    root.path(),
                    &plan,
                    task,
                    &attempt.attempt,
                    origin,
                    "optional-review-upload",
                    &"4".repeat(64),
                    &transport,
                    &crate::reviewer::ReviewCancellation::default(),
                    &|| Ok(()),
                )
                .await
                .unwrap();
            service
                .submit_collector_transfer(&context, &transfer)
                .await
                .unwrap();
            let approval = Approval {
                approval_id: "optional-command-blocked".into(),
                target: "main".into(),
                revision: context.revision.clone(),
                context_digest: context.digest().unwrap(),
                change: Some(context.change.clone()),
                change_version: Some(context.change_version),
                policy_epoch: context.policy_epoch,
            };
            assert!(
                service
                    .approve_with_live_imported_origins(&actor, &context, &approval)
                    .is_err()
            );
            let request = crate::model::IntegrateRequest {
                operation_id: "optional-integrate".into(),
                target: "main".into(),
                expected_target: None,
                revision: context.revision.clone(),
                context_digest: context.digest().unwrap(),
                approval_id: approval.approval_id,
                change: Some(context.change.clone()),
                change_version: Some(context.change_version),
                policy_epoch: context.policy_epoch,
            };
            assert!(
                service
                    .integrate_with_live_imported_origins(&actor, &context, &engine, &request)
                    .is_err()
            );
            return;
        }

        assert_eq!(
            crate::authority::Authority::open_read_only(root.path())
                .unwrap()
                .review_context(&context.digest().unwrap())
                .unwrap(),
            context
        );
        let first_task = service.requirements(&context, &actor).unwrap().remove(0);
        let first = service
            .claim_collector(&context, &make_claim(&first_task, "failure"))
            .unwrap();
        let descriptor = service
            .collector_descriptor(
                &context,
                &crate::skill_workflow::TaskStatusRequest {
                    version: first.attempt.version,
                    task_id: first.attempt.task_id.clone(),
                    task_digest: first.attempt.task_digest.clone(),
                    attempt_id: first.attempt.attempt_id.clone(),
                    attempt_digest: first.attempt.digest().unwrap(),
                    fence: first.attempt.lease.fence,
                    agent: first.attempt.agent.clone(),
                    executor: first.attempt.executor.clone(),
                },
                origin.clone(),
            )
            .unwrap();
        assert_eq!(
            crate::skill_workflow::AgentTaskService::compile_collector_descriptor(
                root.path(),
                &descriptor
            )
            .unwrap(),
            plan
        );
        let mut wrong_descriptor = descriptor.clone();
        wrong_descriptor.capability = "agentscm.collector-record-transfer/v2".into();
        assert!(
            crate::skill_workflow::AgentTaskService::compile_collector_descriptor(
                root.path(),
                &wrong_descriptor
            )
            .is_err()
        );
        let mut cross_profile = make_claim(&first_task, "failure");
        cross_profile.collector_profile_id = "review-profile".into();
        assert!(service.claim_collector(&context, &cross_profile).is_err());
        assert!(
            service
                .claim(&context, &make_claim(&first_task, "failure").claim)
                .is_err()
        );
        exercise_command_collector(
            root.path(),
            &plan,
            &first_task,
            &first.attempt,
            origin.clone(),
            &collector.profile,
            docker_smoke,
        );
        let request =
            expected_command_request(&plan, &first_task, &first.attempt, &collector.profile)
                .unwrap();
        let failure = collector
            .sign_transfer(
                &first_task,
                &first.attempt,
                origin.clone(),
                "upload-failure",
                CollectorPayload::Command {
                    request: request.clone(),
                    record: Box::new(result_record(&request, false)),
                },
            )
            .unwrap();
        let mut wrong_runtime = result_record(&request, true);
        wrong_runtime.runtime_identity = Some("other-runtime".into());
        let mismatched = collector
            .sign_transfer(
                &first_task,
                &first.attempt,
                origin.clone(),
                "wrong-runtime-upload",
                CollectorPayload::Command {
                    request: request.clone(),
                    record: Box::new(wrong_runtime),
                },
            )
            .unwrap();
        assert!(
            service
                .submit_collector_transfer(&context, &mismatched)
                .await
                .is_err()
        );
        let failed = service
            .submit_collector_transfer(&context, &failure)
            .await
            .unwrap();
        assert_eq!(
            failed.state,
            crate::skill_workflow::AgentAttemptState::Finished
        );
        assert!(
            crate::authority::Authority::open(root.path())
                .unwrap()
                .approve(&Approval {
                    approval_id: "too-early".into(),
                    target: "main".into(),
                    revision: context.revision.clone(),
                    context_digest: context.digest().unwrap(),
                    change: Some(context.change.clone()),
                    change_version: Some(context.change_version),
                    policy_epoch: context.policy_epoch,
                })
                .is_err()
        );

        let next_revision = engine
            .checkpoint(
                Some(&context.revision),
                &[FileChange {
                    path: "src/lib.rs".into(),
                    content: Some(b"fn answer() -> u8 { 42 }\n".to_vec()),
                }],
            )
            .unwrap();
        let previous = context.change_version;
        context.change_version += 1;
        context.revision = next_revision;
        crate::authority::Authority::open(root.path())
            .unwrap()
            .update_change_context(previous, &context)
            .unwrap();
        let historical = service
            .submit_collector_transfer(&context, &failure)
            .await
            .unwrap();
        assert_eq!(
            historical.collector_transfer_digest,
            failed.collector_transfer_digest
        );
        assert!(!historical.currently_applicable);
        assert!(
            service
                .reconcile_collector_finalization(&actor, &stale_context)
                .is_err()
        );
        let mut changed = result_record(&request, false);
        changed.detail = "changed after context drift".into();
        let changed = collector
            .sign_transfer(
                &first_task,
                &first.attempt,
                origin.clone(),
                "upload-failure",
                CollectorPayload::Command {
                    request: request.clone(),
                    record: Box::new(changed),
                },
            )
            .unwrap();
        assert!(
            service
                .submit_collector_transfer(&context, &changed)
                .await
                .is_err()
        );
        let plan = compile(root.path(), &requirement.instantiate(&context).unwrap()).unwrap();
        let task = service.requirements(&context, &actor).unwrap().remove(0);
        let attempt = service
            .claim_collector(&context, &make_claim(&task, "success"))
            .unwrap();
        let request =
            expected_command_request(&plan, &task, &attempt.attempt, &collector.profile).unwrap();
        let success = collector
            .sign_transfer(
                &task,
                &attempt.attempt,
                origin.clone(),
                "upload-success",
                CollectorPayload::Command {
                    request: request.clone(),
                    record: Box::new(result_record(&request, true)),
                },
            )
            .unwrap();
        let accepted = service
            .submit_collector_transfer(&context, &success)
            .await
            .unwrap();
        assert_eq!(
            service
                .submit_collector_transfer(&context, &success)
                .await
                .unwrap(),
            accepted
        );
        let CollectorPayload::Command {
            request,
            mut record,
        } = success.payload.clone()
        else {
            unreachable!()
        };
        record.detail = "changed upload bytes".into();
        let conflicting_retry = collector
            .sign_transfer(
                &task,
                &attempt.attempt,
                origin.clone(),
                "upload-success",
                CollectorPayload::Command { request, record },
            )
            .unwrap();
        assert!(
            service
                .submit_collector_transfer(&context, &conflicting_retry)
                .await
                .is_err()
        );
        let review_task = service.next_task(&context, &actor).unwrap().unwrap();
        assert_eq!(review_task.subject.step_id, "review");
        let mut review_claim = make_claim(&review_task, "review-incomplete");
        review_claim.collector_profile_id = "review-profile".into();
        let review_attempt = service.claim_collector(&context, &review_claim).unwrap();
        let incomplete_transport = ReviewScript {
            scope: "5".repeat(64),
            read_source: false,
            calls: AtomicU64::new(0),
        };
        let incomplete = review_collector
            .collect_review_inner(
                root.path(),
                &plan,
                &review_task,
                &review_attempt.attempt,
                origin.clone(),
                "upload-review-incomplete",
                &"4".repeat(64),
                &incomplete_transport,
                &crate::reviewer::ReviewCancellation::default(),
                &|| Ok(()),
            )
            .await
            .unwrap();
        service
            .submit_collector_transfer(&context, &incomplete)
            .await
            .unwrap();
        assert_eq!(incomplete_transport.calls.load(Ordering::SeqCst), 1);

        let final_revision = engine
            .checkpoint(
                Some(&context.revision),
                &[FileChange {
                    path: "src/lib.rs".into(),
                    content: Some(b"fn answer() -> u8 { 43 }\n".to_vec()),
                }],
            )
            .unwrap();
        let previous = context.change_version;
        context.change_version += 1;
        context.revision = final_revision;
        crate::authority::Authority::open(root.path())
            .unwrap()
            .update_change_context(previous, &context)
            .unwrap();
        let plan = compile(root.path(), &requirement.instantiate(&context).unwrap()).unwrap();
        let tasks = service.requirements(&context, &actor).unwrap();
        let task = tasks
            .iter()
            .find(|task| task.subject.step_id == "tests")
            .unwrap()
            .clone();
        let attempt = service
            .claim_collector(&context, &make_claim(&task, "final-tests"))
            .unwrap();
        let request =
            expected_command_request(&plan, &task, &attempt.attempt, &collector.profile).unwrap();
        let success = collector
            .sign_transfer(
                &task,
                &attempt.attempt,
                origin.clone(),
                "upload-final-tests",
                CollectorPayload::Command {
                    request: request.clone(),
                    record: Box::new(result_record(&request, true)),
                },
            )
            .unwrap();
        service
            .submit_collector_transfer(&context, &success)
            .await
            .unwrap();
        let review_task = service.next_task(&context, &actor).unwrap().unwrap();
        let mut review_claim = make_claim(&review_task, "final-review");
        review_claim.collector_profile_id = "review-profile".into();
        let review_attempt = service.claim_collector(&context, &review_claim).unwrap();
        let successful_transport = ReviewScript {
            scope: "5".repeat(64),
            read_source: true,
            calls: AtomicU64::new(0),
        };
        let concrete_root = private_tempdir();
        let concrete_work = concrete_root.path().join("work");
        crate::local_recovery::create_private_directory(&concrete_work).unwrap();
        let executable = concrete_root.path().join("claude-fixture");
        std::fs::write(&executable, b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.300 (Claude Code)'; elif [ \"$1\" = auth ]; then echo '{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}'; else exit 99; fi\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let concrete =
            crate::reviewer::ClaudeCodeTransport::new(&executable, &concrete_work).unwrap();
        assert!(
            review_collector
                .collect_review(
                    root.path(),
                    &plan,
                    &review_task,
                    &review_attempt.attempt,
                    origin.clone(),
                    "concrete-mismatch",
                    &concrete,
                    &crate::reviewer::ReviewCancellation::default(),
                    &|| Ok(())
                )
                .await
                .is_err()
        );
        let review = review_collector
            .collect_review_inner(
                root.path(),
                &plan,
                &review_task,
                &review_attempt.attempt,
                origin.clone(),
                "upload-final-review",
                &"4".repeat(64),
                &successful_transport,
                &crate::reviewer::ReviewCancellation::default(),
                &|| Ok(()),
            )
            .await
            .unwrap();
        let CollectorPayload::Review { request, record } = &review.payload else {
            unreachable!()
        };
        let mut wrong = record.clone();
        wrong.calls[0].reservation.transport_basis = TransportBasis::Fixture;
        let wrong = review_collector
            .sign_transfer(
                &review_task,
                &review_attempt.attempt,
                origin.clone(),
                "wrong-basis",
                CollectorPayload::Review {
                    request: request.clone(),
                    record: wrong,
                },
            )
            .unwrap();
        assert!(
            service
                .submit_collector_transfer(&context, &wrong)
                .await
                .is_err()
        );
        let mut wrong_observation = record.clone();
        wrong_observation.calls[0].observation.basis = TransportBasis::Fixture;
        let wrong = review_collector
            .sign_transfer(
                &review_task,
                &review_attempt.attempt,
                origin.clone(),
                "wrong-observation",
                CollectorPayload::Review {
                    request: request.clone(),
                    record: wrong_observation,
                },
            )
            .unwrap();
        assert!(
            service
                .submit_collector_transfer(&context, &wrong)
                .await
                .is_err()
        );
        authorization.revoke_finalize.store(true, Ordering::SeqCst);
        assert!(
            service
                .submit_collector_transfer(&context, &review)
                .await
                .is_err()
        );
        assert!(
            crate::authority::Authority::open_read_only(root.path())
                .unwrap()
                .imported_workflow_origins(&context)
                .unwrap()
                .is_empty()
        );
        let reopened = AgentTaskService::new_with_collector_authorization(
            root.path(),
            "trusted-local",
            &authorization,
            &authorization,
        )
        .unwrap();
        reopened
            .submit_collector_transfer(&context, &review)
            .await
            .unwrap();
        assert!(
            crate::authority::Authority::open_read_only(root.path())
                .unwrap()
                .imported_workflow_origins(&context)
                .unwrap()
                .is_empty()
        );
        authorization.revoke_finalize.store(false, Ordering::SeqCst);
        assert!(
            reopened
                .reconcile_collector_finalization(&actor, &context)
                .unwrap()
                .is_some()
        );
        assert_eq!(successful_transport.calls.load(Ordering::SeqCst), 2);
        let approval = Approval {
            approval_id: "approved".into(),
            target: "main".into(),
            revision: context.revision.clone(),
            context_digest: context.digest().unwrap(),
            change: Some(context.change.clone()),
            change_version: Some(context.change_version),
            policy_epoch: context.policy_epoch,
        };
        assert!(
            crate::authority::Authority::open(root.path())
                .unwrap()
                .approve(&approval)
                .is_err()
        );
        // A receipt arrives after the service snapshots origins but before its native transaction.
        // The empty guarded manifest must not authorize the newly visible dependencies.
        let native_database = root.path().join("state.sqlite");
        Connection::open(&native_database).unwrap().execute_batch("CREATE TABLE fixture_hidden_receipts AS SELECT * FROM workflow_receipts; DELETE FROM workflow_receipts;").unwrap();
        *authorization.restore_receipt.lock().unwrap() = Some(native_database);
        assert!(
            service
                .approve_with_live_imported_origins(&actor, &context, &approval)
                .is_err()
        );
        // Admission holds the same non-reentrant store lock used by revocation.
        authorization.hold_approval.store(true, Ordering::SeqCst);
        std::thread::scope(|scope| {
            let admission = scope
                .spawn(|| service.approve_with_live_imported_origins(&actor, &context, &approval));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !authorization.entered_approval.load(Ordering::SeqCst) {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            let (sender, receiver) = std::sync::mpsc::channel();
            let authorization = &authorization;
            let revocation = scope.spawn(move || {
                let _lock = authorization.lock.lock().unwrap();
                authorization.allowed.store(false, Ordering::SeqCst);
                sender.send(()).unwrap();
            });
            assert!(
                receiver
                    .recv_timeout(std::time::Duration::from_millis(20))
                    .is_err()
            );
            authorization.hold_approval.store(false, Ordering::SeqCst);
            admission.join().unwrap().unwrap();
            revocation.join().unwrap();
        });
        let second = Approval {
            approval_id: "revoked-origin".into(),
            ..approval
        };
        assert!(
            service
                .approve_with_live_imported_origins(&actor, &context, &second)
                .is_err()
        );
        authorization.allowed.store(true, Ordering::SeqCst);
        let mut authority = crate::authority::Authority::open(root.path()).unwrap();
        let revoked_at_policy_epoch = authority.policy_epoch().unwrap() + 1;
        authority
            .revoke_workflow_executor(&crate::skill_workflow::ExecutorRevocation {
                version: 1,
                registration_id: enrollment.registration_id,
                repository,
                registration_epoch: enrollment.registration_epoch,
                revoked_at_policy_epoch,
                reason: "synthetic post-receipt revocation".into(),
            })
            .unwrap();
        drop(authority);
        let after_executor_revocation = Approval {
            approval_id: "revoked-executor".into(),
            ..second
        };
        assert!(
            service
                .approve_with_live_imported_origins(&actor, &context, &after_executor_revocation,)
                .is_err()
        );
    }
    #[allow(clippy::too_many_arguments)]
    fn exercise_command_collector(
        data_dir: &Path,
        plan: &CompiledPlan,
        task: &WorkflowTask,
        attempt: &WorkflowTaskAttempt,
        origin: OriginGrantBinding,
        profile: &CollectorProfile,
        real: bool,
    ) {
        use std::os::unix::fs::PermissionsExt;
        let root = private_tempdir();
        let config = root.path().join("config");
        crate::local_recovery::create_private_directory(&config).unwrap();
        let socket = root.path().join("docker.sock");
        let executable = root.path().join("docker.py");
        let _listener;
        let backend = if real {
            _listener = None;
            crate::execution::OciBackend::new(
                Path::new(&std::env::var("AGENTSCM_COLLECTOR_SMOKE_DOCKER").unwrap()),
                Path::new(&std::env::var("AGENTSCM_COLLECTOR_SMOKE_SOCKET").unwrap()),
                &config,
            )
            .unwrap()
        } else {
            _listener = Some(std::os::unix::net::UnixListener::bind(&socket).unwrap());
            std::fs::write(&executable, fake_docker::FAKE_DOCKER).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            crate::execution::OciBackend::new(&executable, &socket, &config).unwrap()
        };
        let probe = backend.probe();
        let identity = probe.identity.expect("available test runtime");
        let mut profile = profile.clone();
        if let CollectorProfileAction::Command {
            runtime_scope_sha256,
            ..
        } = &mut profile.action
        {
            *runtime_scope_sha256 = runtime_scope_digest(&identity);
        }
        let state = root.path().join("collector");
        let collector =
            TrustedLocalCollector::new(&state, profile, SigningKey::from_bytes(&[21; 32])).unwrap();
        let transfer = collector
            .collect_command(
                data_dir,
                plan,
                task,
                attempt,
                origin.clone(),
                "production-control-flow",
                &backend,
                &crate::execution::Cancellation::default(),
                &|| Ok(()),
            )
            .unwrap();
        assert_eq!(
            collector
                .retained_transfer("production-control-flow")
                .unwrap(),
            transfer
        );
        assert!(collector.retained_transfer("missing-transfer").is_err());
        let CollectorPayload::Command { record, .. } = &transfer.payload else {
            unreachable!()
        };
        assert_eq!(record.runtime_identity.as_ref(), Some(&identity));
        assert_eq!(
            std::fs::read_dir(state.join("snapshots")).unwrap().count(),
            0
        );
        if !real {
            assert!(command_record_succeeded(record));
        }
        if real && command_record_succeeded(record) {
            assert_eq!(record.stdout, b"collector-smoke");
        }
        assert!(
            collector
                .collect_command(
                    data_dir,
                    plan,
                    task,
                    attempt,
                    origin.clone(),
                    "no-reexecution",
                    &backend,
                    &crate::execution::Cancellation::default(),
                    &|| Ok(())
                )
                .is_err()
        );
        let mut mismatched = collector.profile.clone();
        if let CollectorProfileAction::Command {
            runtime_scope_sha256,
            ..
        } = &mut mismatched.action
        {
            *runtime_scope_sha256 = "0".repeat(64);
        }
        let other = TrustedLocalCollector::new(
            &root.path().join("other"),
            mismatched,
            SigningKey::from_bytes(&[21; 32]),
        )
        .unwrap();
        assert!(
            other
                .collect_command(
                    data_dir,
                    plan,
                    task,
                    attempt,
                    origin,
                    "wrong-runtime",
                    &backend,
                    &crate::execution::Cancellation::default(),
                    &|| Ok(())
                )
                .is_err()
        );
    }

    #[test]
    fn retained_transfer_rejects_oversized_blob_before_loading_it() {
        let (root, collector, ..) = fixture();
        let journal = collector.open_journal().unwrap();
        journal
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        journal
            .execute(
                "INSERT INTO collector_transfers(transfer_request_id,digest,canonical_json) VALUES('corrupt-oversized',?1,zeroblob(?2))",
                params!["f".repeat(64), MAX_COLLECTOR_TRANSFER_BYTES + 1],
            )
            .unwrap();
        drop(journal);
        assert!(matches!(
            collector.retained_transfer("corrupt-oversized"),
            Err(Error::Limit(_))
        ));
        drop(root);
    }

    mod fake_docker {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/common/fake_docker.rs"
        ));
    }

    #[test]
    fn collector_paths_confine_hostile_attempt_ids_and_reject_unsafe_state() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (root, collector, _task, mut attempt, _, _, _) = fixture();
        let outside = private_tempdir();
        std::fs::write(outside.path().join("keep"), b"untouched").unwrap();
        for id in ["../escape", "../../outside", "a/b", "..", "/absolute"] {
            attempt.attempt_id = id.into();
            let path = collector.snapshot_path(&attempt).unwrap();
            assert_eq!(
                path.parent(),
                Some(collector.state_dir.join("snapshots").as_path())
            );
            assert_eq!(path.file_name().unwrap().len(), 64);
        }
        let db = root.path().join("collector.sqlite");
        std::fs::remove_file(&db).unwrap();
        symlink(outside.path().join("keep"), &db).unwrap();
        assert!(collector.open_journal().is_err());
        std::fs::remove_file(&db).unwrap();
        for sidecar in [
            "collector.sqlite-wal",
            "collector.sqlite-shm",
            "collector.sqlite-journal",
        ] {
            let path = root.path().join(sidecar);
            symlink(outside.path().join("keep"), &path).unwrap();
            assert!(collector.open_journal().is_err());
            std::fs::remove_file(path).unwrap();
        }
        let snapshots = root.path().join("snapshots");
        std::fs::remove_dir(&snapshots).unwrap();
        symlink(outside.path(), &snapshots).unwrap();
        assert!(collector.snapshot_path(&attempt).is_err());
        std::fs::remove_file(&snapshots).unwrap();
        crate::local_recovery::create_private_directory(&snapshots).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(collector.open_journal().is_err());
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = outside.path().join("linked-state");
        symlink(root.path(), &link).unwrap();
        assert!(
            TrustedLocalCollector::new(
                &link,
                collector.profile.clone(),
                SigningKey::from_bytes(&[11; 32])
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(outside.path().join("keep")).unwrap(),
            b"untouched"
        );
    }
}
