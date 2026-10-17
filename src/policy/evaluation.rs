use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

use super::issuers::{IssuerRegistrySnapshot, KeyTrustStatus};
use super::requirements::*;
use crate::checks::{CheckState, CheckSubject};
use crate::context::{ContextReadiness, DependencyState, ReviewContext, SUPPORTED_CAPABILITIES};
use crate::evidence::{
    ArtifactStatus, CurrentIssuerTrust, EvaluationClock, EvidenceOutcome, ExecutionBasis,
    ExecutionClaimStatus, Freshness, IntegrityStatus, RecoveryState, RegistryBindingStatus,
    TerminalState, VerificationReport,
};
use crate::{Error, Result};

/// Current rows read by the authenticated authority in its admission transaction.
/// A serializable snapshot is not proof that it is current or complete.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CurrentAttemptSnapshot {
    pub version: u32,
    pub repository: String,
    pub context_sha256: String,
    pub inventory_complete: bool,
    pub attempts: Vec<CurrentAttemptBinding>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CurrentAttemptBinding {
    pub attempt_id: String,
    pub requirement_id: String,
    pub check_kind: String,
    pub subject: CheckSubject,
    pub durable_version: u64,
    pub run_generation: u64,
    pub request_sha256: String,
    pub limits_sha256: String,
    pub state: CheckState,
    pub evidence_sha256: Option<String>,
    pub terminal_record_sha256: Option<String>,
}

/// Resolved bindings, including their generation, supplied by current authority.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencySnapshot {
    pub version: u32,
    pub repository: String,
    pub generation: u64,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub inventory_complete: bool,
    pub bindings: Vec<CurrentDependencyBinding>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CurrentDependencyBinding {
    pub identity: String,
    pub revision_sha256: String,
}

/// An installed qualification reference, not a qualification performed here.
/// C must authenticate this catalogue and retain/validate the actual proof.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QualificationSnapshot {
    pub version: u32,
    pub repository: String,
    pub epoch: u64,
    pub qualifications: Vec<InstalledQualification>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledQualification {
    pub check_kind: String,
    pub profile: String,
    pub proof_sha256: String,
    pub authorized_issuer_ids: Vec<String>,
    pub not_before_ms: u64,
    pub not_after_ms: u64,
    pub revoked_at_ms: Option<u64>,
}

/// All inputs are conditional facts. C must acquire them under its current
/// authorization/publication/retention fences and never accept a client verdict.
pub struct PolicyInputs<'a> {
    pub context: &'a ReviewContext,
    pub registry: &'a IssuerRegistrySnapshot,
    pub current_attempts: &'a CurrentAttemptSnapshot,
    pub dependencies: &'a DependencySnapshot,
    pub qualifications: &'a QualificationSnapshot,
    pub clock: EvaluationClock,
    pub evidence: &'a [VerificationReport],
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVerdict {
    Satisfied,
    NotSatisfied,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PolicyIssue {
    ClockUncertain,
    ContextIncomplete,
    CandidateRequirementUnsupported,
    CandidateRequirementMismatch,
    AttemptInventoryIncomplete,
    CurrentFailure,
    MissingEvidence,
    InvalidSignature,
    StaleVerification,
    StaleRegistry,
    UntrustedIssuer,
    UnpermittedIssuer,
    EvidenceExpired,
    EvidenceFutureIssued,
    EvidenceTooOld,
    EvidenceRetentionTooShort,
    RetentionUnspecified,
    SubjectMismatch,
    PolicyMismatch,
    StaleAttempt,
    OutcomeNotSuccessful,
    RecoveryUnverified,
    ExecutionUnqualified,
    QualificationUnavailable,
    QualificationMismatch,
    QualificationIssuerUnpermitted,
    QualificationNotYetValid,
    QualificationExpired,
    QualificationRevoked,
    CoverageIncomplete,
    ArtifactUnavailable,
    ArtifactExpired,
    ArtifactMismatch,
    IndependenceInsufficient,
    IndependenceUnknown,
    DependencyInventoryIncomplete,
    DependencySnapshotStale,
    DependencyMissing,
    DependencyMismatch,
    DependencyBasisInsufficient,
    DependencyObservationUnavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequirementAssessment {
    pub requirement_id: String,
    pub verdict: PolicyVerdict,
    pub independent_issuers: u32,
    pub selected_evidence_sha256: Vec<String>,
    pub issues: Vec<PolicyIssue>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyEvaluation {
    pub version: u32,
    /// Always conditional on supplied records, not authenticated authority.
    pub basis: EvaluationBasis,
    pub policy_sha256: String,
    pub registry_sha256: String,
    pub context_sha256: String,
    pub current_attempts_sha256: String,
    pub dependency_snapshot_sha256: String,
    pub qualification_snapshot_sha256: String,
    pub verification_facts_sha256: String,
    pub evaluated_at_ms: Option<u64>,
    pub verdict: PolicyVerdict,
    pub checks: Vec<RequirementAssessment>,
    pub issues: Vec<PolicyIssue>,
    /// Unknown or incomplete required facts escalate; candidates cannot waive them.
    pub escalation_required: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationBasis {
    SuppliedRecordsOnly,
}

impl CurrentAttemptSnapshot {
    /// Bounded decoding does not authenticate or establish currency of a snapshot.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(Error::Limit(
                "policy snapshot input exceeds its byte limit".into(),
            ));
        }
        let snapshot: Self = serde_json::from_slice(bytes)?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<()> {
        version_repository(self.version, &self.repository)?;
        digest(&self.context_sha256)?;
        if self.attempts.len() > 256 {
            return Err(Error::Limit("current attempt count exceeds 256".into()));
        }
        let mut ids = HashSet::new();
        for row in &self.attempts {
            identifier(&row.attempt_id)?;
            identifier(&row.requirement_id)?;
            identifier(&row.check_kind)?;
            validate_subject(&row.subject)?;
            integer(row.durable_version)?;
            integer(row.run_generation)?;
            digest(&row.request_sha256)?;
            digest(&row.limits_sha256)?;
            if let Some(value) = &row.evidence_sha256 {
                digest(value)?;
            }
            if let Some(value) = &row.terminal_record_sha256 {
                digest(value)?;
            }
            if row.subject.repository != self.repository || !ids.insert(&row.attempt_id) {
                return Err(Error::Invalid(
                    "current attempt has a duplicate or cross-repository identity".into(),
                ));
            }
            if row.subject.context_digest != self.context_sha256 {
                return Err(Error::Invalid(
                    "attempt inventory crosses review contexts".into(),
                ));
            }
            if row.state == CheckState::Pending
                && (row.run_generation != 0 || row.durable_version != 0)
            {
                return Err(Error::Invalid(
                    "pending attempt has an advanced run fence".into(),
                ));
            }
            if matches!(row.state, CheckState::Pending | CheckState::Running)
                && (row.evidence_sha256.is_some() || row.terminal_record_sha256.is_some())
            {
                return Err(Error::Invalid(
                    "non-terminal attempt has terminal evidence".into(),
                ));
            }
            if row.state != CheckState::Pending
                && (row.run_generation == 0 || row.durable_version == 0)
            {
                return Err(Error::Invalid(
                    "non-pending attempt has no durable run fence".into(),
                ));
            }
            if row.state == CheckState::Success
                && (row.evidence_sha256.is_none() || row.terminal_record_sha256.is_none())
            {
                return Err(Error::Invalid(
                    "successful current attempt has no evidence commitment".into(),
                ));
            }
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        let mut value = self.clone();
        value
            .attempts
            .sort_by(|a, b| a.attempt_id.cmp(&b.attempt_id));
        domain_hash(
            b"agentscm.current-attempt-snapshot.v1\0",
            &bounded_json(&value, MAX_POLICY_BYTES)?,
        )
    }
}

impl DependencySnapshot {
    /// Bounded decoding does not authenticate or establish currency of a snapshot.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(Error::Limit(
                "policy snapshot input exceeds its byte limit".into(),
            ));
        }
        let snapshot: Self = serde_json::from_slice(bytes)?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<()> {
        version_repository(self.version, &self.repository)?;
        integer(self.generation)?;
        time_range(self.observed_at_ms, self.expires_at_ms)?;
        if self.bindings.len() > MAX_POLICY_DEPENDENCIES {
            return Err(Error::Limit(
                "dependency snapshot exceeds 128 bindings".into(),
            ));
        }
        let mut ids = HashSet::new();
        for binding in &self.bindings {
            identifier(&binding.identity)?;
            digest(&binding.revision_sha256)?;
            if !ids.insert(&binding.identity) {
                return Err(Error::Invalid("duplicate current dependency".into()));
            }
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        let mut value = self.clone();
        value.bindings.sort_by(|a, b| a.identity.cmp(&b.identity));
        domain_hash(
            b"agentscm.dependency-snapshot.v1\0",
            &bounded_json(&value, MAX_POLICY_BYTES)?,
        )
    }
}

impl QualificationSnapshot {
    /// Bounded decoding does not authenticate or establish currency of a snapshot.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(Error::Limit(
                "policy snapshot input exceeds its byte limit".into(),
            ));
        }
        let snapshot: Self = serde_json::from_slice(bytes)?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<()> {
        version_repository(self.version, &self.repository)?;
        integer(self.epoch)?;
        if self.qualifications.len() > 128 {
            return Err(Error::Limit(
                "qualification catalogue exceeds 128 entries".into(),
            ));
        }
        let mut ids = HashSet::new();
        for qualification in &self.qualifications {
            identifier(&qualification.check_kind)?;
            identifier(&qualification.profile)?;
            digest(&qualification.proof_sha256)?;
            unique_identifiers(&qualification.authorized_issuer_ids, 1, MAX_POLICY_ISSUERS)?;
            time_range(qualification.not_before_ms, qualification.not_after_ms)?;
            if let Some(at) = qualification.revoked_at_ms {
                integer(at)?;
            }
            if !ids.insert((
                &qualification.check_kind,
                &qualification.profile,
                &qualification.proof_sha256,
            )) {
                return Err(Error::Invalid("duplicate installed qualification".into()));
            }
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        let mut value = self.clone();
        value.qualifications.sort_by(|a, b| {
            (&a.check_kind, &a.profile, &a.proof_sha256).cmp(&(
                &b.check_kind,
                &b.profile,
                &b.proof_sha256,
            ))
        });
        for qualification in &mut value.qualifications {
            qualification.authorized_issuer_ids.sort();
        }
        domain_hash(
            b"agentscm.qualification-snapshot.v1\0",
            &bounded_json(&value, MAX_POLICY_BYTES)?,
        )
    }
}

impl PolicyEvaluation {
    pub fn digest(&self) -> Result<String> {
        domain_hash(
            b"agentscm.policy-evaluation.v1\0",
            &bounded_json(self, MAX_POLICY_BYTES)?,
        )
    }
}

fn version_repository(version: u32, repository: &str) -> Result<()> {
    if version != 1 {
        return Err(Error::Invalid("unsupported policy snapshot version".into()));
    }
    identifier(repository)
}
fn time_range(begin: u64, end: u64) -> Result<()> {
    integer(begin)?;
    integer(end)?;
    if begin >= end {
        return Err(Error::Invalid("policy snapshot time range is empty".into()));
    }
    Ok(())
}
fn validate_subject(subject: &CheckSubject) -> Result<()> {
    subject.change.validate()?;
    if subject.repository != subject.change.repository || subject.change_version == 0 {
        return Err(Error::Invalid(
            "policy subject identity/generation mismatch".into(),
        ));
    }
    integer(subject.change_version)?;
    for value in [
        &subject.context_digest,
        &subject.revision,
        &subject.recipe_sha256,
        &subject.toolchain_sha256,
        &subject.environment_sha256,
    ] {
        digest(value)?;
    }
    Ok(())
}

/// Evaluates supplied, current facts. This function performs no authorization,
/// installation, source read, dispatch, reservation, retention or target update.
pub fn evaluate_policy(
    policy: &InstalledPolicy,
    input: PolicyInputs<'_>,
) -> Result<PolicyEvaluation> {
    let requirements = policy.derive_requirements(input.context)?;
    input.registry.validate()?;
    input.current_attempts.validate()?;
    input.dependencies.validate()?;
    input.qualifications.validate()?;
    if input.evidence.len() > 256 {
        return Err(Error::Limit("policy evidence count exceeds 256".into()));
    }
    for repository in [
        &input.registry.repository,
        &input.current_attempts.repository,
        &input.dependencies.repository,
        &input.qualifications.repository,
    ] {
        if repository != &policy.repository {
            return Err(Error::Conflict(
                "policy inputs cross repository boundaries".into(),
            ));
        }
    }
    let policy_sha256 = policy.digest()?;
    let registry_sha256 = input.registry.digest()?;
    let context_sha256 = input.context.digest()?;
    if context_sha256 != input.current_attempts.context_sha256 {
        return Err(Error::Conflict(
            "current attempt inventory is for another context".into(),
        ));
    }
    let dependency_sha256 = input.dependencies.digest()?;
    let now = match input.clock {
        EvaluationClock::Known { unix_ms } => {
            integer(unix_ms)?;
            Some(unix_ms)
        }
        EvaluationClock::Uncertain => None,
    };
    let mut issues = BTreeSet::new();
    if now.is_none() {
        issues.insert(PolicyIssue::ClockUncertain);
    }
    if !matches!(
        input.context.readiness(SUPPORTED_CAPABILITIES),
        ContextReadiness::Complete
    ) {
        issues.insert(PolicyIssue::ContextIncomplete);
    }
    if !input.current_attempts.inventory_complete {
        issues.insert(PolicyIssue::AttemptInventoryIncomplete);
    }
    for declared in &input.context.required_evidence {
        match policy
            .required_checks
            .iter()
            .find(|r| r.requirement_id == declared.identity)
        {
            None => {
                issues.insert(PolicyIssue::CandidateRequirementUnsupported);
            }
            Some(_) if known_identity(&declared.digest).is_none() => {
                issues.insert(PolicyIssue::ContextIncomplete);
            }
            Some(requirement)
                if known_identity(&declared.digest) != Some(requirement.recipe_sha256.as_str()) =>
            {
                issues.insert(PolicyIssue::CandidateRequirementMismatch);
            }
            _ => {}
        }
    }
    let mut digests = HashSet::new();
    for report in input.evidence {
        if !digests.insert(report.envelope_sha256()) {
            return Err(Error::Invalid("duplicate policy evidence envelope".into()));
        }
        if !input
            .current_attempts
            .attempts
            .iter()
            .any(|a| a.evidence_sha256.as_deref() == Some(report.envelope_sha256()))
        {
            issues.insert(PolicyIssue::StaleAttempt);
        }
    }
    let mut checks = Vec::new();
    let mut accepted = Vec::new();
    for bound in &requirements {
        let mut failures = BTreeSet::new();
        let current: Vec<_> = input
            .current_attempts
            .attempts
            .iter()
            .filter(|a| a.requirement_id == bound.requirement.requirement_id)
            .collect();
        for attempt in &current {
            if attempt.subject == bound.subject && attempt.state == CheckState::Fail {
                failures.insert(PolicyIssue::CurrentFailure);
            }
            if attempt.subject == bound.subject
                && attempt.state == CheckState::Success
                && !input
                    .evidence
                    .iter()
                    .any(|r| Some(r.envelope_sha256()) == attempt.evidence_sha256.as_deref())
            {
                failures.insert(PolicyIssue::MissingEvidence);
            }
        }
        let mut selected = Vec::new();
        let mut issuers = Vec::new();
        for report in input.evidence {
            let relates = current
                .iter()
                .any(|a| a.evidence_sha256.as_deref() == Some(report.envelope_sha256()));
            if !relates {
                continue;
            }
            let rejected = assess_receipt(
                policy,
                bound,
                &input,
                report,
                &policy_sha256,
                &registry_sha256,
                &dependency_sha256,
                now,
            )?;
            if rejected.is_empty() {
                let receipt = report
                    .verified_receipt()
                    .expect("accepted receipt passed strict integrity");
                selected.push(receipt.envelope_sha256().to_owned());
                issuers.push(receipt.payload().issuer_id.clone());
                accepted.push(report);
            } else {
                failures.extend(rejected);
            }
        }
        if selected.is_empty() {
            failures.insert(PolicyIssue::MissingEvidence);
        }
        // Receipts have a separate 256-entry cap; issuer selection is bounded
        // by distinct registry identities, not repeated attempts by one issuer.
        issuers.sort();
        issuers.dedup();
        let independence = input
            .registry
            .assess_independence(&issuers, &bound.requirement.check_kind)?;
        if !independence.unknown_control_issuer_ids.is_empty()
            || !independence.unknown_issuer_ids.is_empty()
        {
            failures.insert(PolicyIssue::IndependenceUnknown);
        }
        if independence.independent_count < bound.requirement.minimum_independent_issuers as usize
            && !selected.is_empty()
            && !failures.contains(&PolicyIssue::IndependenceUnknown)
        {
            failures.insert(PolicyIssue::IndependenceInsufficient);
        }
        selected.sort();
        selected.dedup();
        checks.push(RequirementAssessment {
            requirement_id: bound.requirement.requirement_id.clone(),
            verdict: verdict(&failures),
            independent_issuers: independence.independent_count as u32,
            selected_evidence_sha256: selected,
            issues: failures.into_iter().collect(),
        });
    }
    assess_dependencies(
        policy,
        &input,
        &accepted,
        &dependency_sha256,
        now,
        &mut issues,
    );
    let mut all = issues.clone();
    for check in &checks {
        all.extend(check.issues.iter().copied());
    }
    let result = PolicyEvaluation {
        version: 1,
        basis: EvaluationBasis::SuppliedRecordsOnly,
        policy_sha256,
        registry_sha256,
        context_sha256,
        current_attempts_sha256: input.current_attempts.digest()?,
        dependency_snapshot_sha256: dependency_sha256,
        qualification_snapshot_sha256: input.qualifications.digest()?,
        verification_facts_sha256: verification_facts_digest(input.evidence)?,
        evaluated_at_ms: now,
        verdict: verdict(&all),
        checks,
        escalation_required: !all.is_empty(),
        issues: issues.into_iter().collect(),
    };
    // Also enforce the finite returned representation, not only input lists.
    bounded_json(&result, MAX_POLICY_BYTES)?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn assess_receipt(
    policy: &InstalledPolicy,
    bound: &BoundRequirement,
    input: &PolicyInputs<'_>,
    report: &VerificationReport,
    policy_digest: &str,
    registry_digest: &str,
    dependency_digest: &str,
    now: Option<u64>,
) -> Result<BTreeSet<PolicyIssue>> {
    let mut issues = BTreeSet::new();
    if report.evaluated_clock() != input.clock
        || report.evaluated_registry_sha256() != registry_digest
    {
        issues.insert(PolicyIssue::StaleVerification);
    }
    if report.registry_binding() != RegistryBindingStatus::Current {
        issues.insert(PolicyIssue::StaleRegistry);
    }
    if report.integrity() != IntegrityStatus::Verified {
        issues.insert(PolicyIssue::InvalidSignature);
    }
    let Some(receipt) = report.verified_receipt() else {
        return Ok(issues);
    };
    let payload = receipt.payload();
    if payload.subject != bound.subject
        || payload.requirement_id != bound.requirement.requirement_id
        || payload.check_kind != bound.requirement.check_kind
    {
        issues.insert(PolicyIssue::SubjectMismatch);
    }
    if payload.installed_policy_sha256 != policy_digest
        || payload.installed_policy_epoch != policy.epoch
    {
        issues.insert(PolicyIssue::PolicyMismatch);
    }
    if !bound
        .requirement
        .trusted_issuer_ids
        .contains(&payload.issuer_id)
    {
        issues.insert(PolicyIssue::UnpermittedIssuer);
    }
    if input
        .registry
        .issuers
        .iter()
        .find(|i| i.issuer_id == payload.issuer_id)
        .is_none_or(|i| !i.allowed_check_kinds.contains(&payload.check_kind))
    {
        issues.insert(PolicyIssue::UnpermittedIssuer);
    }
    let current = input
        .current_attempts
        .attempts
        .iter()
        .find(|a| a.attempt_id == payload.attempt_id);
    if current.is_none_or(|a| {
        a.subject != payload.subject
            || a.requirement_id != payload.requirement_id
            || a.check_kind != payload.check_kind
            || a.run_generation != payload.run_generation
            || a.request_sha256 != payload.request_sha256
            || a.limits_sha256 != payload.limits_sha256
            || a.evidence_sha256.as_deref() != Some(receipt.envelope_sha256())
            || a.terminal_record_sha256.as_deref() != Some(payload.terminal_record_sha256.as_str())
            || a.state != CheckState::Success
    }) {
        issues.insert(PolicyIssue::StaleAttempt);
    }
    if payload.terminal_state != TerminalState::Success || payload.outcome != EvidenceOutcome::Pass
    {
        issues.insert(PolicyIssue::OutcomeNotSuccessful);
    }
    if payload.recovery_state != RecoveryState::NotRequired {
        issues.insert(PolicyIssue::RecoveryUnverified);
    }
    if payload.execution_basis != ExecutionBasis::ControllerObserved {
        issues.insert(PolicyIssue::ExecutionUnqualified);
    }
    if let Some(now) = now {
        let trust = input.registry.key_trust(
            &payload.issuer_id,
            &payload.key_id,
            payload.issued_at_ms,
            now,
        )?;
        if trust.status != KeyTrustStatus::Trusted {
            issues.insert(PolicyIssue::UntrustedIssuer);
        }
        if payload.issued_at_ms > now {
            issues.insert(PolicyIssue::EvidenceFutureIssued);
        }
        if payload.expires_at_ms <= now || report.freshness() == Freshness::Expired {
            issues.insert(PolicyIssue::EvidenceExpired);
        }
        if now.saturating_sub(payload.issued_at_ms) > bound.requirement.maximum_age_ms {
            issues.insert(PolicyIssue::EvidenceTooOld);
        }
        if now
            .checked_add(policy.minimum_retention_ms)
            .is_none_or(|until| payload.expires_at_ms < until)
        {
            issues.insert(PolicyIssue::EvidenceRetentionTooShort);
        }
    } else {
        issues.insert(PolicyIssue::ClockUncertain);
    }
    let coverage = &payload.coverage;
    if bound
        .requirement
        .required_scope
        .iter()
        .any(|s| !coverage.scope.contains(s))
        || coverage.observed_count < bound.requirement.required_scope.len() as u32
        || payload.output.truncated
        || (bound.requirement.require_complete_coverage
            && (!coverage.inventory_complete
                || coverage.expected_count != coverage.observed_count
                || !coverage.omissions.is_empty()))
    {
        issues.insert(PolicyIssue::CoverageIncomplete);
    }
    for required in &bound.requirement.required_artifacts {
        let claim = payload
            .artifacts
            .iter()
            .find(|a| &a.artifact_id == required);
        let actual = report
            .artifacts()
            .iter()
            .find(|a| &a.artifact_id == required);
        match (claim, actual) {
            (Some(claim), Some(actual))
                if actual.status == ArtifactStatus::Full && actual.commitment_matches =>
            {
                if let Some(issue) =
                    artifact_retention_issue(claim.expires_at_ms, now, policy.minimum_retention_ms)
                {
                    issues.insert(issue);
                }
            }
            (_, Some(actual)) if actual.status == ArtifactStatus::HashMismatch => {
                issues.insert(PolicyIssue::ArtifactMismatch);
            }
            (_, Some(actual)) if actual.status == ArtifactStatus::Expired => {
                issues.insert(PolicyIssue::ArtifactExpired);
            }
            _ => {
                issues.insert(PolicyIssue::ArtifactUnavailable);
            }
        }
    }
    match &bound.requirement.execution {
        RequiredExecution::SourceIntegrity => {}
        RequiredExecution::QualifiedCommand {
            profile,
            qualification_sha256,
        }
        | RequiredExecution::QualifiedModel {
            profile,
            qualification_sha256,
        } => {
            let claim = payload.qualification_claim.as_ref();
            let matching_claim = claim.is_some_and(|q| {
                q.kind == bound.requirement.check_kind
                    && &q.profile == profile
                    && &q.pin_sha256 == qualification_sha256
            });
            if !matching_claim {
                issues.insert(PolicyIssue::QualificationMismatch);
            }
            let installed = input.qualifications.qualifications.iter().find(|q| {
                q.check_kind == bound.requirement.check_kind
                    && &q.profile == profile
                    && &q.proof_sha256 == qualification_sha256
            });
            if let Some(qualification) = installed {
                if !qualification
                    .authorized_issuer_ids
                    .contains(&payload.issuer_id)
                {
                    issues.insert(PolicyIssue::QualificationIssuerUnpermitted);
                }
                if let Some(now) = now {
                    if qualification.not_before_ms > payload.issued_at_ms
                        || qualification.not_before_ms > now
                    {
                        issues.insert(PolicyIssue::QualificationNotYetValid);
                    }
                    if now >= qualification.not_after_ms {
                        issues.insert(PolicyIssue::QualificationExpired);
                    }
                    if qualification.revoked_at_ms.is_some_and(|at| at <= now) {
                        issues.insert(PolicyIssue::QualificationRevoked);
                    }
                }
            } else {
                issues.insert(PolicyIssue::QualificationUnavailable);
            }
        }
    }
    if (!policy.required_dependencies.is_empty() || policy.require_complete_dependency_inventory)
        && payload.dependency_snapshot_sha256.as_deref() != Some(dependency_digest)
    {
        issues.insert(PolicyIssue::DependencySnapshotStale);
    }
    Ok(issues)
}

fn assess_dependencies(
    policy: &InstalledPolicy,
    input: &PolicyInputs<'_>,
    accepted: &[&VerificationReport],
    snapshot_digest: &str,
    now: Option<u64>,
    issues: &mut BTreeSet<PolicyIssue>,
) {
    let snapshot = input.dependencies;
    if policy.require_complete_dependency_inventory && !snapshot.inventory_complete {
        issues.insert(PolicyIssue::DependencyInventoryIncomplete);
    }
    if (!policy.required_dependencies.is_empty() || policy.require_complete_dependency_inventory)
        && now.is_none_or(|now| now < snapshot.observed_at_ms || now >= snapshot.expires_at_ms)
    {
        issues.insert(PolicyIssue::DependencySnapshotStale);
    }
    for required in &policy.required_dependencies {
        let context = input
            .context
            .dependencies
            .iter()
            .find(|d| d.identity == required.identity);
        let current = snapshot
            .bindings
            .iter()
            .find(|d| d.identity == required.identity);
        if current.is_none_or(|d| d.revision_sha256 != required.revision_sha256) {
            issues.insert(if current.is_none() {
                PolicyIssue::DependencyMissing
            } else {
                PolicyIssue::DependencyMismatch
            });
        }
        let Some(context) = context else {
            issues.insert(PolicyIssue::DependencyMissing);
            continue;
        };
        match &context.state {
            DependencyState::Unknown {} | DependencyState::Unavailable { .. } => {
                issues.insert(PolicyIssue::DependencyBasisInsufficient);
            }
            DependencyState::Resolved { revision }
            | DependencyState::Observed {
                revision: Some(revision),
                ..
            } if revision != &required.revision_sha256 => {
                issues.insert(PolicyIssue::DependencyMismatch);
            }
            DependencyState::Declared {} | DependencyState::Observed { revision: None, .. }
                if required.minimum_basis != DependencyBasis::Declared =>
            {
                issues.insert(PolicyIssue::DependencyBasisInsufficient);
            }
            _ => {}
        }
        if required.minimum_basis == DependencyBasis::Observed {
            let mut retention_issues = BTreeSet::new();
            let observed = accepted.iter().any(|report| {
                let payload = report
                    .verified_receipt()
                    .expect("accepted receipt")
                    .payload();
                payload.dependency_snapshot_sha256.as_deref() == Some(snapshot_digest)
                    && payload.dependencies.iter().any(|d| {
                        if d.identity != required.identity {
                            return false;
                        }
                        let DependencyState::Observed {
                            revision: Some(revision),
                            evidence_digest,
                        } = &d.state
                        else {
                            return false;
                        };
                        revision == &required.revision_sha256
                            && payload.artifacts.iter().any(|artifact| {
                                if artifact.sha256 != *evidence_digest {
                                    return false;
                                }
                                let actual = report
                                    .artifacts()
                                    .iter()
                                    .find(|actual| actual.artifact_id == artifact.artifact_id);
                                if actual.is_some_and(|a| a.status == ArtifactStatus::Expired) {
                                    retention_issues.insert(PolicyIssue::ArtifactExpired);
                                }
                                if actual.is_none_or(|a| {
                                    a.status != ArtifactStatus::Full || !a.commitment_matches
                                }) {
                                    return false;
                                }
                                if let Some(issue) = artifact_retention_issue(
                                    artifact.expires_at_ms,
                                    now,
                                    policy.minimum_retention_ms,
                                ) {
                                    retention_issues.insert(issue);
                                    return false;
                                }
                                true
                            })
                    })
            });
            if !observed {
                issues.extend(retention_issues);
                issues.insert(PolicyIssue::DependencyObservationUnavailable);
            }
        }
    }
}

fn artifact_retention_issue(
    expiry: Option<u64>,
    now: Option<u64>,
    minimum_ms: u64,
) -> Option<PolicyIssue> {
    let Some(now) = now else {
        return Some(PolicyIssue::ClockUncertain);
    };
    match expiry {
        None => Some(PolicyIssue::RetentionUnspecified),
        Some(expiry)
            if now
                .checked_add(minimum_ms)
                .is_none_or(|until| expiry < until) =>
        {
            Some(PolicyIssue::EvidenceRetentionTooShort)
        }
        _ => None,
    }
}

fn verdict(issues: &BTreeSet<PolicyIssue>) -> PolicyVerdict {
    if issues.is_empty() {
        return PolicyVerdict::Satisfied;
    }
    let unknown = |issue: &PolicyIssue| {
        matches!(
            issue,
            PolicyIssue::ClockUncertain
                | PolicyIssue::ContextIncomplete
                | PolicyIssue::AttemptInventoryIncomplete
                | PolicyIssue::MissingEvidence
                | PolicyIssue::RetentionUnspecified
                | PolicyIssue::QualificationUnavailable
                | PolicyIssue::ArtifactUnavailable
                | PolicyIssue::IndependenceUnknown
                | PolicyIssue::DependencyInventoryIncomplete
                | PolicyIssue::DependencySnapshotStale
                | PolicyIssue::DependencyMissing
                | PolicyIssue::DependencyBasisInsufficient
                | PolicyIssue::DependencyObservationUnavailable
        )
    };
    if issues.iter().all(unknown) {
        PolicyVerdict::Indeterminate
    } else {
        PolicyVerdict::NotSatisfied
    }
}

fn verification_facts_digest(reports: &[VerificationReport]) -> Result<String> {
    #[derive(Serialize)]
    struct Facts<'a> {
        envelope_sha256: &'a str,
        registry_sha256: &'a str,
        evaluated_at_ms: Option<u64>,
        integrity: u8,
        registry_binding: u8,
        issuer_trust: u8,
        freshness: u8,
        execution_claim: u8,
        artifacts: Vec<(&'a str, u8, bool)>,
    }
    let mut facts = Vec::with_capacity(reports.len());
    for report in reports {
        let integrity = match report.integrity() {
            IntegrityStatus::Verified => 1,
            IntegrityStatus::UnknownKey => 2,
            IntegrityStatus::InvalidPublicKey => 3,
            IntegrityStatus::NonCanonicalPublicKey => 4,
            IntegrityStatus::WeakPublicKey => 5,
            IntegrityStatus::InvalidSignature => 6,
        };
        let issuer_trust = match report.current_issuer_trust() {
            CurrentIssuerTrust::Trusted => 1,
            CurrentIssuerTrust::UnknownIssuer => 2,
            CurrentIssuerTrust::UnknownKey => 3,
            CurrentIssuerTrust::FutureIssued => 4,
            CurrentIssuerTrust::NotYetValid => 5,
            CurrentIssuerTrust::Expired => 6,
            CurrentIssuerTrust::Revoked => 7,
            CurrentIssuerTrust::ClockUncertain => 8,
        };
        let freshness = match report.freshness() {
            Freshness::Fresh => 1,
            Freshness::FutureIssued => 2,
            Freshness::Expired => 3,
            Freshness::ClockUncertain => 4,
        };
        let execution_claim = match report.execution_claim() {
            ExecutionClaimStatus::ControllerObservedAssertion => 1,
            ExecutionClaimStatus::WorkerAssertion => 2,
            ExecutionClaimStatus::Unqualified => 3,
            ExecutionClaimStatus::Fixture => 4,
        };
        let mut artifacts: Vec<_> = report
            .artifacts()
            .iter()
            .map(|a| {
                let status = match a.status {
                    ArtifactStatus::Full => 1,
                    ArtifactStatus::Partial => 2,
                    ArtifactStatus::Redacted => 3,
                    ArtifactStatus::Unavailable => 4,
                    ArtifactStatus::Expired => 5,
                    ArtifactStatus::HashMismatch => 6,
                };
                (a.artifact_id.as_str(), status, a.commitment_matches)
            })
            .collect();
        artifacts.sort_by(|a, b| a.0.cmp(b.0));
        facts.push(Facts {
            envelope_sha256: report.envelope_sha256(),
            registry_sha256: report.evaluated_registry_sha256(),
            evaluated_at_ms: match report.evaluated_clock() {
                EvaluationClock::Known { unix_ms } => Some(unix_ms),
                EvaluationClock::Uncertain => None,
            },
            integrity,
            registry_binding: if report.registry_binding() == RegistryBindingStatus::Current {
                1
            } else {
                2
            },
            issuer_trust,
            freshness,
            execution_claim,
            artifacts,
        });
    }
    facts.sort_by(|a, b| a.envelope_sha256.cmp(b.envelope_sha256));
    // This is the bounded semantic verification projection, not a hash of
    // discarded corrupt artifact bytes. Valid full artifacts bind their bytes.
    domain_hash(
        b"agentscm.policy-verification-facts.v1\0",
        &bounded_json(&facts, 4 * 1024 * 1024)?,
    )
}
